mod models;
mod utils;
mod cli;
mod decoders;
mod chunk_parser;
mod trials;
mod reduction;

use std::ffi::{CStr, CString};
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use pngstreamdec::{
    close_png, decode_scanlines, open_png, png_get_idat_size, png_set_count_idat_size,
    png_get_text_count, png_get_text_data, free_text_data
};
use pngstreamenc::{
    close_png_encode, encode_scanlines, open_png_encode, ZlibOptions,
};

use crate::models::TrialConfig;
use crate::cli::*;
use crate::utils::*;
use crate::decoders::*;
use crate::chunk_parser::*;
use crate::trials::*;
use crate::reduction::*;

// =========================================================================
// MAIN PROCESSING LOOP
// =========================================================================

fn main() {
    let cli = parse_args();

    if cli.show_help {
        print_usage();
        std::process::exit(0);
    }

    if cli.files.is_empty() && cli.external_input.is_none() {
        if !cli.quiet {
            eprintln!("optipng-rs: Error: No input files specified.\n");
            print_usage();
        }
        std::process::exit(1);
    }

    // Prepare list of file target tasks: (Input path, Output path, is_external)
    let mut tasks: Vec<(String, String, bool)> = Vec::new();

    if let Some(ref ext_in) = cli.external_input {
        let out_path = if let Some(ref out_f) = cli.out_file {
            out_f.clone()
        } else if !cli.files.is_empty() {
            cli.files[0].clone()
        } else {
            let p = PathBuf::from(ext_in);
            p.with_extension("png").to_string_lossy().to_string()
        };
        tasks.push((ext_in.clone(), out_path, true));
    } else {
        for f in &cli.files {
            tasks.push((f.clone(), f.clone(), false));
        }
    }

    let mut total_orig_bytes: u64 = 0;
    let mut total_new_bytes: u64 = 0;
    let mut total_processed_files: usize = 0;

    for (file_path, target_out_path, is_external) in tasks {
        if !cli.quiet {
            println!("Processing: {}", file_path);
        }

        let orig_file_size = fs::metadata(&file_path).map(|m| m.len()).unwrap_or(0);

        let mut width: u32 = 0;
        let mut height: u32 = 0;
        let mut bit_depth: u8 = 0;
        let mut color_type: u8 = 0;
        let stride: usize;
        let mut raw_pixels: Vec<u8>;
        let orig_idat_size: usize;

        // 1. Decode Image Input (External TGA/PNM or PNG FFI)
        if is_external {
            match load_external_image(&file_path) {
                Ok(img) => {
                    width = img.width;
                    height = img.height;
                    bit_depth = img.bit_depth;
                    color_type = img.color_type;
                    stride = img.stride;
                    raw_pixels = img.raw_pixels;
                    orig_idat_size = img.orig_idat_size;
                }
                Err(err_msg) => {
                    eprintln!("  (x) External format decoding error: {}", err_msg);
                    continue;
                }
            }
        } else {
            let c_file = CString::new(file_path.clone()).unwrap();
            let mut stride_usize = 0;

            let dec = open_png(
                c_file.as_ptr(),
                true, //Unindex enabled
                &mut width,
                &mut height,
                &mut bit_depth,
                &mut color_type,
                &mut stride_usize,
            );

            if dec.is_null() {
                eprintln!("  (x) Failed to decode PNG {}", file_path);
                continue;
            }

            // Check if file was already optimized by optipng-rs
            if !cli.force_trials {
                let count = png_get_text_count(dec);
                let mut already_optimized = false;

                for idx in 0..count {
                    let mut kw_ptr: *const std::os::raw::c_char = std::ptr::null();
                    let mut txt_ptr: *const std::os::raw::c_char = std::ptr::null();
                    if png_get_text_data(dec, idx, &mut kw_ptr, &mut txt_ptr) {
                        if !kw_ptr.is_null() {
                            let keyword = unsafe { CStr::from_ptr(kw_ptr) }.to_str().unwrap_or("");
                            if keyword == "optipng-rs" {
                                already_optimized = true;
                            }
                        }

                        // Free both CString allocations using lib.rs's internal allocator
                        free_text_data(kw_ptr as *mut _, txt_ptr as *mut _);

                        if already_optimized {
                            break;
                        }
                    }
                }

                if already_optimized {
                    if !cli.quiet {
                        println!("  (i) File is already optimized by optipng-rs. Skipping (use -ft and/or -force to re-process).");
                    }
                    close_png(dec);
                    continue;
                }
            }

            stride = stride_usize;
            png_set_count_idat_size(dec, true);

            if cli.nz {
                // -o0 / -nz fast path: Skip scanline decoding completely to save memory & CPU cycles
                raw_pixels = Vec::new();
                orig_idat_size = png_get_idat_size(dec);
                close_png(dec);
            } else {
                // Standard path: Decode scanlines into memory for color reduction and re-encoding trials
                let expected_size = stride * height as usize;
                raw_pixels = Vec::with_capacity(expected_size);

                loop {
                    let res = decode_scanlines(dec, 1024);
                    if res.size == 0 || res.data.is_null() {
                        break;
                    }
                    let chunk = unsafe { std::slice::from_raw_parts(res.data, res.size) };
                    raw_pixels.extend_from_slice(chunk);
                }
                orig_idat_size = png_get_idat_size(dec);
                close_png(dec);
            }
        }

        if !cli.quiet && !cli.nz {
            println!(
                "  Input Image ......... : {} x {} / {} bpc / {} / {} bpp",
                width,
                height,
                bit_depth,
                color_type_name(color_type),
                bit_depth * (match color_type { 0 | 3 => 1, 2 => 3, 4 => 2, 6 => 4, _ => 0 })
            );
        }

        // =========================================================================
        // STEP 2: OPTIPNG COLOR & BIT-DEPTH REDUCTION PIPELINE
        // =========================================================================

        let ReductionResult {
            out_color_type,
            out_bit_depth,
            final_palette,
            final_trns,
        } = reduce_image(
            &cli,
            width,
            height,
            color_type,
            bit_depth,
            stride,
            &mut raw_pixels,
        );

        // Prepare palette data to be thread safe
        let shared_palette = final_palette.map(Arc::new);
        let shared_trns = final_trns.map(Arc::new);

        if !cli.quiet && !cli.nz {
            println!(
                "  Image loaded ........ : {} bytes ({}) in memory.",
                raw_pixels.len(),
                format_bytes(raw_pixels.len())
            );
        }

        let mut image_data = Some(Arc::new(raw_pixels));

        // --- Generate trials for this image using heuristics ---
        let (zc_list, zm_list, zs_list, f_list) = if cli.zi == 2 {
            // Validate Zöpfli rules: -zc range/multiple values not allowed
            if let Some(ref user_zc) = cli.zc {
                if user_zc.len() > 1 {
                    eprintln!("Error: Range/multiple values for -zc are not supported when -zi2 (Zöpfli) is enabled. Please specify only one -zc level.");
                    std::process::exit(1);
                }
            }

            let (def_zc, def_f) = get_zopfli_opt_combinations(cli.opt_level);
            let zc = cli.zc.clone().unwrap_or(def_zc);
            let f = cli.f.clone().unwrap_or(def_f);

            // -zm and -zs are ignored when Zöpfli is enabled
            (zc, vec![8], vec![0], f)
        } else {
            let (def_zc, def_zm, def_zs, def_f) = get_opt_combinations(cli.opt_level, out_color_type, out_bit_depth);
            (
                cli.zc.clone().unwrap_or(def_zc),
                cli.zm.clone().unwrap_or(def_zm),
                cli.zs.clone().unwrap_or(def_zs),
                cli.f.clone().unwrap_or(def_f),
            )
        };

        let (global_best_config, global_best_size, global_best_bytes, total_trials) = if cli.nz {
            // Fast Path: 0 trials. Bypass worker threads completely.
            if !cli.quiet {
                println!("  (-nz) IDAT recoding disabled. Skipping trials...");
            }
            (
                Some(TrialConfig {
                    zc: zc_list[0],
                    zm: zm_list[0],
                    zs: zs_list[0],
                    f: f_list[0],
                }),
                orig_idat_size,
                None,
                0,
            )
        } else {
            let (best_config, best_size, best_bytes, num_trials) = run_parallel_trials(
                &cli,
                image_data.as_ref().unwrap(),
                &shared_palette,
                &shared_trns,
                width,
                height,
                out_bit_depth,
                out_color_type,
                &zc_list,
                &zm_list,
                &zs_list,
                &f_list,
            );

            if cli.zi == 2 {
                image_data = None;
            }

            (best_config, best_size, best_bytes, num_trials)
        };

        // 4. Final Output Write
        if let Some(best) = global_best_config {
            if !cli.quiet {
                if total_trials > 1 && cli.zi == 1 {
                    println!(
                        "  Best parameters ..... : -zc{} -zm{} -zs{} -f{}",
                        best.zc, best.zm, best.zs, best.f,
                    );
                } else if !cli.nz && cli.zi == 1 {
                    println!(
                        "  Used parameters ..... : -zc{} -zm{} -zs{} -f{}",
                        best.zc, best.zm, best.zs, best.f,
                    );
                } else if total_trials > 1 && cli.zi == 2 {
                    println!(
                        "  Best parameters ..... : -zc{} -f{}",
                        best.zc, best.f,
                    );
                } else if !cli.nz && cli.zi == 2 {
                    println!(
                        "  Used parameters ..... : -zc{} -f{}",
                        best.zc, best.f,
                    );
                }

                if !cli.nz {
                    println!(
                        "  New image data size . : {} bytes ({})\n  vs original ......... : {} bytes ({})",
                        global_best_size,
                        format_bytes(global_best_size),
                        orig_idat_size,
                        format_bytes(orig_idat_size),
                    );
                }
            }

            // Skip file modification if trial result is larger/equal
            if !cli.force_reenc && !is_external && global_best_size >= orig_idat_size && !cli.nz {
                if !cli.quiet {
                    println!("  /!\\ No compression improvement over source IDAT size. Skipping file write.");
                }
                continue;
            }

            if !cli.simulate {
                let start_encode = Instant::now();
                let original_path = PathBuf::from(&file_path);
                let out_path = if let Some(ref out_f) = cli.out_file {
                    PathBuf::from(out_f)
                } else if let Some(ref out_d) = cli.out_dir {
                    let mut pb = PathBuf::from(out_d);
                    pb.push(PathBuf::from(&target_out_path).file_name().unwrap());
                    pb
                } else {
                    PathBuf::from(&target_out_path)
                };

                let orig_metadata = fs::metadata(&original_path).ok();
                let is_in_place = out_path == original_path;
                let old_path = PathBuf::from(format!("{}.bak.{}", file_path, std::process::id()));

                if is_in_place {
                    if let Err(e) = fs::rename(&original_path, &old_path) {
                        eprintln!("  (x) Failed to rename original file to {:?}: {}", old_path, e);
                        continue;
                    }
                }

                let input_source = if is_in_place { &old_path } else { &original_path };
                let mut success = false;

                // Construct opt_info metadata before writing branches
                let opt_info = if cli.opt_level == 0 {
                    "-o0".to_string()
                } else if cli.cmd_options.is_empty() {
                    format!("{}{}{}{}", best.zc, best.zm, best.zs, best.f)
                } else {
                    format!("{}\n{}{}{}{}", cli.cmd_options, best.zc, best.zm, best.zs, best.f)
                };

                if let Some(winning_bytes) = global_best_bytes {
                    // Direct Write Shortcut (-zi2 / Zöpfli): Write pre-compressed winning PNG directly
                    match fs::write(&out_path, &winning_bytes) {
                        Ok(_) => {
                            success = out_path.exists();
                        }
                        Err(e) => {
                            eprintln!("  (x) Failed to write optimal PNG buffer to disk: {}", e);
                        }
                    }
                } else if cli.nz && !is_external {
                    // Fast Path (-o0 / -nz): Direct raw IDAT stream copy
                    match copy_png_idat_and_add_text(input_source, &out_path, "optipng-rs", &opt_info) {
                        Ok(_) => {
                            success = out_path.exists();
                        }
                        Err(e) => {
                            eprintln!("  (x) Failed to copy IDAT and write text chunk: {}", e);
                        }
                    }
                } else if let Some(ref img_data) = image_data {
                    // Standard Path (zlib): Full zlib re-encoding stream
                    let c_out_path = CString::new(out_path.to_string_lossy().into_owned()).unwrap();
                    let final_options = ZlibOptions {
                        z_implementation: cli.zi,
                        level: best.zc,
                        strategy: best.zs,
                        window_bits: 15,
                        mem_level: best.zm,
                        max_idat_size: 0,
                        expected_idat_size: global_best_size,
                    };

                    let (pal_ptr, pal_len) = match &shared_palette {
                        Some(pal) => (pal.as_ptr(), pal.len()),
                        None => (std::ptr::null(), 0),
                    };

                    let (trns_ptr, trns_len) = match &shared_trns {
                        Some(trns) => (trns.as_ptr(), trns.len()),
                        None => (std::ptr::null(), 0),
                    };

                    let c_key = CString::new("optipng-rs").unwrap();
                    let c_val = CString::new(opt_info).unwrap();
                    let text_keys = [c_key.as_ptr()];
                    let text_vals = [c_val.as_ptr()];

                    let enc = open_png_encode(
                        c_out_path.as_ptr(),
                        width,
                        height,
                        out_bit_depth,
                        out_color_type,
                        best.f,
                        pal_ptr,
                        pal_len,
                        trns_ptr,
                        trns_len,
                        text_keys.as_ptr(),
                        text_vals.as_ptr(),
                        1,
                        final_options,
                    );

                    if !enc.is_null() {
                        let total_rows = height as usize;
                        let row_bytes = if total_rows > 0 { img_data.len() / total_rows } else { 0 };
                        let mut encoded_rows = 0usize;
                        let chunk_rows = 250;

                        while encoded_rows < total_rows {
                            let rows_to_encode = (total_rows - encoded_rows).min(chunk_rows);
                            let offset = encoded_rows * row_bytes;
                            let ptr = unsafe { img_data.as_ptr().add(offset) };

                            encode_scanlines(enc, ptr, rows_to_encode as u32);
                            encoded_rows += rows_to_encode;

                            if !cli.quiet {
                                let pct = (encoded_rows as f64 / total_rows as f64) * 100.0;
                                print!("\r  Final encoding ...... : {:.2} percent done", pct);
                                let _ = io::stdout().flush();
                            }
                        }

                        close_png_encode(enc);
                        success = out_path.exists();
                    }
                }

                if success {
                    let encode_duration = start_encode.elapsed();
                    if !cli.quiet {
                        println!("\r  Final processing took : {}               ", format_duration(encode_duration));
                    }

                    preserve_file_times(&out_path, orig_metadata.as_ref());
                    let actual_size = fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
                    let saved_file_bytes = orig_file_size.saturating_sub(actual_size);
                    let pct_saved = if orig_file_size > 0 {
                        (saved_file_bytes as f64 / orig_file_size as f64) * 100.0
                    } else {
                        0.0
                    };

                    if !cli.quiet {
                        println!("  Resulting file size . : {} bytes ({})", actual_size, format_bytes(actual_size as usize));
                        println!(
                            "  Output size decrease  : {} bytes ({}) ({:.2}%)",
                            saved_file_bytes,
                            format_bytes(saved_file_bytes as usize),
                            pct_saved
                        );
                    }

                    total_orig_bytes += orig_file_size;
                    total_new_bytes += actual_size;
                    total_processed_files += 1;

                    if is_in_place && !cli.backup {
                        let _ = fs::remove_file(&old_path);
                    }
                } else {
                    eprintln!("  (x) Failed to write optimal stream to {:?}", out_path);
                    if is_in_place {
                        let _ = fs::rename(&old_path, &original_path);
                    }
                }
            }
        }
    }

    if !cli.quiet && total_processed_files > 0 {
        let total_saved_bytes = total_orig_bytes.saturating_sub(total_new_bytes);
        let total_pct_saved = if total_orig_bytes > 0 {
            (total_saved_bytes as f64 / total_orig_bytes as f64) * 100.0
        } else {
            0.0
        };
        println!("SUMMARY OF PROCESSED FILES");
        println!("  Files processed ..... : {}", total_processed_files);
        println!("  Total original size . : {} bytes ({})", total_orig_bytes, format_bytes(total_orig_bytes as usize));
        println!("  Total new size ...... : {} bytes ({})", total_new_bytes, format_bytes(total_new_bytes as usize));
        println!(
            "  Total size decrease . : {} bytes ({}) ({:.2}%)",
            total_saved_bytes,
            format_bytes(total_saved_bytes as usize),
            total_pct_saved
        );
    }
}