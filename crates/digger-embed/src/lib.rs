//! EmbeddingGemma 2 on llama.cpp: the files it needs, and the embedder that runs it.

pub mod image_sizing;
pub mod llama;
pub mod models;

pub use llama::LlamaEmbedder;
pub use models::LlamaModels;
