use std::env;
use std::ffi::{c_void, CString};
use std::fs::{self, FileTimes, Metadata};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

use pngstreamdec::{
    close_png, decode_scanlines, open_png, png_get_idat_size, png_set_count_idat_size,
};
use pngstreamenc::{
    close_png_encode, close_png_encode_get_idat_size, encode_scanlines, open_png_encode,
    open_png_encode_stream, ZlibOptions,
};

// =========================================================================
// OPTIPNG ENGINE
// =========================================================================

#[derive(Debug, Clone)]
struct TrialConfig {
    zc: i32,
    zm: i32,
    zs: i32,
    f: u8,
}

struct CliArgs {
    files: Vec<String>,
    opt_level: u8,
    mt: usize,
    zc: Option<Vec<i32>>,
    zm: Option<Vec<i32>>,
    zs: Option<Vec<i32>>,
    f: Option<Vec<u8>>,
    backup: bool,
    simulate: bool,
    quiet: bool,
    nc: bool, // -nc flag to disable color type reduction
    out_file: Option<String>,
    out_dir: Option<String>,
}

/// Zero-allocation stream callback. Discards bytes and only counts the size.
unsafe extern "C" fn counter_write_cb(user_data: *mut c_void, _buf: *const u8, len: usize) -> usize {
    if !user_data.is_null() && !_buf.is_null() {
        let counter = unsafe { &mut *(user_data as *mut usize) };
        *counter += len;
    }
    len
}

/// Helper function to copy file modification and access times across all platforms supported by Rust
fn preserve_file_times(target_path: &Path, orig_metadata: Option<&Metadata>) {
    if let Some(meta) = orig_metadata {
        let mut times = FileTimes::new();
        let mut set_any = false;

        if let Ok(atime) = meta.accessed() {
            times = times.set_accessed(atime);
            set_any = true;
        }
        if let Ok(mtime) = meta.modified() {
            times = times.set_modified(mtime);
            set_any = true;
        }

        if set_any {
            if let Ok(file) = fs::OpenOptions::new().write(true).open(target_path) {
                let _ = file.set_times(times);
            }
        }
    }
}

/// Parses combinatorial strings like "0-3" or "0,5" into a flat i32 vector
fn parse_ranges_i32(input: &str) -> Vec<i32> {
    let mut result = Vec::new();
    for part in input.split(',') {
        if let Some((start_str, end_str)) = part.split_once('-') {
            if let (Ok(start), Ok(end)) = (start_str.parse::<i32>(), end_str.parse::<i32>()) {
                for val in start..=end {
                    result.push(val);
                }
            }
        } else if let Ok(val) = part.parse::<i32>() {
            result.push(val);
        }
    }
    result
}

/// Helper wrapper to parse u8 ranges safely
fn parse_ranges_u8(input: &str) -> Vec<u8> {
    parse_ranges_i32(input)
    .into_iter()
    .filter_map(|v| u8::try_from(v).ok())
    .collect()
}

/// Resolves OptiPNG heuristic combinations based on optimization level
fn get_opt_combinations(level: u8) -> (Vec<i32>, Vec<i32>, Vec<i32>, Vec<u8>) {
    match level {
        0 | 1 => (vec![9], vec![8], vec![0], vec![0]),
        2 => (vec![9], vec![8], vec![0, 1, 2, 3], vec![0, 5]),
        3 => (vec![9], vec![8, 9], vec![0, 1, 2, 3], vec![0, 5]),
        4 => (vec![9], vec![8], vec![0, 1, 2, 3], vec![0, 1, 2, 3, 4, 5]),
        5 => (vec![9], vec![8, 9], vec![0, 1, 2, 3], vec![0, 1, 2, 3, 4, 5]),
        6 => ((1..=9).collect(), vec![8], vec![0, 1, 2, 3], vec![0, 1, 2, 3, 4, 5]),
        7 => ((1..=9).collect(), vec![8, 9], vec![0, 1, 2, 3], vec![0, 1, 2, 3, 4, 5]),
        _ => get_opt_combinations(2),
    }
}

fn parse_args() -> CliArgs {
    let mut args = env::args().skip(1);
    let mut cli = CliArgs {
        files: Vec::new(),
        opt_level: 2,
        mt: 0,
        zc: None,
        zm: None,
        zs: None,
        f: None,
        backup: false,
        simulate: false,
        quiet: false,
        nc: false,
        out_file: None,
        out_dir: Option::None,
    };

    while let Some(arg) = args.next() {
        if arg.starts_with("-o") && arg.len() > 2 && arg[2..].chars().all(|c| c.is_ascii_digit()) {
            let level: u8 = arg[2..].parse().unwrap_or(2);
            cli.opt_level = level.min(7);
            continue;
        }

        match arg.as_str() {
            "-o" => {
                if let Some(val) = args.next() {
                    let level: u8 = val.parse().unwrap_or(2);
                    cli.opt_level = level.min(7);
                }
            }
            "-mt" => {
                if let Some(val) = args.next() {
                    cli.mt = val.parse().unwrap_or(0);
                }
            }
            "-zc" => if let Some(v) = args.next() {
                let mut parsed = parse_ranges_i32(&v);
                cli.zc.get_or_insert_with(Vec::new).append(&mut parsed);
            },
            "-zm" => if let Some(v) = args.next() {
                let mut parsed = parse_ranges_i32(&v);
                cli.zm.get_or_insert_with(Vec::new).append(&mut parsed);
            },
            "-zs" => if let Some(v) = args.next() {
                let mut parsed = parse_ranges_i32(&v);
                cli.zs.get_or_insert_with(Vec::new).append(&mut parsed);
            },
            "-f"  => if let Some(v) = args.next() {
                let mut parsed = parse_ranges_u8(&v);
                cli.f.get_or_insert_with(Vec::new).append(&mut parsed);
            },
            "-backup" | "-keep" => cli.backup = true,
            "-simulate" => cli.simulate = true,
            "-quiet" | "-silent" => cli.quiet = true,
            "-nc" => cli.nc = true,
            "-out" => cli.out_file = args.next(),
            "-dir" => cli.out_dir = args.next(),
            "--" => {
                cli.files.extend(args);
                break;
            }
            _ => {
                if !arg.starts_with('-') {
                    cli.files.push(arg);
                }
            }
        }
    }

    // Deduplicate custom parameter lists if provided
    if let Some(ref mut list) = cli.zc { list.sort_unstable(); list.dedup(); }
    if let Some(ref mut list) = cli.zm { list.sort_unstable(); list.dedup(); }
    if let Some(ref mut list) = cli.zs { list.sort_unstable(); list.dedup(); }
    if let Some(ref mut list) = cli.f  { list.sort_unstable(); list.dedup(); }

    if cli.mt == 0 {
        let available = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        cli.mt = (available / 4).max(1);
    }
    cli
}

fn main() {
    let cli = parse_args();

    if cli.files.is_empty() {
        if !cli.quiet {
            eprintln!("optipng-rs: No input files provided.");
        }
        std::process::exit(1);
    }

    let (def_zc, def_zm, def_zs, def_f) = get_opt_combinations(cli.opt_level);
    let zc_list = cli.zc.clone().unwrap_or(def_zc);
    let zm_list = cli.zm.clone().unwrap_or(def_zm);
    let zs_list = cli.zs.clone().unwrap_or(def_zs);
    let f_list = cli.f.clone().unwrap_or(def_f);

    let mut trials = Vec::new();
    for &zc in &zc_list {
        for &zm in &zm_list {
            for &zs in &zs_list {
                if (zs == 2 || zs == 3) && zc > 1 {
                    continue;
                }
                for &f in &f_list {
                    trials.push(TrialConfig { zc, zm, zs, f });
                }
            }
        }
    }

    let total_trials = trials.len();
    let trials = Arc::new(trials);

    for file_path in &cli.files {
        if !cli.quiet {
            println!("Processing: {} ({} parallel trials)", file_path, total_trials);
        }

        let c_file = CString::new(file_path.clone()).unwrap();
        let mut width = 0;
        let mut height = 0;
        let mut bit_depth = 0;
        let mut color_type = 0;
        let mut stride = 0;

        // 1. Load image and metadata via decoder API
        let dec = open_png(
            c_file.as_ptr(),
                           &mut width,
                           &mut height,
                           &mut bit_depth,
                           &mut color_type,
                           &mut stride,
        );

        if dec.is_null() {
            eprintln!("Failed to decode {}", file_path);
            continue;
        }

        // Enable IDAT size tracking on decoder
        png_set_count_idat_size(dec, true);

        let mut out_color_type = color_type;
        let mut out_bit_depth = bit_depth;

        // For palleted images
        if color_type == 3 {
            out_bit_depth = 8;
            out_color_type = if stride == (width as usize * 4) { 6 } else { 2 };
        }

        // PRE-ALLOCATE EXACT CAPACITY
        let expected_size = stride as usize * height as usize;
        let mut raw_pixels = Vec::with_capacity(expected_size);

        loop {
            let res = decode_scanlines(dec, 1024);
            if res.size == 0 || res.data.is_null() {
                break;
            }
            let chunk = unsafe { std::slice::from_raw_parts(res.data, res.size) };
            raw_pixels.extend_from_slice(chunk);
        }
        let orig_idat_size = png_get_idat_size(dec);
        close_png(dec);

        // 2a. Bit depth reduction (Fake 16-bit to 8-bit)
        if out_bit_depth == 16 {
            let mut is_fake_16 = true;
            for chunk in raw_pixels.chunks_exact(2) {
                if chunk[0] != chunk[1] {
                    is_fake_16 = false;
                    break;
                }
            }

            if is_fake_16 {
                let mut write_idx = 0;
                for i in (0..raw_pixels.len()).step_by(2) {
                    raw_pixels[write_idx] = raw_pixels[i];
                    write_idx += 1;
                }
                raw_pixels.truncate(write_idx);
                raw_pixels.shrink_to_fit();

                out_bit_depth = 8;
                stride /= 2;

                if !cli.quiet {
                    println!("  Reducing bit depth from 16 to 8 (fake 16-bit image detected)");
                }
            }
        }

        // 2b. Check opacity & reduce color type if 100% opaque
        if !cli.nc && (out_color_type == 4 || out_color_type == 6) {
            let (bytes_to_keep, bytes_to_skip) = match (out_color_type, out_bit_depth) {
                (6, 8) => (3, 1),
                (4, 8) => (1, 1),
                (6, 16) => (6, 2),
                (4, 16) => (2, 2),
                _ => (0, 0),
            };

            if bytes_to_keep > 0 {
                let pixel_size = bytes_to_keep + bytes_to_skip;
                let w_usize = width as usize;
                let h_usize = height as usize;

                let mut has_transparency = false;
                'check: for y in 0..h_usize {
                    let row_start = y * stride;
                    for x in 0..w_usize {
                        let px_start = row_start + x * pixel_size;
                        if out_bit_depth == 8 {
                            let alpha_offset = px_start + pixel_size - 1;
                            if raw_pixels[alpha_offset] != 0xFF {
                                has_transparency = true;
                                break 'check;
                            }
                        } else if out_bit_depth == 16 {
                            let alpha_offset = px_start + pixel_size - 2;
                            if raw_pixels[alpha_offset] != 0xFF || raw_pixels[alpha_offset + 1] != 0xFF {
                                has_transparency = true;
                                break 'check;
                            }
                        }
                    }
                }

                if !has_transparency {
                    let mut write_idx = 0;
                    for y in 0..h_usize {
                        let row_start = y * stride;
                        for x in 0..w_usize {
                            let px_start = row_start + x * pixel_size;
                            for i in 0..bytes_to_keep {
                                raw_pixels[write_idx] = raw_pixels[px_start + i];
                                write_idx += 1;
                            }
                        }
                    }
                    raw_pixels.truncate(write_idx);
                    raw_pixels.shrink_to_fit();

                    let old_color_type = out_color_type;
                    out_color_type = if out_color_type == 6 { 2 } else { 0 };

                    if !cli.quiet {
                        println!(
                            "  Reducing color type from {} to {} (all pixels are 100% opaque)",
                                 old_color_type, out_color_type
                        );
                    }
                }
            }
        }

        let image_data = Arc::new(raw_pixels);
        let mut handles = Vec::new();

        // Shared work queue index & completed trial counter across threads
        let next_trial_index = Arc::new(AtomicUsize::new(0));
        let completed_trials = Arc::new(AtomicUsize::new(0));

        // 3. Parallel encoding trials (Dynamic Work Stealing Queue)
        for _ in 0..cli.mt {
            let trials_clone = Arc::clone(&trials);
            let img = Arc::clone(&image_data);
            let next_idx = Arc::clone(&next_trial_index);
            let completed = Arc::clone(&completed_trials);
            let quiet = cli.quiet;

            handles.push(thread::spawn(move || {
                let mut best_size = usize::MAX;
                let mut best_config = None;

                loop {
                    let idx = next_idx.fetch_add(1, Ordering::Relaxed);
                    if idx >= trials_clone.len() {
                        break;
                    }

                    let trial = &trials_clone[idx];
                    let mut dummy_written: usize = 0;
                    let options = ZlibOptions {
                        level: trial.zc,
                        strategy: trial.zs,
                        window_bits: 15,
                        mem_level: trial.zm,
                        max_idat_size: 32768,
                        expected_idat_size: 0, // Trial pass
                    };

                    let enc = open_png_encode_stream(
                        counter_write_cb,
                        &mut dummy_written as *mut _ as *mut c_void,
                        width,
                        height,
                        out_bit_depth,
                        out_color_type,
                        trial.f,
                        options,
                    );

                    if !enc.is_null() {
                        encode_scanlines(enc, img.as_ptr(), height);
                        let trial_idat_size = close_png_encode_get_idat_size(enc);

                        if trial_idat_size < best_size && trial_idat_size > 0 {
                            best_size = trial_idat_size;
                            best_config = Some(trial.clone());
                        }
                    }

                    let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
                    if !quiet {
                        print!("\r  Trials completed: {}/{}", done, total_trials);
                        let _ = io::stdout().flush();
                    }
                }
                (best_size, best_config)
            }));
        }

        let mut global_best_size = usize::MAX;
        let mut global_best_config = None;

        for handle in handles {
            if let Ok((size, Some(config))) = handle.join() {
                if size < global_best_size {
                    global_best_size = size;
                    global_best_config = Some(config);
                }
            }
        }

        if !cli.quiet {
            println!(); // Line break after progress counter
        }

        // 4. Final Output Write with atomic .png.old replacement & timestamp preservation
        if let Some(best) = global_best_config {
            if !cli.quiet {
                println!(
                    "\n  Winner: zc={} zm={} zs={} f={} (IDAT payload size: {} bytes vs original: {} bytes)",
                         best.zc, best.zm, best.zs, best.f, global_best_size, orig_idat_size
                );
            }

            // SKIP FILE MODIFICATION IF TRIAL RESULT IS SAME OR LARGER
            if global_best_size >= orig_idat_size {
                if !cli.quiet {
                    println!("  No compression improvement over source IDAT size. Skipping file write.");
                }
                continue;
            }

            if !cli.simulate {
                let max_chunk_size = 0x7FFF_FFFFusize; // 2^31 - 1 bytes
                let num_chunks = (global_best_size + max_chunk_size - 1) / max_chunk_size;

                if !cli.quiet {
                    println!(
                        "  Preparing stream: {} chunk(s) required.",
                             num_chunks
                    );
                }

                let original_path = PathBuf::from(file_path);
                let out_path = if let Some(ref out_f) = cli.out_file {
                    PathBuf::from(out_f)
                } else if let Some(ref out_d) = cli.out_dir {
                    let mut pb = PathBuf::from(out_d);
                    pb.push(original_path.file_name().unwrap());
                    pb
                } else {
                    original_path.clone()
                };

                let orig_metadata = fs::metadata(&original_path).ok();
                let is_in_place = out_path == original_path;
                let old_path = PathBuf::from(format!("{}.bak", file_path));

                if is_in_place {
                    if let Err(e) = fs::rename(&original_path, &old_path) {
                        eprintln!("  Failed to rename original file to {:?}: {}", old_path, e);
                        continue;
                    }
                }

                let c_out_path = CString::new(out_path.to_string_lossy().into_owned()).unwrap();
                let final_options = ZlibOptions {
                    level: best.zc,
                    strategy: best.zs,
                    window_bits: 15,
                    mem_level: best.zm,
                    max_idat_size: 0, // 0 defaults to 0x7FFFFFFF max chunk size
                    expected_idat_size: global_best_size, // Direct streaming mode enabled
                };

                let enc = open_png_encode(
                    c_out_path.as_ptr(),
                                          width,
                                          height,
                                          out_bit_depth,
                                          out_color_type,
                                          best.f,
                                          final_options,
                );

                let mut success = false;
                if !enc.is_null() {
                    encode_scanlines(enc, image_data.as_ptr(), height);
                    close_png_encode(enc);
                    success = out_path.exists();
                }

                if success {
                    preserve_file_times(&out_path, orig_metadata.as_ref());
                    let actual_size = fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
                    if !cli.quiet {
                        println!("  Resulting file size: {} bytes", actual_size);
                    }
                    if is_in_place && !cli.backup {
                        let _ = fs::remove_file(&old_path);
                    }
                } else {
                    eprintln!("  Failed to write optimal stream to {:?}", out_path);
                    if is_in_place {
                        let _ = fs::rename(&old_path, &original_path);
                    }
                }
            }
        }
    }
}
