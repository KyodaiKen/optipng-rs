/***************************************************************
 * optipng-rs: Compression trial routines and configuration    *
 ***************************************************************/

use std::cmp::Reverse;
use std::ffi::{c_void, CString};
use std::sync::Arc;

use pngstreamenc::{
    close_png_encode_get_idat_size, encode_scanlines, open_png_encode_stream, ZlibOptions,
};

use crate::models::TrialConfig;
use crate::utils::{buffer_write_cb, counter_write_cb, PngWriteCallback};

/* Maps zlib compression levels (-zc) to Zöpfli iteration counts. */
pub fn zc_to_zopfli_iterations(zc: i32) -> i32 {
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

/* Returns difficulty ranking for zlib strategies for trial ordering. */
pub fn zs_difficulty(zs: i32) -> u8 {
    match zs {
        1 => 4, // Filtered (Hardest)
        0 => 3, // Default
        2 => 2, // Huffman-only
        3 => 1, // RLE (Easiest)
        _ => 0,
    }
}

/* Returns preset Zöpfli optimization combinations based on optimization level. */
pub fn get_zopfli_opt_combinations(level: u8) -> (Vec<i32>, Vec<u8>) {
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

/* Returns preset zlib optimization parameter combinations based on level and image type. */
pub fn get_opt_combinations(level: u8, color_type: u8, bit_depth: u8) -> (Vec<i32>, Vec<i32>, Vec<i32>, Vec<u8>) {
    match level {
        0 | 1 => {
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

/* Builds and sorts the complete list of trials to attempt for an image configuration. */
pub fn build_trial_list(
    zc_list: &[i32],
    zm_list: &[i32],
    zs_list: &[i32],
    f_list: &[u8],
) -> Vec<TrialConfig> {
    let mut trials = Vec::new();
    for &zc in zc_list {
        for &zm in zm_list {
            for &zs in zs_list {
                if (zs == 2 || zs == 3) && zc > 1 {
                    continue;
                }
                for &f in f_list {
                    trials.push(TrialConfig { zc, zm, zs, f });
                }
            }
        }
    }
    trials.sort_by_key(|t| Reverse((t.zc, zs_difficulty(t.zs), t.zm, t.f)));
    trials
}

/* Executes a single compression trial on raw image data and returns IDAT size & optional stream. */
pub fn run_single_trial(
    image_data: &Arc<Vec<u8>>,
    shared_palette: &Option<Arc<Vec<u8>>>,
    shared_trns: &Option<Arc<Vec<u8>>>,
    width: u32,
    height: u32,
    out_bit_depth: u8,
    out_color_type: u8,
    trial: &TrialConfig,
    opt_level: u8,
    cmd_opts: &str,
    zi: u8,
    on_scanline_chunk: Option<&dyn Fn(usize)>,
) -> (usize, Option<Vec<u8>>) {
    let total_rows = height as usize;
    let row_bytes = if total_rows > 0 { image_data.len() / total_rows } else { 0 };
    let chunk_rows = 250;
    let is_zopfli = zi == 2;

    let mut trial_output_buffer = Vec::new();
    let mut dummy_written: usize = 0;

    let z_level = if is_zopfli {
        zc_to_zopfli_iterations(trial.zc)
    } else {
        trial.zc
    };

    let options = ZlibOptions {
        z_implementation: zi,
        level: z_level,
        strategy: trial.zs,
        window_bits: 15,
        mem_level: trial.zm,
        max_idat_size: 32768,
        expected_idat_size: 0,
    };

    let (pal_ptr, pal_len) = match shared_palette {
        Some(pal) => (pal.as_ptr(), pal.len()),
        None => (std::ptr::null(), 0),
    };

    let (trns_ptr, trns_len) = match shared_trns {
        Some(trns) => (trns.as_ptr(), trns.len()),
        None => (std::ptr::null(), 0),
    };

    let opt_info = if opt_level == 0 {
        "-o0".to_string()
    } else if cmd_opts.is_empty() && zi == 1 {
        format!("{}{}{}{}", trial.zc, trial.zm, trial.zs, trial.f)
    } else if !cmd_opts.is_empty() && zi == 1 {
        format!("{}\n{}{}{}{}", cmd_opts, trial.zc, trial.zm, trial.zs, trial.f)
    } else if !cmd_opts.is_empty() && zi == 2 {
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

    if enc.is_null() {
        return (usize::MAX, None);
    }

    let mut encoded_rows = 0usize;
    while encoded_rows < total_rows {
        let rows_to_encode = (total_rows - encoded_rows).min(chunk_rows);
        let offset = encoded_rows * row_bytes;
        let ptr = unsafe { image_data.as_ptr().add(offset) };

        encode_scanlines(enc, ptr, rows_to_encode as u32);
        encoded_rows += rows_to_encode;

        if let Some(cb) = on_scanline_chunk {
            cb(rows_to_encode);
        }
    }

    let trial_idat_size = close_png_encode_get_idat_size(enc);

    if encoded_rows == total_rows && trial_idat_size > 0 {
        let bytes_opt = if is_zopfli { Some(trial_output_buffer) } else { None };
        (trial_idat_size, bytes_opt)
    } else {
        (usize::MAX, None)
    }
}