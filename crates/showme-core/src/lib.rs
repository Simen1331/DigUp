//! ShowMe's engine: which files to read, how to read them, the index, and search. The model is behind `Embedder`.

pub mod code;
pub mod crawler;
pub mod embedder;
pub mod estimator;
pub mod extract;
pub mod folder_selection;
pub mod images;
pub mod indexer;
pub mod media;
pub mod model;
pub mod paths;
pub mod pdf;
pub mod proto;
pub mod rules;
pub mod search_text;
pub mod searcher;
pub mod settings;
pub mod store;
pub mod vectors;
