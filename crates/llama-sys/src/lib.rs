//! Raw bindings to llama.cpp and mtmd at the tag in scripts/build-llama.sh. The safe wrapper lives in digger-embed.
#![allow(non_upper_case_globals, non_camel_case_types, non_snake_case, clippy::all)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
