/***************************************************************
* optipng-rs: Multi-threaded compression trials and heuristics *
***************************************************************/

use std::cmp::Reverse;
use std::ffi::{c_void, CString};
use std::io::{self, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use pngstreamenc::{
    close_png_encode_get_idat_size, encode_scanlines, open_png_encode_stream, ZlibOptions,
};

use crate::models::{CliArgs, TrialConfig};
use crate::utils::{buffer_write_cb, counter_write_cb, format_duration, PngWriteCallback};

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

pub fn zs_difficulty(zs: i32) -> u8 {
    match zs {
        1 => 4, // Filtered (Hardest)
        0 => 3, // Default
        2 => 2, // Huffman-only
        3 => 1, // RLE (Easiest)
        _ => 0,
    }
}

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

pub fn get_opt_combinations(level: u8, color_type: u8, bit_depth: u8) -> (Vec<i32>, Vec<i32>, Vec<i32>, Vec<u8>) {
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

pub fn run_parallel_trials(
    cli: &CliArgs,
    image_data: &Arc<Vec<u8>>,
    shared_palette: &Option<Arc<Vec<u8>>>,
    shared_trns: &Option<Arc<Vec<u8>>>,
    width: u32,
    height: u32,
    out_bit_depth: u8,
    out_color_type: u8,
    zc_list: &[i32],
    zm_list: &[i32],
    zs_list: &[i32],
    f_list: &[u8],
) -> (Option<TrialConfig>, usize, Option<Vec<u8>>, usize) {
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
    let total_trials = trials.len();
    let trials = Arc::new(trials);

    if !cli.quiet {
        println!("  Starting Trials ..... : {} trials, with {} parallel threads", total_trials, cli.mt);
    }

    let mut handles = Vec::new();
    let next_trial_index = Arc::new(AtomicUsize::new(0));
    let completed_trials = Arc::new(AtomicUsize::new(0));
    let completed_scanlines = Arc::new(AtomicUsize::new(0));
    let stdout_lock = Arc::new(std::sync::Mutex::new(()));

    let total_rows = height as usize;
    let row_bytes = if total_rows > 0 { image_data.len() / total_rows } else { 0 };
    let total_scanlines = total_trials * total_rows;

    let cmd_options = cli.cmd_options.clone();
    let opt_level = cli.opt_level;
    let is_zopfli = cli.zi == 2;
    let zi = cli.zi;
    let mt = cli.mt;
    let quiet = cli.quiet;

    let start_trials = Instant::now();

    for _ in 0..mt {
        let trials_clone = Arc::clone(&trials);
        let image_data_ref = Arc::clone(image_data);
        let palette_clone = shared_palette.clone();
        let trns_clone = shared_trns.clone();
        let next_idx = Arc::clone(&next_trial_index);
        let completed = Arc::clone(&completed_trials);
        let scanlines_acc = Arc::clone(&completed_scanlines);
        let lock = Arc::clone(&stdout_lock);
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
                    z_implementation: zi,
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

    let mut global_best_config: Option<TrialConfig> = None;
    let mut global_best_size: usize = usize::MAX;
    let mut global_best_bytes: Option<Vec<u8>> = None;

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

    (global_best_config, global_best_size, global_best_bytes, total_trials)
}