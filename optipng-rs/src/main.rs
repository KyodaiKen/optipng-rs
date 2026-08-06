use std::cmp::Reverse;
use std::env;
use std::ffi::{c_void, CString};
use std::ffi::CStr;
use std::fs::{self, FileTimes, Metadata};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use pngstreamdec::{
    close_png, decode_scanlines, open_png, png_get_idat_size, png_set_count_idat_size,
    png_get_text_count, png_get_text_data, free_text_data
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
    zi: u8,
    zc: Option<Vec<i32>>,
    zm: Option<Vec<i32>>,
    zs: Option<Vec<i32>>,
    f: Option<Vec<u8>>,
    backup: bool,
    simulate: bool,
    quiet: bool,
    nc: bool,
    nb: bool,
    np: bool,
    nx: bool,
    nz: bool,
    out_file: Option<String>,
    out_dir: Option<String>,
    show_help: bool,
    force_trials: bool,
    force_reenc: bool,
    cmd_options: String,
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
// RAW PNG CHUNK COPY & RE-PARTITIONING FOR -o0 / -nz
// =========================================================================

fn make_crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    for n in 0..256 {
        let mut c = n as u32;
        for _ in 0..8 {
            if c & 1 != 0 {
                c = 0xEDB8_8320 ^ (c >> 1);
            } else {
                c >>= 1;
            }
        }
        table[n] = c;
    }
    table
}

fn update_crc(mut crc: u32, table: &[u32; 256], buf: &[u8]) -> u32 {
    for &b in buf {
        crc = table[((crc ^ (b as u32)) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc
}

fn png_crc(chunk_type: &[u8; 4], data: &[u8]) -> u32 {
    let table = make_crc_table();
    let mut crc = 0xFFFF_FFFFu32;
    crc = update_crc(crc, &table, chunk_type);
    crc = update_crc(crc, &table, data);
    !crc
}

/// Copies existing compressed IDAT bytes from an existing PNG without zlib re-encoding,
/// coalesces IDAT chunks, and inserts/updates the `tEXt` metadata chunk.
fn copy_png_idat_and_add_text(
    in_path: &Path,
    out_path: &Path,
    text_keyword: &str,
    text_value: &str,
) -> Result<usize, String> {
    let raw_data = fs::read(in_path).map_err(|e| format!("Failed to read input file: {}", e))?;
    if raw_data.len() < 8 || &raw_data[0..8] != b"\x89PNG\r\n\x1a\n" {
        return Err("Not a valid PNG file".to_string());
    }

    let mut pos = 8;
    let mut chunks_before_idat: Vec<Vec<u8>> = Vec::new();
    let mut chunks_after_idat: Vec<Vec<u8>> = Vec::new();
    let mut combined_idat: Vec<u8> = Vec::new();
    let mut seen_idat = false;

    while pos + 12 <= raw_data.len() {
        let length = u32::from_be_bytes(raw_data[pos..pos + 4].try_into().unwrap()) as usize;
        let chunk_type: [u8; 4] = raw_data[pos + 4..pos + 8].try_into().unwrap();
        let data_start = pos + 8;
        let data_end = data_start + length;
        let crc_start = data_end;

        if crc_start + 4 > raw_data.len() {
            return Err("PNG chunk extended beyond EOF".to_string());
        }

        pos = crc_start + 4;
        let chunk_data = &raw_data[data_start..data_end];

        match &chunk_type {
            b"IHDR" => {
                let mut full_chunk = Vec::with_capacity(12 + length);
                full_chunk.extend_from_slice(&(length as u32).to_be_bytes());
                full_chunk.extend_from_slice(&chunk_type);
                full_chunk.extend_from_slice(chunk_data);
                let crc = png_crc(&chunk_type, chunk_data);
                full_chunk.extend_from_slice(&crc.to_be_bytes());
                chunks_before_idat.push(full_chunk);
            }
            b"IDAT" => {
                seen_idat = true;
                combined_idat.extend_from_slice(chunk_data);
            }
            b"IEND" => {
                break;
            }
            b"tEXt" => {
                // Remove pre-existing optipng-rs text chunk if updating
                if let Some(null_idx) = chunk_data.iter().position(|&b| b == 0) {
                    if let Ok(keyword) = std::str::from_utf8(&chunk_data[..null_idx]) {
                        if keyword == text_keyword {
                            continue;
                        }
                    }
                }
                let mut full_chunk = Vec::with_capacity(12 + length);
                full_chunk.extend_from_slice(&(length as u32).to_be_bytes());
                full_chunk.extend_from_slice(&chunk_type);
                full_chunk.extend_from_slice(chunk_data);
                let crc = png_crc(&chunk_type, chunk_data);
                full_chunk.extend_from_slice(&crc.to_be_bytes());
                if seen_idat {
                    chunks_after_idat.push(full_chunk);
                } else {
                    chunks_before_idat.push(full_chunk);
                }
            }
            _ => {
                let mut full_chunk = Vec::with_capacity(12 + length);
                full_chunk.extend_from_slice(&(length as u32).to_be_bytes());
                full_chunk.extend_from_slice(&chunk_type);
                full_chunk.extend_from_slice(chunk_data);
                let crc = png_crc(&chunk_type, chunk_data);
                full_chunk.extend_from_slice(&crc.to_be_bytes());
                if seen_idat {
                    chunks_after_idat.push(full_chunk);
                } else {
                    chunks_before_idat.push(full_chunk);
                }
            }
        }
    }

    if combined_idat.is_empty() {
        return Err("No IDAT chunk found in PNG file".to_string());
    }

    let mut out_file = fs::File::create(out_path).map_err(|e| format!("Failed to create output file: {}", e))?;

    // 1. Write Header
    out_file.write_all(b"\x89PNG\r\n\x1a\n").map_err(|e| e.to_string())?;

    // 2. Write Chunks Before IDAT (IHDR, PLTE, tRNS, etc.)
    for chunk in &chunks_before_idat {
        out_file.write_all(chunk).map_err(|e| e.to_string())?;
    }

    // 3. Write new tEXt chunk
    let mut text_data = Vec::new();
    text_data.extend_from_slice(text_keyword.as_bytes());
    text_data.push(0); // Null byte separator
    text_data.extend_from_slice(text_value.as_bytes());

    let text_len = text_data.len() as u32;
    let text_crc = png_crc(b"tEXt", &text_data);

    out_file.write_all(&text_len.to_be_bytes()).map_err(|e| e.to_string())?;
    out_file.write_all(b"tEXt").map_err(|e| e.to_string())?;
    out_file.write_all(&text_data).map_err(|e| e.to_string())?;
    out_file.write_all(&text_crc.to_be_bytes()).map_err(|e| e.to_string())?;

    // 4. Write coalesced IDAT chunk(s) (Max size 0x7FFFFFFF per chunk)
    const MAX_IDAT_CHUNK_SIZE: usize = 0x7FFFFFFF;
    for chunk_slice in combined_idat.chunks(MAX_IDAT_CHUNK_SIZE) {
        let idat_len = chunk_slice.len() as u32;
        let idat_crc = png_crc(b"IDAT", chunk_slice);

        out_file.write_all(&idat_len.to_be_bytes()).map_err(|e| e.to_string())?;
        out_file.write_all(b"IDAT").map_err(|e| e.to_string())?;
        out_file.write_all(chunk_slice).map_err(|e| e.to_string())?;
        out_file.write_all(&idat_crc.to_be_bytes()).map_err(|e| e.to_string())?;
    }

    // 5. Write Chunks After IDAT
    for chunk in &chunks_after_idat {
        out_file.write_all(chunk).map_err(|e| e.to_string())?;
    }

    // 6. Write IEND chunk
    let iend_crc = png_crc(b"IEND", &[]);
    out_file.write_all(&0u32.to_be_bytes()).map_err(|e| e.to_string())?;
    out_file.write_all(b"IEND").map_err(|e| e.to_string())?;
    out_file.write_all(&iend_crc.to_be_bytes()).map_err(|e| e.to_string())?;

    out_file.flush().map_err(|e| e.to_string())?;

    Ok(combined_idat.len())
}

// =========================================================================
// HELPER FUNCTIONS & CLI PARSER
// =========================================================================

fn print_usage() {
    let version = env!("CARGO_PKG_VERSION");
    println!(
    r#"optipng-rs v{version} - High-performance parallel PNG optimizer and converter

USAGE:
  optipng-rs [options] <file1.png> [file2.png ...]
  optipng-rs [options] -e <input_file> [output.png]

GENERAL OPTIONS:
  -h, --help         Print this help message
  -quiet, -silent    Quiet mode (suppress non-error output)
  -mt <threads>      Number of worker threads (default: 75% of CPUs)
  -backup, -keep     Keep a backup copy of original files (.bak)
  -simulate          Simulation mode (run trials only, skip writing files)
  -force             Force file write even if compressed size increases
  -ft                Force trials even if file was previously optimized

OPTIMIZATION OPTIONS:
  -o <level>         Optimization level 0-7 (default: 2)
  -zi <1|2>          Encoder implementation: 1 = zlib (default), 2 = Zöpfli (SLOW!!!)
                        Zöpfli only supports the parameters -zc and -f, and
                        the compression level is mapped to Zöpfli's number of
                        iterations as follows (level => itrerations):
                            1 => 1
                            2 => 3
                            3 => 5
                            4 => 10
                            5 => 15   Zöpfli default
                            6 => 30
                            7 => 50
                            8 => 100
                            9 => 500  Maximum squeeze

                        Optimization preset levels (-o) are as follows:
                            -o0 => unchanged, no IDAT re-encoding
                            -o1 => -zc1 -f3
                            -o2 => -zc2 -f5
                            -o3 => -zc3 -f5
                            -o4 => -zc4 -f5
                            -o5 => -zc5 -f5
                            -o6 => -zc6 -f5
                            -o7 => -zc7 -f3,5

                        Memory usage: Trial results will remain in memory and Zöpfli
                        needs random access to the data. The image data is also
                        in memory as often as you have trials plus the raw pixels
                        from the original file. Data is freed from memory as soon
                        as it isn't needed anymore, though the peak memory will be
                        as explained above + runtime and Zöpfli overhead.

  -zc <levels>       zlib compression levels (e.g., -zc1-9 or -zc9)
  -zm <levels>       zlib memory levels (e.g., -zm1-9 or -zm8,9)
  -zs <strategies>   zlib compression strategies (e.g., -zs0-3)
  -f <filters>       PNG delta filter algorithms (e.g., -f0,5 or -f0-5)
  -nz                No IDAT recoding (fast path / zero re-compression)

IMAGE REDUCTION OPTIONS:
  -nb                Disable bit depth reduction
  -nc                Disable color type reduction
  -np                Disable palette reduction
  -nx                Disable all image reductions (-nb, -nc, -np)

INPUT / OUTPUT OPTIONS:
  -e <file>          External input file format (TGA, PPM, PGM, PAM) to encode
  -out <file>        Output file path
  -dir <directory>   Output directory

METADATA & DEDUPLICATION:
  * All non-essential PNG metadata is stripped during optimization.
  * A 'tEXt' chunk with key 'optipng-rs' is injected to track optimization state:
  - Line 1: User-specified command options (if present).
  - Line 2: Winning trial settings (-zc -zm -zs -f) or '-o0'."#
    );
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

fn zc_to_zopfli_iterations(zc: i32) -> i32 {
    match zc {
        1 => 1,
        2 => 3,
        3 => 5,
        4 => 10,
        5 => 15,  // Zöpfli default
        6 => 30,
        7 => 50,
        8 => 100,
        9 => 500, // Maximum squeeze
        _ => 15,
    }
}

fn get_zopfli_opt_combinations(level: u8) -> (Vec<i32>, Vec<u8>) {
    match level {
        1 => (vec![1], vec![3]),
        2 => (vec![2], vec![5]),
        3 => (vec![3], vec![5]),
        4 => (vec![4], vec![5]),
        5 => (vec![5], vec![5]),
        6 => (vec![6], vec![5]),
        7 => (vec![7], vec![3, 5]),
        _ => get_zopfli_opt_combinations(2),
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

fn get_opt_combinations(level: u8, color_type: u8, bit_depth: u8) -> (Vec<i32>, Vec<i32>, Vec<i32>, Vec<u8>) {
    match level {
        0 | 1 => {
            // OptiPNG Heuristic: Filter 0 for Palette or sub-8-bit; Adaptive (5) for Truecolor/Grayscale
            let filter = if color_type == 3 || bit_depth < 8 {
                vec![0]
            } else {
                vec![5]
            };
            (vec![9], vec![8], vec![0], filter)
        }
        2 => (vec![9], vec![8], vec![0, 1, 2, 3], vec![0, 5]),
        3 => (vec![9], vec![8, 9], vec![0, 1, 2, 3], vec![0, 5]),
        4 => (vec![9], vec![8], vec![0, 1, 2, 3], vec![0, 1, 2, 3, 4, 5]),
        5 => (vec![9], vec![8, 9], vec![0, 1, 2, 3], vec![0, 1, 2, 3, 4, 5]),
        6 => ((1..=9).collect(), vec![8], vec![0, 1, 2, 3], vec![0, 1, 2, 3, 4, 5]),
        7 => ((1..=9).collect(), vec![8, 9], vec![0, 1, 2, 3], vec![0, 1, 2, 3, 4, 5]),
        _ => get_opt_combinations(2, color_type, bit_depth),
    }
}

fn parse_args() -> CliArgs {
    let raw_args: Vec<String> = env::args().skip(1).collect();
    let mut cli = CliArgs {
        files: Vec::new(),
        external_input: None,
        opt_level: 2,
        mt: 0,
        zi: 1,
        zc: None,
        zm: None,
        zs: None,
        f: None,
        backup: false,
        simulate: false,
        quiet: false,
        nc: false,
        nb: false,
        np: false,
        nx: false,
        nz: false,
        out_file: None,
        out_dir: None,
        show_help: false,
        force_trials: false,
        force_reenc: false,
        cmd_options: String::new(),
    };

    let mut opt_tokens: Vec<String> = Vec::new();
    let mut i = 0;

    while i < raw_args.len() {
        let arg = &raw_args[i];

        if arg == "-h" || arg == "--help" {
            cli.show_help = true;
            return cli;
        }

        if arg == "-e" {
            opt_tokens.push(arg.clone());
            i += 1;
            if i < raw_args.len() {
                cli.external_input = Some(raw_args[i].clone());
                i += 1;
            } else {
                eprintln!("Error: Option -e requires an input file argument.");
                cli.show_help = true;
                return cli;
            }
            continue;
        }

        if arg == "-out" {
            opt_tokens.push(arg.clone());
            i += 1;
            if i < raw_args.len() {
                cli.out_file = Some(raw_args[i].clone());
                i += 1;
            }
            continue;
        }

        if arg == "-dir" {
            opt_tokens.push(arg.clone());
            i += 1;
            if i < raw_args.len() {
                cli.out_dir = Some(raw_args[i].clone());
                i += 1;
            }
            continue;
        }

        if arg == "-ft" {
            cli.force_trials = true;
            opt_tokens.push(arg.clone());
            i += 1;
            continue;
        }

        if arg == "-force" {
            cli.force_reenc = true;
            opt_tokens.push(arg.clone());
            i += 1;
            continue;
        }

        if arg.starts_with("-o") && arg.len() > 2 && arg[2..].chars().all(|c| c.is_ascii_digit()) {
            let level: u8 = arg[2..].parse().unwrap_or(2);
            cli.opt_level = level.min(7);
            opt_tokens.push(arg.clone());
            i += 1;
            continue;
        }

        if arg.starts_with("-mt") && arg.len() > 3 && arg[3..].chars().all(|c| c.is_ascii_digit()) {
            cli.mt = arg[3..].parse().unwrap_or(0);
            opt_tokens.push(arg.clone());
            i += 1;
            continue;
        }

        if arg.starts_with("-zi") {
            let val_str = if arg.len() > 3 {
                opt_tokens.push(arg.clone());
                Some(arg[3..].to_string())
            } else {
                opt_tokens.push(arg.clone());
                i += 1;
                if i < raw_args.len() {
                    opt_tokens.push(raw_args[i].clone());
                    Some(raw_args[i].clone())
                } else {
                    None
                }
            };

            if let Some(v) = val_str {
                if v.contains(',') || v.contains('-') {
                    eprintln!("Error: Options like -zi1,2 or -zi1-2 are not allowed. Select either -zi1 or -zi2.");
                    std::process::exit(1);
                }
                match v.parse::<u8>() {
                    Ok(1) => cli.zi = 1,
                    Ok(2) => cli.zi = 2,
                    _ => {
                        eprintln!("Error: Invalid value for -zi. Supported values are 1 (zlib) or 2 (Zöpfli).");
                        std::process::exit(1);
                    }
                }
            }
            i += 1;
            continue;
        }

        if arg.starts_with("-zc") {
            let val_str = if arg.len() > 3 {
                opt_tokens.push(arg.clone());
                Some(arg[3..].to_string())
            } else {
                opt_tokens.push(arg.clone());
                i += 1;
                if i < raw_args.len() {
                    opt_tokens.push(raw_args[i].clone());
                    Some(raw_args[i].clone())
                } else {
                    None
                }
            };
            if let Some(v) = val_str {
                let mut parsed = parse_ranges_i32(&v);
                cli.zc.get_or_insert_with(Vec::new).append(&mut parsed);
            }
            i += 1;
            continue;
        }

        if arg.starts_with("-zm") {
            let val_str = if arg.len() > 3 {
                opt_tokens.push(arg.clone());
                Some(arg[3..].to_string())
            } else {
                opt_tokens.push(arg.clone());
                i += 1;
                if i < raw_args.len() {
                    opt_tokens.push(raw_args[i].clone());
                    Some(raw_args[i].clone())
                } else {
                    None
                }
            };
            if let Some(v) = val_str {
                let mut parsed = parse_ranges_i32(&v);
                cli.zm.get_or_insert_with(Vec::new).append(&mut parsed);
            }
            i += 1;
            continue;
        }

        if arg.starts_with("-zs") {
            let val_str = if arg.len() > 3 {
                opt_tokens.push(arg.clone());
                Some(arg[3..].to_string())
            } else {
                opt_tokens.push(arg.clone());
                i += 1;
                if i < raw_args.len() {
                    opt_tokens.push(raw_args[i].clone());
                    Some(raw_args[i].clone())
                } else {
                    None
                }
            };
            if let Some(v) = val_str {
                let mut parsed = parse_ranges_i32(&v);
                cli.zs.get_or_insert_with(Vec::new).append(&mut parsed);
            }
            i += 1;
            continue;
        }

        if arg.starts_with("-f") {
            let val_str = if arg.len() > 2 {
                opt_tokens.push(arg.clone());
                Some(arg[2..].to_string())
            } else {
                opt_tokens.push(arg.clone());
                i += 1;
                if i < raw_args.len() {
                    opt_tokens.push(raw_args[i].clone());
                    Some(raw_args[i].clone())
                } else {
                    None
                }
            };
            if let Some(v) = val_str {
                let mut parsed = parse_ranges_u8(&v);
                cli.f.get_or_insert_with(Vec::new).append(&mut parsed);
            }
            i += 1;
            continue;
        }

        match arg.as_str() {
            "-o" => {
                opt_tokens.push(arg.clone());
                i += 1;
                if i < raw_args.len() {
                    opt_tokens.push(raw_args[i].clone());
                    let level: u8 = raw_args[i].parse().unwrap_or(2);
                    cli.opt_level = level.min(7);
                    i += 1;
                }
            }
            "-mt" => {
                opt_tokens.push(arg.clone());
                i += 1;
                if i < raw_args.len() {
                    opt_tokens.push(raw_args[i].clone());
                    cli.mt = raw_args[i].parse().unwrap_or(0);
                    i += 1;
                }
            }
            "-backup" | "-keep" => { cli.backup = true; opt_tokens.push(arg.clone()); i += 1; }
            "-simulate" => { cli.simulate = true; opt_tokens.push(arg.clone()); i += 1; }
            "-quiet" | "-silent" => { cli.quiet = true; opt_tokens.push(arg.clone()); i += 1; }
            "-nc" => { cli.nc = true; opt_tokens.push(arg.clone()); i += 1; }
            "-nb" => { cli.nb = true; opt_tokens.push(arg.clone()); i += 1; }
            "-np" => { cli.np = true; opt_tokens.push(arg.clone()); i += 1; }
            "-nx" => { cli.nx = true; opt_tokens.push(arg.clone()); i += 1; }
            "-nz" => { cli.nz = true; opt_tokens.push(arg.clone()); i += 1; }
            "--" => {
                opt_tokens.push(arg.clone());
                i += 1;
                cli.files.extend(raw_args[i..].iter().cloned());
                break;
            }
            _ => {
                if arg.starts_with('-') {
                    opt_tokens.push(arg.clone());
                } else {
                    cli.files.push(arg.clone());
                }
                i += 1;
            }
        }
    }

    // Implement level 0
    if cli.opt_level == 0 {
        cli.nx = true;
        cli.nz = true;
    }

    cli.cmd_options = opt_tokens.join(" ");

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

type PngWriteCallback = unsafe extern "C" fn(*mut c_void, *const u8, usize) -> usize;

unsafe extern "C" fn buffer_write_cb(user_data: *mut c_void, buf: *const u8, len: usize) -> usize {
    if !user_data.is_null() && !buf.is_null() {
        let vec = unsafe { &mut *(user_data as *mut Vec<u8>) };
        let slice = unsafe { std::slice::from_raw_parts(buf, len) };
        vec.extend_from_slice(slice);
    }
    len
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
        let mut stride: usize;
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

        let mut out_color_type = color_type;
        let mut out_bit_depth = bit_depth;
        let mut final_palette: Option<Vec<u8>> = None;
        let mut final_trns: Option<Vec<u8>> = None;

        if color_type == 3 {
            out_bit_depth = 8;
            out_color_type = if stride == (width as usize * 4) { 6 } else { 2 };
        }

        // =========================================================================
        // STEP 2: OPTIPNG COLOR & BIT-DEPTH REDUCTION PIPELINE
        // =========================================================================

        if !cli.nx {
            if !cli.nb {
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
            }

            if !cli.nc {
                // 2b. Check opacity & reduce color type if 100% opaque
                if out_color_type == 4 || out_color_type == 6 {
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

                // 2c. Truecolor to Grayscale Reduction (RGB/RGBA -> Gray/GrayA)
                if out_color_type == 2 || out_color_type == 6 {
                    let channels = if out_color_type == 6 { 4 } else { 3 };
                    let sample_bytes = (out_bit_depth / 8) as usize;
                    let px_bytes = channels * sample_bytes;

                    let mut is_grayscale = true;

                    for px in raw_pixels.chunks_exact(px_bytes) {
                        if sample_bytes == 1 {
                            // 8-bit: Check if R == G and G == B
                            if px[0] != px[1] || px[1] != px[2] {
                                is_grayscale = false;
                                break;
                            }
                        } else {
                            // 16-bit: Compare sample words
                            if px[0..2] != px[2..4] || px[2..4] != px[4..6] {
                                is_grayscale = false;
                                break;
                            }
                        }
                    }

                    if is_grayscale {
                        let num_pixels = raw_pixels.len() / px_bytes;
                        let mut write_idx = 0;

                        for i in 0..num_pixels {
                            let px_start = i * px_bytes;

                            // Retain Red channel sample as Gray
                            raw_pixels.copy_within(px_start..px_start + sample_bytes, write_idx);
                            write_idx += sample_bytes;

                            // Retain Alpha channel sample if present
                            if out_color_type == 6 {
                                let alpha_start = px_start + 3 * sample_bytes;
                                raw_pixels.copy_within(alpha_start..alpha_start + sample_bytes, write_idx);
                                write_idx += sample_bytes;
                            }
                        }

                        raw_pixels.truncate(write_idx);
                        raw_pixels.shrink_to_fit();

                        let old_color_type = out_color_type;
                        out_color_type = if out_color_type == 6 { 4 } else { 0 };

                        if !cli.quiet {
                            println!(
                                "  (i) Reducing color type from {} to {} (R==G==B across all pixels)",
                                     color_type_name(old_color_type),
                                     color_type_name(out_color_type)
                            );
                        }
                    }
                }

                // 2d. Reduction to Indexed / Palette (Unique Colors <= 256, down to 1, 2, or 4-bit)
                if !cli.np && out_bit_depth == 8 && (out_color_type == 2 || out_color_type == 6) {
                    use std::collections::{HashMap, HashSet};

                    let px_bytes = match out_color_type {
                        0 => 1, // Gray
                        2 => 3, // RGB
                        4 => 2, // GrayA
                        6 => 4, // RGBA
                        _ => 0,
                    };

                    if px_bytes > 0 {
                        // 1. Collect unique pixel colors (preserves Scanline / First Appearance order)
                        let mut unique_colors: Vec<Vec<u8>> = Vec::new();
                        let mut seen = HashSet::new();

                        for px in raw_pixels.chunks_exact(px_bytes) {
                            if seen.insert(px) {
                                if unique_colors.len() >= 256 {
                                    break;
                                }
                                unique_colors.push(px.to_vec());
                            }
                        }

                        // Process palette reduction only if total unique colors <= 256
                        if unique_colors.len() <= 256 && !unique_colors.is_empty() && seen.len() <= 256 {
                            let extract_rgba = |px: &[u8]| -> (u8, u8, u8, u8) {
                                match out_color_type {
                                    0 => (px[0], px[0], px[0], 255),
                                    2 => (px[0], px[1], px[2], 255),
                                    4 => (px[0], px[0], px[0], px[1]),
                                    6 => (px[0], px[1], px[2], px[3]),
                                    _ => (0, 0, 0, 255),
                                }
                            };

                            // 2. Sort palette to optimize for DEFLATE + tRNS chunk trimming:
                            // - Non-opaque colors (a < 255) are moved to the front sorted by alpha.
                            // - Opaque colors (a == 255) maintain Scanline First-Appearance order.
                            unique_colors.sort_by_key(|px| {
                                let (_, _, _, a) = extract_rgba(px);
                                (a == 255, a)
                            });

                            // 3. Build lookup map and PNG PLTE / tRNS structures
                            let mut palette_map: HashMap<Vec<u8>, u8> = HashMap::with_capacity(unique_colors.len());
                            let mut palette_rgb: Vec<u8> = Vec::with_capacity(unique_colors.len() * 3);
                            let mut palette_trns: Vec<u8> = Vec::with_capacity(unique_colors.len());
                            let mut has_trns = false;

                            for (idx, px) in unique_colors.iter().enumerate() {
                                palette_map.insert(px.clone(), idx as u8);

                                let (r, g, b, a) = extract_rgba(px);
                                palette_rgb.extend_from_slice(&[r, g, b]);
                                palette_trns.push(a);
                                if a < 255 {
                                    has_trns = true;
                                }
                            }

                            // 4. Re-index image pixel data to sorted 8-bit palette indices
                            let num_pixels = raw_pixels.len() / px_bytes;
                            for i in 0..num_pixels {
                                let px_start = i * px_bytes;
                                let idx = *palette_map.get(&raw_pixels[px_start..px_start + px_bytes]).unwrap();
                                raw_pixels[i] = idx;
                            }
                            raw_pixels.truncate(num_pixels);

                            // 5. Determine optimal sub-8-bit depth (1, 2, 4, or 8 bits)
                            let target_bit_depth: u8 = match unique_colors.len() {
                                0..=2 => 1,
                                3..=4 => 2,
                                5..=16 => 4,
                                _ => 8,
                            };

                            // 6. Pack sub-byte pixels row-by-row if bit depth < 8
                            if target_bit_depth < 8 {
                                out_bit_depth = target_bit_depth;
                                let w = width as usize;
                                let h = height as usize;
                                let mut packed_pixels = Vec::with_capacity(h * ((w * (target_bit_depth as usize) + 7) / 8));

                                for r in 0..h {
                                    let row_start = r * w;
                                    let row_end = (row_start + w).min(num_pixels);
                                    let row_pixels = &raw_pixels[row_start..row_end];

                                    let mut current_byte = 0u8;
                                    let mut bit_pos = 0; // Filled bits in current byte

                                    for &idx in row_pixels {
                                        let shift = 8 - bit_pos - target_bit_depth;
                                        current_byte |= (idx & ((1 << target_bit_depth) - 1)) << shift;
                                        bit_pos += target_bit_depth;

                                        if bit_pos == 8 {
                                            packed_pixels.push(current_byte);
                                            current_byte = 0;
                                            bit_pos = 0;
                                        }
                                    }

                                    // Pad trailing partial byte for current scanline
                                    if bit_pos > 0 {
                                        packed_pixels.push(current_byte);
                                    }
                                }
                                raw_pixels = packed_pixels;
                            }

                            raw_pixels.shrink_to_fit();

                            // 7. Trim trailing 255 alpha values from tRNS chunk per PNG spec
                            if !has_trns {
                                palette_trns.clear();
                            } else {
                                while palette_trns.last() == Some(&255) {
                                    palette_trns.pop();
                                }
                            }

                            let old_color_type = out_color_type;
                            out_color_type = 3; // Palette

                            // Save final palette data
                            final_palette = Some(palette_rgb);
                            if !palette_trns.is_empty() {
                                final_trns = Some(palette_trns);
                            }

                            if !cli.quiet && color_type != 3 {
                                println!(
                                    "  (i) Reducing color type from {} to {} ({} bit, Color count: {})",
                                         color_type_name(old_color_type),
                                         color_type_name(out_color_type),
                                         out_bit_depth,
                                         palette_map.len()
                                );
                            }
                        }
                    }
                }
            }
        }

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

        let mut global_best_config: Option<TrialConfig> = None;
        let mut global_best_size: usize = usize::MAX;
        let mut global_best_bytes: Option<Vec<u8>> = None;
        let total_trials: usize;

        if cli.nz {
            // Fast Path: 0 trials. Bypass worker threads completely.
            if !cli.quiet {
                println!("  (-nz) IDAT recoding disabled. Skipping trials...");
            }
            global_best_config = Some(TrialConfig {
                zc: zc_list[0],
                zm: zm_list[0],
                zs: zs_list[0],
                f: f_list[0],
            });
            global_best_size = orig_idat_size;
            total_trials = 0;
        } else {
            // Run standard parallel trial queue...
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

            trials.sort_by_key(|t| Reverse((t.zc, zs_difficulty(t.zs), t.zm, t.f)));
            total_trials = trials.len();
            let trials = Arc::new(trials);

            if !cli.quiet {
                println!("  Starting Trials ..... : {} trials, with {} parallel threads", total_trials, cli.mt);
            }

            // Parallel encoding trials
            let mut handles = Vec::new();
            let next_trial_index = Arc::new(AtomicUsize::new(0));
            let completed_trials = Arc::new(AtomicUsize::new(0));
            let completed_scanlines = Arc::new(AtomicUsize::new(0));
            let stdout_lock = Arc::new(std::sync::Mutex::new(()));

            let total_rows = height as usize;
            let row_bytes = if total_rows > 0 { image_data.as_ref().unwrap().len() / total_rows } else { 0 };
            let total_scanlines = total_trials * total_rows;

            let cmd_options = cli.cmd_options.clone();
            let opt_level = cli.opt_level;
            let is_zopfli = cli.zi == 2;

            let start_trials = Instant::now();

            for _ in 0..cli.mt {
                let trials_clone = Arc::clone(&trials);
                let image_data_ref = Arc::clone(image_data.as_ref().unwrap());
                let palette_clone = shared_palette.clone();
                let trns_clone = shared_trns.clone();
                let next_idx = Arc::clone(&next_trial_index);
                let completed = Arc::clone(&completed_trials);
                let scanlines_acc = Arc::clone(&completed_scanlines);
                let lock = Arc::clone(&stdout_lock);
                let quiet = cli.quiet;
                let cmd_opts = cmd_options.clone();

                handles.push(thread::spawn(move || {
                    let mut best_size = usize::MAX;
                    let mut best_config = None;
                    let mut best_png_bytes: Option<Vec<u8>> = None;
                    let chunk_rows = 250;

                    loop {
                        let idx = next_idx.fetch_add(1, Ordering::Relaxed);
                        if idx >= trials_clone.len() {
                            break;
                        }

                        let trial = &trials_clone[idx];
                        let mut trial_output_buffer = Vec::new();
                        let mut dummy_written: usize = 0;

                        let z_level = if is_zopfli {
                            zc_to_zopfli_iterations(trial.zc)
                        } else {
                            trial.zc
                        };

                        let options = ZlibOptions {
                            z_implementation: cli.zi,
                            level: z_level,
                            strategy: trial.zs,
                            window_bits: 15,
                            mem_level: trial.zm,
                            max_idat_size: 32768,
                            expected_idat_size: 0,
                        };

                        let (pal_ptr, pal_len) = match &palette_clone {
                            Some(pal) => (pal.as_ptr(), pal.len()),
                                           None => (std::ptr::null(), 0),
                        };

                        let (trns_ptr, trns_len) = match &trns_clone {
                            Some(trns) => (trns.as_ptr(), trns.len()),
                                           None => (std::ptr::null(), 0),
                        };

                        // Prepare metadata chunk for Zöpfli buffer retention
                        let opt_info = if opt_level == 0 {
                            "-o0".to_string()
                        } else if cmd_opts.is_empty() && cli.zi == 1 {
                            format!("{}{}{}{}", trial.zc, trial.zm, trial.zs, trial.f)
                        } else if !cmd_opts.is_empty() && cli.zi == 1 {
                            format!("{}\n{}{}{}{}", cmd_opts, trial.zc, trial.zm, trial.zs, trial.f)
                        } else if !cmd_opts.is_empty() && cli.zi == 2 {
                            format!("{}\n{}{}", cmd_opts, trial.zc, trial.f)
                        } else {
                            "".to_string()
                        };

                        let c_key = CString::new("optipng-rs").unwrap();
                        let c_val = CString::new(opt_info).unwrap();
                        let text_keys = [c_key.as_ptr()];
                        let text_vals = [c_val.as_ptr()];

                        let (write_cb_fn, user_data_ptr, text_k_ptr, text_v_ptr, text_cnt) = if is_zopfli {
                            (
                                buffer_write_cb as PngWriteCallback,
                             &mut trial_output_buffer as *mut _ as *mut c_void,
                             text_keys.as_ptr(),
                             text_vals.as_ptr(),
                             1usize,
                            )
                        } else {
                            (
                                counter_write_cb as PngWriteCallback,
                             &mut dummy_written as *mut _ as *mut c_void,
                             std::ptr::null(),
                             std::ptr::null(),
                             0usize,
                            )
                        };

                        let enc = open_png_encode_stream(
                            write_cb_fn,
                            user_data_ptr,
                            width,
                            height,
                            out_bit_depth,
                            out_color_type,
                            trial.f,
                            pal_ptr,
                            pal_len,
                            trns_ptr,
                            trns_len,
                            text_k_ptr,
                            text_v_ptr,
                            text_cnt,
                            options,
                        );

                        if !enc.is_null() {
                            let mut encoded_rows = 0usize;
                            {
                                let img = Arc::clone(&image_data_ref);
                                while encoded_rows < total_rows {
                                    let rows_to_encode = (total_rows - encoded_rows).min(chunk_rows);
                                    let offset = encoded_rows * row_bytes;
                                    let ptr = unsafe { img.as_ptr().add(offset) };

                                    encode_scanlines(enc, ptr, rows_to_encode as u32);
                                    encoded_rows += rows_to_encode;

                                    // Report percentage only for zlib; display trial count for Zöpfli
                                    if !quiet && !is_zopfli && total_scanlines > 0 {
                                        let cur_scanlines = scanlines_acc.fetch_add(rows_to_encode, Ordering::Relaxed) + rows_to_encode;
                                        let done = completed.load(Ordering::Relaxed);
                                        let progress_pct = ((cur_scanlines as f64 / total_scanlines as f64) * 100.0).min(100.0);
                                        if let Ok(_guard) = lock.try_lock() {
                                            print!("\r  Trial progress ...... : {:.2} percent, {} trials done", progress_pct, done);
                                            let _ = io::stdout().flush();
                                        }
                                    }
                                }
                            }

                            let trial_idat_size = close_png_encode_get_idat_size(enc);

                            if encoded_rows == total_rows && trial_idat_size < best_size && trial_idat_size > 0 {
                                best_size = trial_idat_size;
                                best_config = Some(trial.clone());
                                if is_zopfli {
                                    best_png_bytes = Some(trial_output_buffer);
                                }
                            }
                        }

                        let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
                        if !quiet {
                            if is_zopfli {
                                if let Ok(_guard) = lock.try_lock() {
                                    print!("\r  Trial progress ...... : {}/{} trials completed", done, total_trials);
                                    let _ = io::stdout().flush();
                                }
                            } else if total_scanlines > 0 {
                                let cur_scanlines = scanlines_acc.load(Ordering::Relaxed);
                                let progress_pct = ((cur_scanlines as f64 / total_scanlines as f64) * 100.0).min(100.0);
                                if let Ok(_guard) = lock.try_lock() {
                                    print!("\r  Trial progress ...... : {:.2} percent, {} trials done", progress_pct, done);
                                    let _ = io::stdout().flush();
                                }
                            }
                        }
                    }
                    (best_size, best_config, best_png_bytes)
                }));
            }

            if cli.zi == 2 {
                image_data = None;
            }

            for handle in handles {
                if let Ok((size, Some(config), maybe_bytes)) = handle.join() {
                    if size < global_best_size {
                        global_best_size = size;
                        global_best_config = Some(config);
                        if is_zopfli {
                            global_best_bytes = maybe_bytes;
                        }
                    }
                }
            }

            let trial_duration = start_trials.elapsed();
            if !cli.quiet {
                println!("\r  Trial process took .. : {}                       ", format_duration(trial_duration));
            }
        }

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
