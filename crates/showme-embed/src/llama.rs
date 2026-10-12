//! EmbeddingGemma 2 on llama.cpp, in this process: the image and audio encoders (mtmd) feed the text model, and its
//! mean-pooled, L2-normalized output is the vector.
//!
//! Inputs are framed as the reference (mlx-vlm, transformers) frames them: `<bos> text <eos>`,
//! `<bos> <|image> soft tokens <image|> <eos>` and the same for audio. The model is non-causal with no KV cache, so
//! each input, token ids and encoder rows together, goes through one `llama_process` call. Audio past 30 s is
//! dropped, as in the reference.

use std::ffi::{CStr, CString, c_char, c_void};
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::ptr;
use std::sync::{Mutex, Once};
use std::time::Instant;

use llama_sys as sys;
use showme_core::embedder::{Embedder, Embedding, WorkerError, WorkerInfo};
use showme_core::model::prompts;

use crate::image_sizing;
use crate::models::{self, LlamaModels};

pub const BUDGETS: [u32; 5] = [70, 140, 280, 560, 1120];

/// Which vectors this pipeline makes (the model files, llama.cpp, the resizing here, the prompts). It changes only
/// when the vectors do: every llama.cpp update must reproduce the reference vectors
/// (crates/showme-embed/tests/reference), or come with a new version here (then every index embeds again). DigUp's
/// llama.cpp b11461 on Metal made version 1.
pub const VECTOR_VERSION: u32 = 1;

/// What every LlamaEmbedder reports (its `vector_source` goes into index fingerprints), known without loading it.
pub fn model_info(text_only: bool) -> WorkerInfo {
    WorkerInfo {
        model: "google/embeddinggemma-2".into(),
        revision: models::REVISION.into(),
        runtime: format!("llama.cpp-vectors{VECTOR_VERSION}"),
        dtype: "text-q8_0+mmproj-q8_0".into(),
        dim: 768,
        text_only,
    }
}

const MAX_SEQUENCES: i32 = 16;

pub struct LlamaEmbedder {
    info: WorkerInfo,
    model: *mut sys::llama_model,
    context: *mut sys::llama_context,
    media: *mut sys::mtmd_context,
    batch: *mut sys::llama_batch_ext,
    vocab: *const sys::llama_vocab,
    /// 768: the output projection.
    embedding_size: usize,
    /// The width of encoder rows fed to the text model.
    row_size: usize,
    max_tokens: usize,
    /// Inputs and tokens in `batch`, and the next token's position in its input.
    sequences: i32,
    tokens: usize,
    position: sys::llama_pos,
    pub load_seconds: f64,
}

// The pointers are only used from the thread that owns the embedder.
unsafe impl Send for LlamaEmbedder {}

fn failed(why: impl Into<String>) -> WorkerError {
    WorkerError::Failed(why.into())
}

fn c_path(path: &Path) -> Result<CString, WorkerError> {
    CString::new(path.as_os_str().as_encoded_bytes()).map_err(|_| failed(format!("bad path {}", path.display())))
}

impl LlamaEmbedder {
    /// `text_only`: just the text model (queries). `log`: where llama.cpp's warnings and errors go (stderr when None).
    pub fn new(models: &LlamaModels, text_only: bool, log: Option<&Path>) -> Result<LlamaEmbedder, WorkerError> {
        let started = Instant::now();
        log_to(log);
        init_backend();
        if !text_only && models.media.is_none() {
            return Err(WorkerError::NotFound(format!(
                "{} (images and audio) isn't downloaded yet",
                models::MEDIA.name
            )));
        }
        let text_path = c_path(&models.text)?;
        // SAFETY: plain llama.cpp calls; every pointer made here is freed in `drop`, or right away on failure.
        unsafe {
            let mut model_params = sys::llama_model_default_params();
            model_params.n_gpu_layers = 999;
            let model = sys::llama_model_load_from_file(text_path.as_ptr(), model_params);
            if model.is_null() {
                return Err(failed(format!("can't load {}", models.text.display())));
            }
            // One input must fit one micro-batch: 2048 tokens (the reference's cut), which also holds a 1120-token
            // image or a 750-token 30 s clip. Queries are short, so the query encoder reserves less and cuts at 512.
            let max_tokens: u32 = if text_only { 512 } else { 2048 };
            let mut params = sys::llama_context_default_params();
            params.n_ctx = max_tokens;
            params.n_batch = max_tokens;
            params.n_ubatch = max_tokens;
            params.n_seq_max = MAX_SEQUENCES as u32;
            params.kv_unified = true; // each input may use the whole context, as in llama-embedding
            params.embeddings = true;
            params.pooling_type = sys::LLAMA_POOLING_TYPE_MEAN;
            let context = sys::llama_init_from_model(model, params);
            if context.is_null() {
                sys::llama_model_free(model);
                return Err(failed("can't create a llama.cpp context"));
            }
            let mut media = ptr::null_mut();
            if let (Some(media_file), false) = (&models.media, text_only) {
                let mut media_params = sys::mtmd_context_params_default();
                media_params.use_gpu = true;
                media_params.print_timings = false;
                media_params.warmup = true;
                media_params.image_min_tokens = 1; // never rescale: images arrive at their final size
                media_params.image_max_tokens = 1120;
                media = sys::mtmd_init_from_file(c_path(media_file)?.as_ptr(), model, media_params);
                if media.is_null() {
                    sys::llama_free(context);
                    sys::llama_model_free(model);
                    return Err(failed(format!("can't load {}", media_file.display())));
                }
            }
            let mut embedder = LlamaEmbedder {
                info: model_info(text_only),
                model,
                context,
                media,
                batch: sys::llama_batch_ext_init(context),
                vocab: sys::llama_model_get_vocab(model),
                embedding_size: sys::llama_model_n_embd_out(model) as usize,
                row_size: sys::llama_model_n_embd_inp(model) as usize,
                max_tokens: max_tokens as usize,
                sequences: 0,
                tokens: 0,
                position: 0,
                load_seconds: 0.0,
            };
            embedder.info.dim = embedder.embedding_size;
            // The first calls build the GPU pipelines: pay that now.
            for _ in 0..2 {
                embedder.embed_texts(&[prompts::query("warm up")])?;
            }
            embedder.load_seconds = (started.elapsed().as_secs_f64() * 100.0).round() / 100.0;
            log_line(&format!(
                "ready in {} s ({}, vectors {VECTOR_VERSION})",
                embedder.load_seconds,
                if text_only { "text-only" } else { "full" }
            ));
            Ok(embedder)
        }
    }

    /// `<bos> text <eos>`, cut to `max_tokens` the way transformers cuts (keeping both special tokens).
    fn tokenize(&self, text: &str) -> Vec<sys::llama_token> {
        let bytes = text.as_bytes();
        let mut tokens = vec![0 as sys::llama_token; bytes.len() + 8];
        // SAFETY: `tokens` has room for the count passed; a negative result says how much room is needed.
        let mut count = unsafe {
            sys::llama_tokenize(
                self.vocab,
                bytes.as_ptr() as *const c_char,
                bytes.len() as i32,
                tokens.as_mut_ptr(),
                tokens.len() as i32,
                true,
                true,
            )
        };
        if count < 0 {
            tokens = vec![0; (-count) as usize];
            count = unsafe {
                sys::llama_tokenize(
                    self.vocab,
                    bytes.as_ptr() as *const c_char,
                    bytes.len() as i32,
                    tokens.as_mut_ptr(),
                    tokens.len() as i32,
                    true,
                    true,
                )
            };
        }
        tokens.truncate(count.max(0) as usize);
        if tokens.len() > self.max_tokens {
            tokens.truncate(self.max_tokens - 1);
            tokens.push(unsafe { sys::llama_vocab_eos(self.vocab) });
        }
        tokens
    }

    /// Starts the next input in the batch (each input is its own sequence).
    fn begin(&mut self) {
        self.sequences += 1;
        self.position = 0;
    }

    /// Drops everything added so far, including an input that failed halfway.
    fn reset(&mut self) {
        unsafe { sys::llama_batch_ext_clear(self.batch) };
        self.sequences = 0;
        self.tokens = 0;
    }

    fn add_token(&mut self, token: sys::llama_token) -> Result<(), String> {
        let index = unsafe { sys::llama_batch_ext_add_token(self.batch, self.sequences - 1, token) };
        self.place(index)
    }

    /// `count` encoder rows, each `row_size` floats (copied by the batch).
    fn add_rows(&mut self, rows: *const f32, count: usize) -> Result<(), String> {
        for row in 0..count {
            let embd =
                sys::llama_embd { data: unsafe { rows.add(row * self.row_size) }, n_rows: 1, n_embd: self.row_size };
            let index = unsafe { sys::llama_batch_ext_add_embd(self.batch, self.sequences - 1, embd) };
            self.place(index)?;
        }
        Ok(())
    }

    fn place(&mut self, index: i32) -> Result<(), String> {
        if index < 0 {
            return Err(format!("batch full ({index})"));
        }
        let pos: [sys::llama_pos; 4] = [self.position, 0, 0, 0];
        let placed = unsafe {
            sys::llama_batch_ext_set_pos(self.batch, index, pos.as_ptr())
                && sys::llama_batch_ext_set_output_embd(self.batch, index, true)
        };
        if !placed {
            return Err(format!("can't place token {index}"));
        }
        self.position += 1;
        self.tokens += 1;
        Ok(())
    }

    /// Runs the text model over the batch and returns one L2-normalized vector per input, in order.
    fn run(&mut self) -> Result<Vec<Vec<f32>>, WorkerError> {
        let status = unsafe { sys::llama_process(self.context, sys::LLAMA_PROCESS_TYPE_ENCODE, self.batch) };
        let result = if status != 0 {
            Err(failed(format!("llama_process failed ({status})")))
        } else {
            (0..self.sequences)
                .map(|sequence| {
                    let pooled = unsafe { sys::llama_get_embeddings_seq(self.context, sequence) };
                    if pooled.is_null() {
                        return Err(failed(format!("no pooled embedding for input {sequence}")));
                    }
                    let pooled = unsafe { std::slice::from_raw_parts(pooled, self.embedding_size) };
                    let sum: f64 = pooled.iter().map(|value| *value as f64 * *value as f64).sum();
                    let scale = if sum > 0.0 { (1.0 / sum.sqrt()) as f32 } else { 0.0 };
                    Ok(pooled.iter().map(|value| value * scale).collect())
                })
                .collect()
        };
        self.reset();
        result
    }

    /// One file's vector, or why it has none (an unreadable file must not sink its neighbours).
    fn embed_media(&mut self, path: &str, image: bool, budget: u32) -> Result<Vec<f32>, String> {
        let bitmap = self.load(path, image, budget)?;
        // SAFETY: the bitmap and chunks are freed below whatever happens; `media` lives as long as `self`.
        unsafe {
            let chunks = sys::mtmd_input_chunks_init();
            let marker = sys::mtmd_default_marker();
            let text = sys::mtmd_input_text {
                text: marker,
                text_len: CStr::from_ptr(marker).to_bytes().len(),
                add_special: true,
                parse_special: true,
            };
            let bitmaps = [bitmap as *const sys::mtmd_bitmap];
            let status = sys::mtmd_tokenize(self.media, chunks, &text, bitmaps.as_ptr(), 1);
            let result = if status != 0 {
                Err(format!("can't tokenize {path} ({status})"))
            } else {
                self.reset();
                self.begin();
                let mut outcome = Ok(());
                for index in 0..sys::mtmd_input_chunks_size(chunks) {
                    let chunk = sys::mtmd_input_chunks_get(chunks, index);
                    outcome = if sys::mtmd_input_chunk_get_type(chunk) == sys::MTMD_INPUT_CHUNK_TYPE_TEXT {
                        let mut count = 0usize;
                        let tokens = sys::mtmd_input_chunk_get_tokens_text(chunk, &mut count);
                        if tokens.is_null() {
                            continue;
                        }
                        std::slice::from_raw_parts(tokens, count).iter().try_for_each(|token| self.add_token(*token))
                    } else if sys::mtmd_encode_chunk(self.media, chunk) != 0 {
                        Err(format!("the encoder failed on {path}"))
                    } else {
                        let rows = sys::mtmd_get_output_embd(self.media);
                        if rows.is_null() {
                            Err(format!("the encoder failed on {path}"))
                        } else {
                            self.add_rows(rows, sys::mtmd_input_chunk_get_n_tokens(chunk))
                        }
                    };
                    if outcome.is_err() {
                        break;
                    }
                }
                match outcome {
                    Ok(()) => self.run().map(|mut vectors| vectors.remove(0)).map_err(|error| error.to_string()),
                    Err(error) => {
                        self.reset();
                        Err(error)
                    }
                }
            };
            sys::mtmd_input_chunks_free(chunks);
            sys::mtmd_bitmap_free(bitmap);
            result
        }
    }

    /// Reads an image or audio file with mtmd's helpers (stb_image, miniaudio at 16 kHz), resizes images for `budget`
    /// and cuts audio at 30 s. The caller frees the bitmap.
    fn load(&self, path: &str, image: bool, budget: u32) -> Result<*mut sys::mtmd_bitmap, String> {
        let name = CString::new(path).map_err(|_| format!("bad path {path}"))?;
        // SAFETY: each bitmap made here is either returned (the caller frees it) or freed before returning.
        unsafe {
            let wrapper = sys::mtmd_helper_bitmap_init_from_file(
                self.media,
                name.as_ptr(),
                false,
                sys::mtmd_helper_init_opt_default(),
            );
            if !wrapper.video_ctx.is_null() {
                if !wrapper.bitmap.is_null() {
                    sys::mtmd_bitmap_free(wrapper.bitmap);
                }
                sys::mtmd_helper_video_free(wrapper.video_ctx);
                return Err(format!("video goes in as frames and sound, not as a file: {path}"));
            }
            let bitmap = wrapper.bitmap;
            if bitmap.is_null() {
                return Err(format!("can't read {path}"));
            }
            if sys::mtmd_bitmap_is_audio(bitmap) == image {
                sys::mtmd_bitmap_free(bitmap);
                return Err(format!("not {}: {path}", if image { "an image" } else { "audio" }));
            }
            if image {
                let (width, height) =
                    (sys::mtmd_bitmap_get_nx(bitmap) as usize, sys::mtmd_bitmap_get_ny(bitmap) as usize);
                let Some((to_height, to_width)) = image_sizing::target_size(height, width, budget as usize) else {
                    sys::mtmd_bitmap_free(bitmap);
                    return Err(format!("image too thin to embed: {path}"));
                };
                if (to_height, to_width) == (height, width) {
                    return Ok(bitmap);
                }
                let source = std::slice::from_raw_parts(sys::mtmd_bitmap_get_data(bitmap), width * height * 3);
                let pixels = image_sizing::resize_bicubic(source, width, height, to_width, to_height);
                sys::mtmd_bitmap_free(bitmap);
                let resized = sys::mtmd_bitmap_init(to_width as u32, to_height as u32, pixels.as_ptr());
                return if resized.is_null() { Err(format!("can't resize {path}")) } else { Ok(resized) };
            }
            let samples = sys::mtmd_bitmap_get_n_bytes(bitmap) / std::mem::size_of::<f32>();
            let limit = 30 * sys::mtmd_get_audio_sample_rate(self.media) as usize;
            if samples <= limit {
                return Ok(bitmap);
            }
            let prefix = sys::mtmd_bitmap_init_from_audio(limit, sys::mtmd_bitmap_get_data(bitmap) as *const f32);
            sys::mtmd_bitmap_free(bitmap);
            if prefix.is_null() { Err(format!("can't cut {path} at 30 s")) } else { Ok(prefix) }
        }
    }
}

impl Embedder for LlamaEmbedder {
    fn info(&self) -> &WorkerInfo {
        &self.info
    }

    fn embed_texts(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, WorkerError> {
        self.reset();
        let mut vectors = Vec::with_capacity(texts.len());
        for text in texts {
            let tokens = self.tokenize(text);
            if self.sequences == MAX_SEQUENCES || self.tokens + tokens.len() > self.max_tokens {
                vectors.extend(self.run()?);
            }
            self.begin();
            for token in tokens {
                self.add_token(token).map_err(failed)?;
            }
        }
        if self.sequences > 0 {
            vectors.extend(self.run()?);
        }
        Ok(vectors)
    }

    fn embed_images(&mut self, paths: &[String], budget: u32) -> Result<Vec<Embedding>, WorkerError> {
        if self.media.is_null() {
            return Err(failed("this embedder was started text-only"));
        }
        if !BUDGETS.contains(&budget) {
            return Err(failed(format!("budget must be one of {BUDGETS:?}")));
        }
        Ok(paths.iter().map(|path| embedding(self.embed_media(path, true, budget))).collect())
    }

    fn embed_audio(&mut self, paths: &[String]) -> Result<Vec<Embedding>, WorkerError> {
        if self.media.is_null() {
            return Err(failed("this embedder was started text-only"));
        }
        Ok(paths.iter().map(|path| embedding(self.embed_media(path, false, 0))).collect())
    }
}

fn embedding(result: Result<Vec<f32>, String>) -> Embedding {
    match result {
        Ok(vector) => Embedding { vector: Some(vector), error: None },
        Err(error) => Embedding { vector: None, error: Some(error.chars().take(300).collect()) },
    }
}

impl Drop for LlamaEmbedder {
    fn drop(&mut self) {
        // SAFETY: each pointer was made in `new` and is freed once.
        unsafe {
            if !self.batch.is_null() {
                sys::llama_batch_ext_free(self.batch);
            }
            if !self.media.is_null() {
                sys::mtmd_free(self.media);
            }
            if !self.context.is_null() {
                sys::llama_free(self.context);
            }
            if !self.model.is_null() {
                sys::llama_model_free(self.model);
            }
        }
    }
}

// llama.cpp's log is process-wide: warnings and errors go to one file (or stderr), the rest is dropped (it logs a lot
// at load time).

struct Log {
    file: Option<File>,
    /// CONT continues the previous message, at its level.
    level: sys::ggml_log_level,
}

static LOG: Mutex<Log> = Mutex::new(Log { file: None, level: 0 });

fn log_to(path: Option<&Path>) {
    let mut log = LOG.lock().unwrap();
    log.file = path.and_then(|path| std::fs::OpenOptions::new().create(true).append(true).open(path).ok());
}

fn log_line(message: &str) {
    write_log(&format!("[llama.cpp] {message}\n"));
}

fn write_log(text: &str) {
    let mut log = LOG.lock().unwrap();
    match log.file.as_mut() {
        Some(file) => {
            let _ = file.write_all(text.as_bytes());
        }
        None => eprint!("{text}"),
    }
}

unsafe extern "C" fn log_callback(level: sys::ggml_log_level, text: *const c_char, _: *mut c_void) {
    if text.is_null() {
        return;
    }
    let text = unsafe { CStr::from_ptr(text) }.to_string_lossy();
    let keep = {
        let mut log = LOG.lock().unwrap();
        if level != sys::GGML_LOG_LEVEL_CONT {
            log.level = level;
        }
        log.level >= sys::GGML_LOG_LEVEL_WARN
    };
    if keep {
        write_log(&text);
    }
}

/// llama.cpp's process-wide setup, once.
fn init_backend() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        sys::llama_log_set(Some(log_callback), ptr::null_mut());
        sys::mtmd_helper_log_set(Some(log_callback), ptr::null_mut());
        sys::llama_backend_init();
    });
}

/// What llama.cpp was built with and the devices it can use, one a line.
pub fn devices() -> Vec<String> {
    init_backend();
    let mut lines = Vec::new();
    // SAFETY: the strings llama.cpp returns here are static.
    unsafe {
        lines.push(format!("llama.cpp: {}", CStr::from_ptr(sys::llama_print_system_info()).to_string_lossy().trim()));
        for index in 0..sys::ggml_backend_dev_count() {
            let device = sys::ggml_backend_dev_get(index);
            let name = CStr::from_ptr(sys::ggml_backend_dev_name(device)).to_string_lossy();
            let description = CStr::from_ptr(sys::ggml_backend_dev_description(device)).to_string_lossy();
            let (mut free, mut total) = (0usize, 0usize);
            sys::ggml_backend_dev_memory(device, &mut free, &mut total);
            lines.push(format!(
                "device {index}: {name} ({description}), {} MiB free of {} MiB",
                free >> 20,
                total >> 20
            ));
        }
    }
    lines
}
