//! Minimal in-memory optimization test harness.
//!
//! Usage: optipng-testing <input.png> [output.png]
//!   - If output path is given, writes the optimized bytes there.
//!   - Prints size comparison.

use std::ffi::CString;
use std::fs;

use pngstreamdec::{
    close_png, decode_scanlines, open_png, png_get_idat_size, png_set_count_idat_size,
};
use liboptipng_rs::{OptimizationTask, LoadedImage, optimize_memory};

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        eprintln!("Usage: {} <input.png> [output.png]", args[0]);
        std::process::exit(1);
    }

    let input_path = &args[1];
    let output_path = args.get(2).cloned();

    // ── Decode the PNG into raw pixels ────────────────────────────────────
    let c_path = CString::new(input_path.as_str()).expect("invalid path");

    let mut width = 0u32;
    let mut height = 0u32;
    let mut bit_depth = 0u8;
    let mut color_type = 0u8;
    let mut stride = 0usize;

    let dec = open_png(
        c_path.as_ptr(),
        true,               // unindex palette → RGBA
        &mut width,
        &mut height,
        &mut bit_depth,
        &mut color_type,
        &mut stride,
    );

    if dec.is_null() {
        eprintln!("Failed to open {}", input_path);
        std::process::exit(1);
    }

    png_set_count_idat_size(dec, true);

    let expected_size = stride * height as usize;
    let mut pixels = Vec::with_capacity(expected_size);

    loop {
        let res = decode_scanlines(dec, 1024);
        if res.size == 0 || res.data.is_null() {
            break;
        }
        let chunk = unsafe { std::slice::from_raw_parts(res.data, res.size) };
        pixels.extend_from_slice(chunk);
    }

    let orig_idat_size = png_get_idat_size(dec);
    close_png(dec);

    println!("Loaded: {}x{}  bit_depth={}  color_type={}  stride={}",
             width, height, bit_depth, color_type, stride);
    println!("Pixels: {} bytes  |  Orig IDAT: {} bytes",
             pixels.len(), orig_idat_size);

    // ── Build an OptimizationTask with the in-memory source ──────────────
    let loaded = LoadedImage {
        width,
        height,
        bit_depth,
        color_type,
        stride,
        raw_pixels: pixels,
        orig_idat_size,
    };

    let mut task = OptimizationTask::default();
    task.opt_level = 7;
    task.in_memory_source = Some(loaded);
    task.reduce_format = true;   // allow format reduction
    task.mt = 32;

    // Progress callback
    task.progress_callback = Some(Box::new(|completed, total| {
        let pct = if total > 0 { completed * 100 / total } else { 0 };
        eprint!("\r    progress: {:>3}%\x1b[K", pct);
        if completed >= total { eprintln!(); }
    }));

    // ── Run optimization ─────────────────────────────────────────────────
    let result = optimize_memory(task)
        .unwrap_or_else(|e| {
            eprintln!("Optimization failed: {}", e);
            std::process::exit(1);
        });

    let saved = result.orig_idat_size.saturating_sub(result.best_idat_size);
    let pct = if result.orig_idat_size > 0 {
        (saved as f64 / result.orig_idat_size as f64) * 100.0
    } else {
        0.0
    };

    println!("\n=== Result ===");
    println!("Optimized IDAT: {} bytes", result.best_idat_size);
    println!("Savings:        {} bytes ({:.1}%)", saved, pct);
    println!("Full stream:    {} bytes", result.optimized_bytes.len());
    println!("Winner:         {:?}", result.best_config);

    // ── Optionally write output ──────────────────────────────────────────
    if let Some(out) = &output_path {
        fs::write(out, &result.optimized_bytes).expect("failed to write output");
        println!("\nWrote → {}", out);
    }
}
