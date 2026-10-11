//! Digger's engine: which files to read, how to read them, the index, and search. The model is behind `Embedder`.

pub mod code;
pub mod crawler;
pub mod embedder;
pub mod extract;
pub mod folder_selection;
pub mod indexer;
pub mod model;
pub mod rules;
pub mod search_text;
pub mod searcher;
pub mod store;
pub mod vectors;
