//! The four calls every embedding runtime answers. The model itself lives in `digger-embed` (llama.cpp); the engine
//! only sees this.

use std::fmt;

/// What the embedding runtime reports about its model; part of the index fingerprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerInfo {
    pub model: String,
    pub revision: String,
    pub runtime: String,
    pub dtype: String,
    pub dim: usize,
    pub text_only: bool,
}

impl WorkerInfo {
    /// Vectors from different models, revisions, runtimes or precisions must never share an index.
    pub fn vector_source(&self) -> String {
        let revision: String = self.revision.chars().take(7).collect();
        format!("{}@{revision} {} {}", self.model, self.runtime, self.dtype)
    }
}

/// One embedding, or why the file couldn't be embedded.
#[derive(Debug, Clone)]
pub struct Embedding {
    pub vector: Option<Vec<f32>>,
    pub error: Option<String>,
}

/// The model failed as a whole (not one input): indexing stops, and pending files stay pending.
#[derive(Debug, Clone)]
pub enum WorkerError {
    NotFound(String),
    Failed(String),
}

impl fmt::Display for WorkerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorkerError::NotFound(why) => write!(f, "model not found: {why}"),
            WorkerError::Failed(why) => write!(f, "model error: {why}"),
        }
    }
}

impl std::error::Error for WorkerError {}

/// One thread at a time.
pub trait Embedder {
    fn info(&self) -> &WorkerInfo;
    fn embed_texts(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, WorkerError>;
    fn embed_images(&mut self, paths: &[String], budget: u32) -> Result<Vec<Embedding>, WorkerError>;
    fn embed_audio(&mut self, paths: &[String]) -> Result<Vec<Embedding>, WorkerError>;
}
