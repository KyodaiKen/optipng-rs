/***********************************************************
 * optipng-rs: OPTIPNG COLOR & BIT-DEPTH REDUCTION PIPELINE *
 ***********************************************************/

use std::collections::{HashMap, HashSet};
use crate::models::CliArgs;
use crate::utils::color_type_name;

pub struct ReductionResult {
    pub out_color_type: u8,
    pub out_bit_depth: u8,
    pub final_palette: Option<Vec<u8>>,
    pub final_trns: Option<Vec<u8>>,
}

pub fn reduce_image(
    cli: &CliArgs,
    width: u32,
    height: u32,
    color_type: u8,
    bit_depth: u8,
    mut stride: usize,
    raw_pixels: &mut Vec<u8>,
) -> ReductionResult {
    let mut out_color_type = color_type;
    let mut out_bit_depth = bit_depth;
    let mut final_palette: Option<Vec<u8>> = None;
    let mut final_trns: Option<Vec<u8>> = None;

    if color_type == 3 {
        out_bit_depth = 8;
        out_color_type = if stride == (width as usize * 4) { 6 } else { 2 };
    }

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
                            *raw_pixels = packed_pixels;
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

    ReductionResult {
        out_color_type,
        out_bit_depth,
        final_palette,
        final_trns,
    }
}