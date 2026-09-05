/*****************************************************************
* optipng-rs: RAW PNG CHUNK COPY & RE-PARTITIONING FOR -o0 / -nz *
*****************************************************************/

use std::fs;
use std::path::Path;
use std::io::Write;

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

/* Copies existing compressed IDAT bytes from an existing PNG without zlib re-encoding,
 * coalesces IDAT chunks, and inserts/updates the `tEXt` metadata chunk. */
pub fn copy_png_idat_and_add_text(
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