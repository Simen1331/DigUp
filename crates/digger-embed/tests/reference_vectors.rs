//! The vectors this build makes for fixed inputs, against the ones `VECTOR_VERSION` stands for: DigUp's
//! Tests/LlamaRuntimeTests/Reference/vectors.json, made by llama.cpp on a Mac's GPU. Indexes record that version, so
//! every llama.cpp update, and every backend (CPU, Vulkan, CUDA), must pass this. Needs the model files; skipped
//! without them (`digger download`).

use std::collections::BTreeMap;
use std::path::PathBuf;

use digger_core::embedder::Embedder;
use digger_core::model::prompts;
use digger_embed::llama::{LlamaEmbedder, VECTOR_VERSION};
use digger_embed::models::LlamaModels;

/// Each vector's cosine with its reference must stay at least this: runtimes that agree measure ≥ 0.999, and less is
/// a change people would see in their results.
const AGREEMENT: f32 = 0.999;

#[derive(serde::Deserialize)]
struct Reference {
    vector_version: u32,
    made_with: String,
    vectors: BTreeMap<String, Vec<f32>>,
}

fn folder() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../Tests/LlamaRuntimeTests/Reference")
}

#[test]
fn vectors_match_the_reference() {
    let Ok(models) = LlamaModels::locate() else {
        eprintln!("skipped: no model files");
        return;
    };
    if models.media.is_none() {
        eprintln!("skipped: no image and audio encoders");
        return;
    }
    let reference: Reference = serde_json::from_slice(&std::fs::read(folder().join("vectors.json")).unwrap()).unwrap();
    assert_eq!(reference.vector_version, VECTOR_VERSION);

    let long: Vec<String> = (1..=60)
        .map(|n| {
            format!(
                "Note {n}: the review moved to room {} because the projector in the main hall failed again, and the team agreed to send the slides the night before.",
                n % 7 + 2
            )
        })
        .collect();
    let texts = [
        ("query", prompts::query("payment declined error")),
        ("query-bn", prompts::query("ইলিশ মাছ রান্না")),
        ("query-ar", prompts::query("متى تغلق المكتبة")),
        (
            "document",
            prompts::document(
                Some("Tokyo itinerary"),
                "Flight to Tokyo Haneda on November 3. Then the train to Kamakura to see the Great Buddha, and back for dinner in Shibuya.",
            ),
        ),
        ("document-long", prompts::document(None, &long.join(" "))),
    ];
    let log = std::env::temp_dir().join("digger-reference-vectors.log");
    let mut embedder = LlamaEmbedder::new(&models, false, Some(&log)).unwrap();
    let mut made: BTreeMap<String, Vec<f32>> = BTreeMap::new();
    let inputs: Vec<String> = texts.iter().map(|(_, text)| text.clone()).collect();
    for ((name, _), vector) in texts.iter().zip(embedder.embed_texts(&inputs).unwrap()) {
        made.insert(name.to_string(), vector);
    }
    for (name, file, budget) in
        [("screen-280", "screen.png", 280), ("photo-140", "photo.jpg", 140), ("frame-70", "photo.jpg", 70)]
    {
        let path = folder().join(file).to_string_lossy().into_owned();
        let embedding = embedder.embed_images(&[path], budget).unwrap().remove(0);
        made.insert(name.into(), embedding.vector.unwrap_or_else(|| panic!("{file}: {:?}", embedding.error)));
    }
    let speech = folder().join("speech.wav").to_string_lossy().into_owned();
    let embedding = embedder.embed_audio(&[speech]).unwrap().remove(0);
    made.insert("speech".into(), embedding.vector.unwrap_or_else(|| panic!("speech.wav: {:?}", embedding.error)));

    assert_eq!(made.keys().collect::<Vec<_>>(), reference.vectors.keys().collect::<Vec<_>>());
    let mut worst = (String::new(), 1.0f32);
    for (name, vector) in &made {
        let cosine: f32 = vector.iter().zip(&reference.vectors[name]).map(|(a, b)| a * b).sum();
        println!("{name}: cosine {cosine:.5}");
        if cosine < worst.1 {
            worst = (name.clone(), cosine);
        }
    }
    assert!(
        worst.1 >= AGREEMENT,
        "{}: cosine {} with vectors {} ({}): this build makes other vectors",
        worst.0,
        worst.1,
        reference.vector_version,
        reference.made_with
    );
}
