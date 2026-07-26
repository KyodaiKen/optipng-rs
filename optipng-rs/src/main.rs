use std::cmp::Reverse;
use std::env;
use std::ffi::{c_void, CString};
use std::fs::{self, FileTimes, Metadata};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use pngstreamdec::{
    close_png, decode_scanlines, open_png, png_get_idat_size, png_set_count_idat_size,
};
use pngstreamenc::{
    close_png_encode, close_png_encode_get_idat_size, encode_scanlines, open_png_encode,
    open_png_encode_stream, ZlibOptions,
};

// =========================================================================
// OPTIPNG ENGINE & STRUCTURES
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
    external_input: Option<String>,
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
    show_help: bool,
}

struct LoadedImage {
    width: u32,
    height: u32,
    bit_depth: u8,
    color_type: u8,
    stride: usize,
    raw_pixels: Vec<u8>,
    orig_idat_size: usize,
}

// =========================================================================
// EXTERNAL FORMAT DECODERS (TGA / PPM / PGM / PAM)
// =========================================================================

/// Decodes Targa (.tga) images (Uncompressed and RLE TrueColor/Grayscale)
fn decode_tga(data: &[u8]) -> Result<LoadedImage, String> {
    if data.len() < 18 {
        return Err("TGA header too short".into());
    }
    let id_len = data[0] as usize;
    let color_map_type = data[1];
    let image_type = data[2];
    let width = u16::from_le_bytes([data[12], data[13]]) as u32;
    let height = u16::from_le_bytes([data[14], data[15]]) as u32;
    let bpp = data[16];
    let descriptor = data[17];

    if color_map_type != 0 {
        return Err("Color-mapped TGA is not supported".into());
    }
    if width == 0 || height == 0 {
        return Err("Invalid TGA image dimensions".into());
    }

    let is_top_down = (descriptor & 0x20) != 0;
    let is_right_to_left = (descriptor & 0x10) != 0;

    let (channels, color_type) = match bpp {
        8 => (1, 0u8),   // Grayscale
        24 => (3, 2u8),  // RGB
        32 => (4, 6u8),  // RGBA
        _ => return Err(format!("Unsupported TGA bits-per-pixel: {}", bpp)),
    };

    let mut offset = 18 + id_len;
    let pixel_count = (width * height) as usize;
    let mut pixels = Vec::with_capacity(pixel_count * channels);

    if image_type == 2 || image_type == 3 {
        // Uncompressed TrueColor or Grayscale
        let required = offset + pixel_count * channels;
        if data.len() < required {
            return Err("Truncated TGA pixel data".into());
        }
        let raw = &data[offset..required];
        for chunk in raw.chunks_exact(channels) {
            if channels >= 3 {
                pixels.push(chunk[2]); // Convert BGR(A) -> RGB(A)
                pixels.push(chunk[1]);
                pixels.push(chunk[0]);
                if channels == 4 {
                    pixels.push(chunk[3]);
                }
            } else {
                pixels.extend_from_slice(chunk);
            }
        }
    } else if image_type == 10 || image_type == 11 {
        // RLE Compressed TrueColor or Grayscale
        while pixels.len() < pixel_count * channels && offset < data.len() {
            let packet_header = data[offset];
            offset += 1;
            let count = ((packet_header & 0x7F) as usize) + 1;
            let is_rle = (packet_header & 0x80) != 0;

            if is_rle {
                if offset + channels > data.len() {
                    return Err("Unexpected EOF in TGA RLE stream".into());
                }
                let pixel = &data[offset..offset + channels];
                offset += channels;
                for _ in 0..count {
                    if channels >= 3 {
                        pixels.push(pixel[2]);
                        pixels.push(pixel[1]);
                        pixels.push(pixel[0]);
                        if channels == 4 {
                            pixels.push(pixel[3]);
                        }
                    } else {
                        pixels.extend_from_slice(pixel);
                    }
                }
            } else {
                let bytes_needed = count * channels;
                if offset + bytes_needed > data.len() {
                    return Err("Unexpected EOF in TGA raw stream".into());
                }
                let raw = &data[offset..offset + bytes_needed];
                offset += bytes_needed;
                for chunk in raw.chunks_exact(channels) {
                    if channels >= 3 {
                        pixels.push(chunk[2]);
                        pixels.push(chunk[1]);
                        pixels.push(chunk[0]);
                        if channels == 4 {
                            pixels.push(chunk[3]);
                        }
                    } else {
                        pixels.extend_from_slice(chunk);
                    }
                }
            }
        }
    } else {
        return Err(format!("Unsupported TGA image type code: {}", image_type));
    }

    // Orient image scanlines correctly (Default TGA is bottom-up)
    let row_bytes = width as usize * channels;
    let mut final_pixels = vec![0u8; pixels.len()];

    for y in 0..height as usize {
        let src_y = if is_top_down { y } else { (height as usize - 1) - y };
        let src_start = src_y * row_bytes;
        let dst_start = y * row_bytes;

        if is_right_to_left {
            for x in 0..width as usize {
                let src_x = (width as usize - 1) - x;
                let src_px = src_start + src_x * channels;
                let dst_px = dst_start + x * channels;
                final_pixels[dst_px..dst_px + channels].copy_from_slice(&pixels[src_px..src_px + channels]);
            }
        } else {
            final_pixels[dst_start..dst_start + row_bytes].copy_from_slice(&pixels[src_start..src_start + row_bytes]);
        }
    }

    Ok(LoadedImage {
        width,
        height,
        bit_depth: 8,
        color_type,
        stride: row_bytes,
        raw_pixels: final_pixels,
        orig_idat_size: data.len(),
    })
}

/// Decodes Netpbm images (.ppm, .pgm, .pam)
fn decode_netpbm(data: &[u8]) -> Result<LoadedImage, String> {
    let mut pos = 0;

    let read_line = |pos: &mut usize| -> Result<String, String> {
        let start = *pos;
        while *pos < data.len() && data[*pos] != b'\n' {
            *pos += 1;
        }
        if *pos >= data.len() {
            return Err("Unexpected EOF reading header".into());
        }
        let line_bytes = &data[start..*pos];
        *pos += 1;
        std::str::from_utf8(line_bytes)
        .map(|s| s.trim().to_string())
        .map_err(|e| e.to_string())
    };

    let magic = read_line(&mut pos)?;

    if magic == "P7" {
        // PAM Format
        let mut width = 0u32;
        let mut height = 0u32;
        let mut depth = 0u32;
        let mut maxval = 0u32;
        let mut tupltype = String::new();

        loop {
            let line = read_line(&mut pos)?;
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            if line == "ENDHDR" {
                break;
            }
            if let Some((key, val)) = line.split_once(' ') {
                let val_trimmed = val.trim();
                match key.trim() {
                    "WIDTH" => width = val_trimmed.parse().unwrap_or(0),
                    "HEIGHT" => height = val_trimmed.parse().unwrap_or(0),
                    "DEPTH" => depth = val_trimmed.parse().unwrap_or(0),
                    "MAXVAL" => maxval = val_trimmed.parse().unwrap_or(0),
                    "TUPLTYPE" => tupltype = val_trimmed.to_string(),
                    _ => {}
                }
            }
        }

        if width == 0 || height == 0 || depth == 0 || maxval == 0 {
            return Err("Invalid PAM header configuration".into());
        }

        let color_type = match (depth, tupltype.as_str()) {
            (1, _) => 0u8, // Grayscale
            (2, _) => 4u8, // Gray + Alpha
            (3, _) => 2u8, // RGB
            (4, _) => 6u8, // RGBA
            _ => match depth {
                1 => 0, 2 => 4, 3 => 2, 4 => 6,
                _ => return Err(format!("Unsupported PAM depth: {}", depth)),
            },
        };

        let bit_depth = if maxval <= 255 { 8 } else if maxval <= 65535 { 16 } else { return Err("Unsupported PAM maxval".into()); };
        let bytes_per_sample = if bit_depth == 8 { 1 } else { 2 };
        let stride = width as usize * depth as usize * bytes_per_sample;
        let expected_bytes = height as usize * stride;

        if data.len() - pos < expected_bytes {
            return Err("PAM pixel data truncated".into());
        }

        let raw_pixels = data[pos..pos + expected_bytes].to_vec();

        Ok(LoadedImage {
            width,
            height,
            bit_depth,
            color_type,
            stride,
            raw_pixels,
            orig_idat_size: data.len(),
        })
    } else if magic == "P5" || magic == "P6" {
        // P5 = Binary PGM, P6 = Binary PPM
        let is_rgb = magic == "P6";
        let mut header_tokens = Vec::new();

        while header_tokens.len() < 3 && pos < data.len() {
            while pos < data.len() && data[pos].is_ascii_whitespace() {
                pos += 1;
            }
            if pos < data.len() && data[pos] == b'#' {
                while pos < data.len() && data[pos] != b'\n' {
                    pos += 1;
                }
                continue;
            }
            let tok_start = pos;
            while pos < data.len() && !data[pos].is_ascii_whitespace() {
                pos += 1;
            }
            if tok_start < pos {
                if let Ok(tok) = std::str::from_utf8(&data[tok_start..pos]) {
                    header_tokens.push(tok.to_string());
                }
            }
        }

        if header_tokens.len() < 3 {
            return Err("Invalid PPM/PGM header structure".into());
        }

        if pos < data.len() && data[pos].is_ascii_whitespace() {
            pos += 1;
        }

        let width: u32 = header_tokens[0].parse().map_err(|_| "Invalid width")?;
        let height: u32 = header_tokens[1].parse().map_err(|_| "Invalid height")?;
        let maxval: u32 = header_tokens[2].parse().map_err(|_| "Invalid maxval")?;

        let color_type = if is_rgb { 2u8 } else { 0u8 };
        let channels = if is_rgb { 3 } else { 1 };
        let bit_depth = if maxval <= 255 { 8 } else { 16 };
        let bytes_per_sample = if bit_depth == 8 { 1 } else { 2 };
        let stride = width as usize * channels * bytes_per_sample;
        let expected_bytes = height as usize * stride;

        if data.len() - pos < expected_bytes {
            return Err("PPM/PGM pixel data truncated".into());
        }

        let raw_pixels = data[pos..pos + expected_bytes].to_vec();

        Ok(LoadedImage {
            width,
            height,
            bit_depth,
            color_type,
            stride,
            raw_pixels,
            orig_idat_size: data.len(),
        })
    } else {
        Err("Unsupported Netpbm magic format".into())
    }
}

/// Dispatches image loading based on file headers / extensions
fn load_external_image(file_path: &str) -> Result<LoadedImage, String> {
    let data = fs::read(file_path).map_err(|e| format!("Failed to read file {:?}: {}", file_path, e))?;

    if data.starts_with(b"P5") || data.starts_with(b"P6") || data.starts_with(b"P7") {
        decode_netpbm(&data)
    } else {
        decode_tga(&data)
    }
}

// =========================================================================
// HELPER FUNCTIONS & CLI PARSER
// =========================================================================

fn print_usage() {
    println!("optipng-rs: High-performance parallel PNG optimizer and converter\n");
    println!("USAGE:");
    println!("  optipng-rs [options] <file1.png> [file2.png ...]");
    println!("  optipng-rs [options] -e <input_file> [output.png]\n");
    println!("OPTIONS:");
    println!("  -o <level>         Optimization level 0-7 (default: 2)");
    println!("  -mt <threads>      Number of worker threads (default: 75% of CPUs)");
    println!("  -e <file>          External input file format (TGA, PPM, PGM, PAM) to encode");
    println!("  -out <file>        Output file path");
    println!("  -dir <directory>   Output directory");
    println!("  -zc <levels>       zlib compression levels (e.g. -zc1-9 or -zc9)");
    println!("  -zm <levels>       zlib memory levels (e.g. -zm1-9 or -zm8,9)");
    println!("  -zs <strategies>   zlib compression strategies (e.g. -zs0-3)");
    println!("  -f <filters>       PNG delta filter algorithms (e.g. -f0,5 or -f0-5)");
    println!("  -nc                Disable color type & transparency reduction");
    println!("  -backup, -keep     Keep backup copy of original file (.bak)");
    println!("  -simulate          Simulation mode (trials only, no file writes)");
    println!("  -quiet, -silent    Quiet mode");
    println!("  -h, --help         Print this help message\n");
}

fn color_type_name(color_type: u8) -> &'static str {
    match color_type {
        0 => "Y (Grayscale)",
        2 => "RGB",
        3 => "Palette",
        4 => "YA (Grayscale+Transparency)",
        6 => "RGBA (RGB+Transparency)",
        _ => "Unknown",
    }
}

unsafe extern "C" fn counter_write_cb(user_data: *mut c_void, _buf: *const u8, len: usize) -> usize {
    if !user_data.is_null() && !_buf.is_null() {
        let counter = unsafe { &mut *(user_data as *mut usize) };
        *counter += len;
    }
    len
}

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

fn parse_ranges_u8(input: &str) -> Vec<u8> {
    parse_ranges_i32(input)
    .into_iter()
    .filter_map(|v| u8::try_from(v).ok())
    .collect()
}

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
        external_input: None,
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
        out_dir: None,
        show_help: false,
    };

    while let Some(arg) = args.next() {
        if arg == "-h" || arg == "--help" {
            cli.show_help = true;
            return cli;
        }

        if arg == "-e" {
            if let Some(val) = args.next() {
                cli.external_input = Some(val);
            } else {
                eprintln!("Error: Option -e requires an input file argument.");
                cli.show_help = true;
                return cli;
            }
            continue;
        }

        if arg.starts_with("-o") && arg.len() > 2 && arg[2..].chars().all(|c| c.is_ascii_digit()) {
            let level: u8 = arg[2..].parse().unwrap_or(2);
            cli.opt_level = level.min(7);
            continue;
        }

        if arg.starts_with("-mt") && arg.len() > 3 && arg[3..].chars().all(|c| c.is_ascii_digit()) {
            cli.mt = arg[3..].parse().unwrap_or(0);
            continue;
        }

        if arg.starts_with("-zc") {
            let val_str = if arg.len() > 3 { Some(arg[3..].to_string()) } else { args.next() };
            if let Some(v) = val_str {
                let mut parsed = parse_ranges_i32(&v);
                cli.zc.get_or_insert_with(Vec::new).append(&mut parsed);
            }
            continue;
        }

        if arg.starts_with("-zm") {
            let val_str = if arg.len() > 3 { Some(arg[3..].to_string()) } else { args.next() };
            if let Some(v) = val_str {
                let mut parsed = parse_ranges_i32(&v);
                cli.zm.get_or_insert_with(Vec::new).append(&mut parsed);
            }
            continue;
        }

        if arg.starts_with("-zs") {
            let val_str = if arg.len() > 3 { Some(arg[3..].to_string()) } else { args.next() };
            if let Some(v) = val_str {
                let mut parsed = parse_ranges_i32(&v);
                cli.zs.get_or_insert_with(Vec::new).append(&mut parsed);
            }
            continue;
        }

        if arg.starts_with("-f") {
            let val_str = if arg.len() > 2 { Some(arg[2..].to_string()) } else { args.next() };
            if let Some(v) = val_str {
                let mut parsed = parse_ranges_u8(&v);
                cli.f.get_or_insert_with(Vec::new).append(&mut parsed);
            }
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

    if let Some(ref mut list) = cli.zc { list.sort_unstable(); list.dedup(); }
    if let Some(ref mut list) = cli.zm { list.sort_unstable(); list.dedup(); }
    if let Some(ref mut list) = cli.zs { list.sort_unstable(); list.dedup(); }
    if let Some(ref mut list) = cli.f  { list.sort_unstable(); list.dedup(); }

    if cli.mt == 0 {
        let available = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        cli.mt = ((available * 3) / 4).max(1);
    }
    cli
}

fn format_bytes(bytes: usize) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB", "PB", "EB", "ZB", "YB"];
    if bytes == 0 {
        return "0 B".to_string();
    }
    let base = 1024f64;
    let bytes_f = bytes as f64;
    let digit = (bytes_f.log(base)).floor() as usize;
    let digit = digit.min(UNITS.len() - 1);

    if digit == 0 {
        format!("{} {}", bytes, UNITS[0])
    } else {
        let value = bytes_f / base.powi(digit as i32);
        format!("{:.2} {}", value, UNITS[digit])
    }
}

fn format_duration(duration: Duration) -> String {
    let total_millis = duration.as_millis();
    let millis = (total_millis % 1000) as u64;
    let total_secs = duration.as_secs();
    let secs = total_secs % 60;
    let total_mins = total_secs / 60;
    let mins = total_mins % 60;
    let total_hours = total_mins / 60;
    let hours = total_hours % 24;
    let days = total_hours / 24;

    if days > 0 {
        format!("{}:{:02}:{:02}:{:02}.{:03}", days, hours, mins, secs, millis)
    } else if hours > 0 {
        format!("{:02}:{:02}:{:02}.{:03}", hours, mins, secs, millis)
    } else {
        format!("{:02}:{:02}.{:03}", mins, secs, millis)
    }
}

fn zs_difficulty(zs: i32) -> u8 {
    match zs {
        1 => 4, // Filtered (Hardest)
        0 => 3, // Default
        2 => 2, // Huffman-only
        3 => 1, // RLE (Easiest)
        _ => 0,
    }
}

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

    // Sort difficult trials first (LPT Scheduling)
    trials.sort_by_key(|t| Reverse((t.zc, zs_difficulty(t.zs), t.zm, t.f)));

    let total_trials = trials.len();
    let trials = Arc::new(trials);

    // Prepare list of file target tasks: (Input path, Output path, is_external)
    let mut tasks: Vec<(String, String, bool)> = Vec::new();

    if let Some(ext_in) = cli.external_input {
        let out_path = if let Some(ref out_f) = cli.out_file {
            out_f.clone()
        } else if !cli.files.is_empty() {
            cli.files[0].clone()
        } else {
            let p = PathBuf::from(&ext_in);
            p.with_extension("png").to_string_lossy().to_string()
        };
        tasks.push((ext_in, out_path, true));
    } else {
        for f in &cli.files {
            tasks.push((f.clone(), f.clone(), false));
        }
    }

    for (file_path, target_out_path, is_external) in tasks {
        if !cli.quiet {
            println!("Processing: {} ({} trials, with {} parallel threads)", file_path, total_trials, cli.mt);
        }

        let mut width: u32 = 0;
        let mut height: u32 = 0;
        let mut bit_depth: u8 = 0;
        let mut color_type: u8 = 0;
        let mut stride: usize = 0;
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

            png_set_count_idat_size(dec, true);

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

        if !cli.quiet {
            println!(
                "  Input Image ......... : {} x {} / {} bpc / {} / {} bpp",
                width,
                height,
                bit_depth,
                color_type_name(color_type),
                bit_depth * (match color_type { 0 | 3 => 1, 2 => 3, 4 => 2, 6 => 4, _ => 0 })
            );
        }

        let mut out_color_type = color_type;
        let mut out_bit_depth = bit_depth;

        if color_type == 3 {
            out_bit_depth = 8;
            out_color_type = if stride == (width as usize * 4) { 6 } else { 2 };
        }

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
                    println!("  (i) Reducing bit depth from 16 to 8 (fake 16-bit image detected)");
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
                            "  (i) Reducing color type from {} to {} (all pixels are 100% opaque)",
                                 color_type_name(old_color_type),
                                 color_type_name(out_color_type)
                        );
                    }
                }
            }
        }

        if !cli.quiet {
            println!(
                "  Image loaded ........ : {} bytes ({}) in memory. Starting trials...",
                     raw_pixels.len(),
                     format_bytes(raw_pixels.len())
            );
        }

        // 3. Parallel encoding trials (Dynamic Work Stealing Queue)
        let image_data = Arc::new(raw_pixels);
        let mut handles = Vec::new();

        let next_trial_index = Arc::new(AtomicUsize::new(0));
        let completed_trials = Arc::new(AtomicUsize::new(0));
        let completed_scanlines = Arc::new(AtomicUsize::new(0));
        let stdout_lock = Arc::new(std::sync::Mutex::new(()));

        let total_rows = height as usize;
        let row_bytes = if total_rows > 0 { image_data.len() / total_rows } else { 0 };
        let total_scanlines = total_trials * total_rows;

        let start_trials = Instant::now();

        for _ in 0..cli.mt {
            let trials_clone = Arc::clone(&trials);
            let img = Arc::clone(&image_data);
            let next_idx = Arc::clone(&next_trial_index);
            let completed = Arc::clone(&completed_trials);
            let scanlines_acc = Arc::clone(&completed_scanlines);
            let lock = Arc::clone(&stdout_lock);
            let quiet = cli.quiet;

            handles.push(thread::spawn(move || {
                let mut best_size = usize::MAX;
                let mut best_config = None;
                let chunk_rows = 250;

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
                        expected_idat_size: 0,
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
                        let mut encoded_rows = 0usize;
                        while encoded_rows < total_rows {
                            let rows_to_encode = (total_rows - encoded_rows).min(chunk_rows);
                            let offset = encoded_rows * row_bytes;
                            let ptr = unsafe { img.as_ptr().add(offset) };

                            encode_scanlines(enc, ptr, rows_to_encode as u32);
                            encoded_rows += rows_to_encode;

                            let cur_scanlines = scanlines_acc.fetch_add(rows_to_encode, Ordering::Relaxed) + rows_to_encode;

                            if !quiet && total_scanlines > 0 {
                                let done = completed.load(Ordering::Relaxed);
                                let progress_pct = ((cur_scanlines as f64 / total_scanlines as f64) * 100.0).min(100.0);
                                if let Ok(_guard) = lock.try_lock() {
                                    print!("\r  Trial progress ...... : {:.2} percent, {} trials done", progress_pct, done);
                                    let _ = io::stdout().flush();
                                }
                            }
                        }

                        let trial_idat_size = close_png_encode_get_idat_size(enc);

                        if trial_idat_size < best_size && trial_idat_size > 0 {
                            best_size = trial_idat_size;
                            best_config = Some(trial.clone());
                        }
                    }

                    let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
                    if !quiet && total_scanlines > 0 {
                        let cur_scanlines = scanlines_acc.load(Ordering::Relaxed);
                        let progress_pct = ((cur_scanlines as f64 / total_scanlines as f64) * 100.0).min(100.0);
                        if let Ok(_guard) = lock.try_lock() {
                            print!("\r  Trial progress ...... : {:.2} percent, {} trials done", progress_pct, done);
                            let _ = io::stdout().flush();
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

        let trial_duration = start_trials.elapsed();
        if !cli.quiet {
            println!("\r  Trial process took .. : {}                       ", format_duration(trial_duration));
        }

        // 4. Final Output Write
        if let Some(best) = global_best_config {
            if !cli.quiet {
                if total_trials > 1 {
                    println!(
                        "  Best parameters ..... : -zc{} -zm{} -zs{} -f{}",
                        best.zc, best.zm, best.zs, best.f,
                    );
                } else {
                    println!(
                        "  Used parameters ..... : -zc{} -zm{} -zs{} -f{}",
                        best.zc, best.zm, best.zs, best.f,
                    );
                }
                println!(
                    "  New image data size . : {} bytes ({})\n  vs original ......... : {} bytes ({})",
                    global_best_size,
                    format_bytes(global_best_size),
                    orig_idat_size,
                    format_bytes(orig_idat_size),
                );
            }

            // For existing PNGs, skip file modification if trial result is larger
            if !is_external && global_best_size >= orig_idat_size {
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
                let old_path = PathBuf::from(format!("{}.bak", file_path));

                if is_in_place {
                    if let Err(e) = fs::rename(&original_path, &old_path) {
                        eprintln!("  (x) Failed to rename original file to {:?}: {}", old_path, e);
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
                    let total_rows = height as usize;
                    let row_bytes = if total_rows > 0 { image_data.len() / total_rows } else { 0 };
                    let mut encoded_rows = 0usize;
                    let chunk_rows = 250;

                    while encoded_rows < total_rows {
                        let rows_to_encode = (total_rows - encoded_rows).min(chunk_rows);
                        let offset = encoded_rows * row_bytes;
                        let ptr = unsafe { image_data.as_ptr().add(offset) };

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

                if success {
                    let encode_duration = start_encode.elapsed();
                    if !cli.quiet {
                        println!("\r  Final encoding took . : {}               ", format_duration(encode_duration));
                    }

                    preserve_file_times(&out_path, orig_metadata.as_ref());
                    let actual_size = fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
                    if !cli.quiet {
                        println!("  Resulting file size . : {} bytes ({})", actual_size, format_bytes(actual_size as usize));
                    }
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
}
