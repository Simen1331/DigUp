//! Digger's command-line tool. For now it reports what llama.cpp was built with and which devices it can use.

use std::ffi::CStr;

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("info") => info(),
        _ => {
            eprintln!("usage: digger info");
            std::process::exit(2);
        }
    }
}

fn info() {
    // SAFETY: plain calls into llama.cpp; the strings it returns are static or live until llama_backend_free.
    unsafe {
        llama_sys::llama_backend_init();
        let system = CStr::from_ptr(llama_sys::llama_print_system_info()).to_string_lossy();
        println!("llama.cpp: {system}");
        for index in 0..llama_sys::ggml_backend_dev_count() {
            let device = llama_sys::ggml_backend_dev_get(index);
            let name = CStr::from_ptr(llama_sys::ggml_backend_dev_name(device)).to_string_lossy();
            let description = CStr::from_ptr(llama_sys::ggml_backend_dev_description(device)).to_string_lossy();
            let (mut free, mut total) = (0usize, 0usize);
            llama_sys::ggml_backend_dev_memory(device, &mut free, &mut total);
            println!(
                "device {index}: {name} ({description}), {} MiB free of {} MiB",
                free >> 20,
                total >> 20
            );
        }
        llama_sys::llama_backend_free();
    }
}
