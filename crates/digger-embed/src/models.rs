//! The EmbeddingGemma 2 files Digger runs: ggml-org's Q8_0 GGUF conversion at one pinned revision of the repo (made
//! from google/embeddinggemma-2 at 914f7f8). Never the F16 files: the model's activations overflow float16. The same
//! files as DigUp's, so the two make the same vectors.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use digger_core::embedder::WorkerError;
use sha2::{Digest, Sha256};

pub const REPOSITORY: &str = "ggml-org/embeddinggemma-2-GGUF";
pub const REVISION: &str = "bfcd298762cc34d0357ece5ebdd31791a3a374d8";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelFile {
    pub name: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
}

impl ModelFile {
    /// Hugging Face's address for this file at the pinned revision.
    pub fn url(&self) -> String {
        format!("https://huggingface.co/{REPOSITORY}/resolve/{REVISION}/{}", self.name)
    }
}

/// The text model (310 MB): all that queries need.
pub const TEXT: ModelFile = ModelFile {
    name: "embeddinggemma-2-Q8_0.gguf",
    bytes: 309_855_456,
    sha256: "2188ac1deca4b77dffefd603c2776a9d76d9d74ec01841392982ebb840b09135",
};
/// The image and audio encoders (555 MB).
pub const MEDIA: ModelFile = ModelFile {
    name: "mmproj-embeddinggemma-2-Q8_0.gguf",
    bytes: 554_821_024,
    sha256: "c4a8a52691ecef40618438928bdf9e68379b854e24166f292592353db0aab64f",
};
/// In download order.
pub const ALL: [ModelFile; 2] = [TEXT, MEDIA];

/// Where the files are: the text part, and the encoders once they're there too.
#[derive(Debug, Clone)]
pub struct LlamaModels {
    pub text: PathBuf,
    pub media: Option<PathBuf>,
}

/// Digger's data folder: `$XDG_DATA_HOME/digger`, or ~/.local/share/digger.
pub fn data_folder() -> PathBuf {
    match std::env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
        Some(data) => PathBuf::from(data).join("digger"),
        None => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share/digger"),
    }
}

/// Where Digger keeps the model.
pub fn installed() -> PathBuf {
    data_folder().join("models")
}

/// The pinned revision's folder in the Hugging Face cache (`hf download` puts the files there).
pub fn hugging_face_snapshot() -> PathBuf {
    let hub = std::env::var_os("HF_HUB_CACHE")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HF_HOME").map(|home| PathBuf::from(home).join("hub")))
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache/huggingface/hub"));
    hub.join(format!("models--{}", REPOSITORY.replace('/', "--")))
        .join("snapshots")
        .join(REVISION)
}

/// The folders to look in, first match wins. `$DIGGER_MODELS` is the only place looked at when it's set.
pub fn folders() -> Vec<PathBuf> {
    match std::env::var_os("DIGGER_MODELS").filter(|value| !value.is_empty()) {
        Some(chosen) => vec![PathBuf::from(chosen)],
        None => vec![installed(), hugging_face_snapshot()],
    }
}

impl LlamaModels {
    /// The first folder with the text part.
    pub fn locate() -> Result<LlamaModels, WorkerError> {
        for folder in folders() {
            let text = folder.join(TEXT.name);
            if !text.exists() {
                continue;
            }
            let media = folder.join(MEDIA.name);
            return Ok(LlamaModels {
                text,
                media: media.exists().then_some(media),
            });
        }
        let looked: Vec<String> = folders().iter().map(|folder| folder.display().to_string()).collect();
        Err(WorkerError::NotFound(format!(
            "no {} in {}; get it with: digger download",
            TEXT.name,
            looked.join(", ")
        )))
    }

    /// Both files are there, so indexing can run (checked without loading anything).
    pub fn complete() -> bool {
        Self::locate().is_ok_and(|models| models.media.is_some())
    }
}

/// The files that aren't in `folder` yet. Only verified files get their final name, so being there is enough.
pub fn missing(folder: &Path) -> Vec<ModelFile> {
    ALL.into_iter()
        .filter(|file| !folder.join(file.name).exists())
        .collect()
}

/// The SHA-256 of a file as lowercase hex.
pub fn sha256(path: &Path) -> std::io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 8 << 20];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Downloads the missing files into `folder`, going on from a `.part` left by an interrupted download, and checks
/// each against its SHA-256 before giving it its name. `progress` hears (file, bytes so far, its size).
pub fn download(folder: &Path, progress: &mut dyn FnMut(&ModelFile, u64, u64)) -> Result<(), String> {
    fs::create_dir_all(folder).map_err(|error| error.to_string())?;
    for file in missing(folder) {
        let target = folder.join(file.name);
        let part = folder.join(format!("{}.part", file.name));
        // A copy in the Hugging Face cache needs no download.
        let cached = hugging_face_snapshot().join(file.name);
        if fs::metadata(&cached).is_ok_and(|meta| meta.len() == file.bytes)
            && sha256(&cached).is_ok_and(|sum| sum == file.sha256)
        {
            fs::copy(&cached, &part).map_err(|error| error.to_string())?;
            fs::rename(&part, &target).map_err(|error| error.to_string())?;
            continue;
        }
        let mut have = fs::metadata(&part).map(|meta| meta.len()).unwrap_or(0);
        if have > file.bytes {
            fs::remove_file(&part).map_err(|error| error.to_string())?;
            have = 0;
        }
        if have < file.bytes {
            let mut request = ureq::get(&file.url());
            if have > 0 {
                request = request.header("Range", &format!("bytes={have}-"));
            }
            let response = request.call().map_err(|error| format!("{}: {error}", file.name))?;
            if have > 0 && response.status() != 206 {
                have = 0; // the server sent all of it
            }
            let mut out = fs::OpenOptions::new()
                .create(true)
                .write(true)
                .append(have > 0)
                .truncate(have == 0)
                .open(&part)
                .map_err(|error| error.to_string())?;
            let mut body = response.into_body().into_reader();
            let mut buffer = vec![0u8; 1 << 20];
            loop {
                let read = body
                    .read(&mut buffer)
                    .map_err(|error| format!("{}: {error}", file.name))?;
                if read == 0 {
                    break;
                }
                out.write_all(&buffer[..read]).map_err(|error| error.to_string())?;
                have += read as u64;
                progress(&file, have, file.bytes);
            }
        }
        let sum = sha256(&part).map_err(|error| error.to_string())?;
        if sum != file.sha256 {
            let _ = fs::remove_file(&part);
            return Err(format!(
                "{} didn't arrive whole (its checksum is {sum}); run it again",
                file.name
            ));
        }
        fs::rename(&part, &target).map_err(|error| error.to_string())?;
    }
    Ok(())
}
