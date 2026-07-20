use std::env;
use std::ffi::{c_void, CString};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

use pngstreamenc::{
    close_png_encode, encode_scanlines, open_png_encode,
    open_png_encode_stream, PngEncoder, PngWriteCallback, ZlibOptions,
};
use pngstreamdec::{
    close_png, decode_scanlines, open_png, PngDecoder, PngReadCallback, ScanlinesResult,
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
    out_file: Option<String>,
    out_dir: Option<String>,
}

/// In-memory stream writing callback for parallel search trials
unsafe extern "C" fn mem_write_cb(user_data: *mut c_void, buf: *const u8, len: usize) -> usize {
    let vec = &mut *(user_data as *mut Vec<u8>);
    let data = std::slice::from_raw_parts(buf, len);
    vec.extend_from_slice(data);
    len
}

/// Parses combinatorial strings like "0-3" or "0,5" into a flat i32 vector (Stable Rust)
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
        out_file: None,
        out_dir: None,
    };

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-o0" => cli.opt_level = 0,
            "-o1" => cli.opt_level = 1,
            "-o2" => cli.opt_level = 2,
            "-o3" => cli.opt_level = 3,
            "-o4" => cli.opt_level = 4,
            "-o5" => cli.opt_level = 5,
            "-o6" => cli.opt_level = 6,
            "-o7" => cli.opt_level = 7,
            "-o" => {
                if let Some(val) = args.next() {
                    cli.opt_level = val.parse().unwrap_or(2);
                }
            }
            "-mt" => {
                if let Some(val) = args.next() {
                    cli.mt = val.parse().unwrap_or(0);
                }
            }
            "-zc" => if let Some(v) = args.next() { cli.zc = Some(parse_ranges_i32(&v)); },
            "-zm" => if let Some(v) = args.next() { cli.zm = Some(parse_ranges_i32(&v)); },
            "-zs" => if let Some(v) = args.next() { cli.zs = Some(parse_ranges_i32(&v)); },
            "-f"  => if let Some(v) = args.next() { cli.f  = Some(parse_ranges_u8(&v)); },
            "-backup" | "-keep" => cli.backup = true,
            "-simulate" => cli.simulate = true,
            "-quiet" | "-silent" => cli.quiet = true,
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
                for &f in &f_list {
                    trials.push(TrialConfig { zc, zm, zs, f });
                }
            }
        }
    }

    let trials = Arc::new(trials);

    for file_path in &cli.files {
        if !cli.quiet {
            println!("Processing: {} ({} parallel trials)", file_path, trials.len());
        }

        let c_file = CString::new(file_path.clone()).unwrap();
        let mut width = 0;
        let mut height = 0;
        let mut bit_depth = 0;
        let mut color_type = 0;
        let mut stride = 0;

        // 1. Load image and metadata via decoder API
        let dec = unsafe {
            open_png(
                c_file.as_ptr(),
                     &mut width,
                     &mut height,
                     &mut bit_depth,
                     &mut color_type,
                     &mut stride,
            )
        };

        if dec.is_null() {
            eprintln!("Failed to decode {}", file_path);
            continue;
        }

        // Account for decoder expanding indexed (color_type 3) to RGB (2) or RGBA (6)
        let mut out_color_type = color_type;
        let mut out_bit_depth = bit_depth;
        if color_type == 3 {
            out_bit_depth = 8;
            out_color_type = if stride == (width as usize * 4) { 6 } else { 2 };
        }

        let mut raw_pixels = Vec::new();
        loop {
            // Read chunks of scanlines using API
            let res = unsafe { decode_scanlines(dec, 1024) };
            if res.size == 0 || res.data.is_null() {
                break;
            }
            let chunk = unsafe { std::slice::from_raw_parts(res.data, res.size) };
            raw_pixels.extend_from_slice(chunk);
        }
        unsafe { close_png(dec) };

        let image_data = Arc::new(raw_pixels);
        let chunk_size = (trials.len() + cli.mt - 1) / cli.mt;
        let mut handles = Vec::new();

        // 2. Parallel encoding trials
        for i in 0..cli.mt {
            let trials_clone = Arc::clone(&trials);
            let img = Arc::clone(&image_data);

            let start = i * chunk_size;
            let end = ((i + 1) * chunk_size).min(trials_clone.len());
            if start >= end {
                break;
            }

            handles.push(thread::spawn(move || {
                let mut best_size = usize::MAX;
                let mut best_config = None;

                for trial in &trials_clone[start..end] {
                    let mut out_buf: Vec<u8> = Vec::new();
                    let options = ZlibOptions {
                        level: trial.zc,
                        strategy: trial.zs,
                        window_bits: 15,
                        mem_level: trial.zm,
                    };

                    let enc = unsafe {
                        open_png_encode_stream(
                            mem_write_cb,
                            &mut out_buf as *mut _ as *mut c_void,
                            width,
                            height,
                            out_bit_depth,
                            out_color_type,
                            trial.f,
                            options,
                        )
                    };

                    if !enc.is_null() {
                        unsafe {
                            encode_scanlines(enc, img.as_ptr(), height);
                            close_png_encode(enc);
                        }

                        if out_buf.len() < best_size && !out_buf.is_empty() {
                            best_size = out_buf.len();
                            best_config = Some(trial.clone());
                        }
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

        // 3. Final Output Write
        if let Some(best) = global_best_config {
            if !cli.simulate {
                let out_path = if let Some(ref out_f) = cli.out_file {
                    PathBuf::from(out_f)
                } else if let Some(ref out_d) = cli.out_dir {
                    let mut pb = PathBuf::from(out_d);
                    pb.push(PathBuf::from(file_path).file_name().unwrap());
                    pb
                } else {
                    PathBuf::from(file_path)
                };

                if cli.backup {
                    let backup_path = out_path.with_extension("bak");
                    let _ = fs::copy(&file_path, backup_path);
                }

                let c_out_path = CString::new(out_path.to_string_lossy().into_owned()).unwrap();
                let final_options = ZlibOptions {
                    level: best.zc,
                    strategy: best.zs,
                    window_bits: 15,
                    mem_level: best.zm,
                };

                let enc = unsafe {
                    open_png_encode(
                        c_out_path.as_ptr(),
                                    width,
                                    height,
                                    out_bit_depth,
                                    out_color_type,
                                    best.f,
                                    final_options,
                    )
                };

                if !enc.is_null() {
                    unsafe {
                        encode_scanlines(enc, image_data.as_ptr(), height);
                        close_png_encode(enc);
                    }
                    if !cli.quiet {
                        println!(
                            "  Winner: zc={} zm={} zs={} f={} ({} bytes)",
                                 best.zc, best.zm, best.zs, best.f, global_best_size
                        );
                    }
                } else {
                    eprintln!("  Failed to write optimal stream to {:?}", out_path);
                }
            } else if !cli.quiet {
                println!(
                    "  Simulated Winner: zc={} zm={} zs={} f={}",
                    best.zc, best.zm, best.zs, best.f
                );
            }
        }
    }
}
