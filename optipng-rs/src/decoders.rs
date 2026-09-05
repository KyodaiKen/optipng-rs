/***************************************************************
* optipng-rs: EXTERNAL FORMAT DECODERS (TGA / PPM / PGM / PAM) *
***************************************************************/

use std::fs;
use crate::models::LoadedImage;

// Decodes Targa (.tga) images (Uncompressed and RLE TrueColor/Grayscale)
pub fn decode_tga(data: &[u8]) -> Result<LoadedImage, String> {
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

// Decodes Netpbm images (.ppm, .pgm, .pam)
pub fn decode_netpbm(data: &[u8]) -> Result<LoadedImage, String> {
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

// Dispatches image loading based on file headers / extensions
pub fn load_external_image(file_path: &str) -> Result<LoadedImage, String> {
    let data = fs::read(file_path).map_err(|e| format!("Failed to read file {:?}: {}", file_path, e))?;

    if data.starts_with(b"P5") || data.starts_with(b"P6") || data.starts_with(b"P7") {
        decode_netpbm(&data)
    } else {
        decode_tga(&data)
    }
}
