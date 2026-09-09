/***************************************************************
 * optipng-rs: Main multi-threaded execution and progress UI   *
 ***************************************************************/

mod models;
mod utils;
mod cli;
mod decoders;
mod chunk_parser;
mod trials;
mod reduction;

use std::collections::HashSet;
use std::ffi::CString;
use std::fs;
use std::io::{self, Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Instant;
use sysinfo::System;

use pngstreamdec::{
    close_png, decode_scanlines, open_png, png_get_idat_size, png_set_count_idat_size,
};
use pngstreamenc::{close_png_encode, encode_scanlines, open_png_encode, ZlibOptions};

use crate::chunk_parser::*;
use crate::cli::*;
use crate::decoders::*;
use crate::models::{CliArgs, FileState, FileTask, ReductionResult, TrialConfig};
use crate::reduction::*;
use crate::trials::*;
use crate::utils::*;

/// Validates PNG magic bytes and checks if the file was already optimized by optipng-rs.
/// Returns `(is_valid_png, is_already_optimized)`.
fn check_png_file(path: &Path, force_trials: bool) -> (bool, bool) {
    let mut file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return (false, false),
    };

    let mut header = [0u8; 8];
    if file.read_exact(&mut header).is_err() {
        return (false, false);
    }

    // Verify 8-byte PNG magic number
    if header != *b"\x89PNG\r\n\x1a\n" {
        return (false, false);
    }

    if force_trials {
        return (true, false);
    }

    let mut length_buf = [0u8; 4];
    let mut type_buf = [0u8; 4];

    while file.read_exact(&mut length_buf).is_ok() && file.read_exact(&mut type_buf).is_ok() {
        let len = u32::from_be_bytes(length_buf) as u64;
        if &type_buf == b"tEXt" {
            let mut data = vec![0u8; len as usize];
            if file.read_exact(&mut data).is_ok() {
                if let Some(null_pos) = data.iter().position(|&b| b == 0) {
                    if let Ok(kw) = std::str::from_utf8(&data[..null_pos]) {
                        if kw == "optipng-rs" {
                            return (true, true);
                        }
                    }
                }
            }
            let _ = file.seek(io::SeekFrom::Current(4)); // Skip CRC
        } else if &type_buf == b"IEND" {
            break;
        } else {
            let _ = file.seek(io::SeekFrom::Current((len + 4) as i64));
        }
    }

    (true, false)
}

/// Tracks file metrics discovered during directory and path scanning.
#[derive(Default, Debug)]
struct ScanStats {
    /// Count of valid PNG files discovered.
    valid_pngs: usize,
    /// Count of valid PNG files that were skipped because they are already optimized.
    already_optimized: usize,
    /// Count of non-PNG files or corrupted/invalid PNG files encountered.
    non_pngs: usize,
}

/// Recursively scans directories to collect valid target PNG files, tracking file statistics.
fn scan_directory(
    dir: &Path,
    current_depth: usize,
    max_depth: Option<usize>,
    recursive: bool,
    force_trials: bool,
        base_dir: &Path,
        visited_dirs: &mut HashSet<PathBuf>,
        visited_files: &mut HashSet<PathBuf>,
        found_files: &mut Vec<PathBuf>,
        scan_pb: Option<&indicatif::ProgressBar>,
        stats: &mut ScanStats,
) -> io::Result<()> {
    let canonical_dir = match fs::canonicalize(dir) {
        Ok(p) => p,
        Err(_) => return Ok(()),
    };

    if !visited_dirs.insert(canonical_dir) {
        return Ok(());
    }

    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();

        let meta = match fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };

        if meta.is_dir() {
            if recursive {
                let next_depth = current_depth + 1;
                if max_depth.map_or(true, |limit| next_depth <= limit) {
                    scan_directory(
                        &path,
                        next_depth,
                        max_depth,
                        recursive,
                        force_trials,
                            base_dir,
                            visited_dirs,
                            visited_files,
                            found_files,
                            scan_pb,
                            stats,
                    )?;
                }
            }
        } else if meta.is_file() {
            let is_png_ext = path
            .extension()
            .map_or(false, |ext| ext.to_string_lossy().eq_ignore_ascii_case("png"));

            if is_png_ext {
                let canonical_file = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                if visited_files.insert(canonical_file) {
                    if let Some(pb) = scan_pb {
                        pb.tick();
                    }

                    let rel_path = if recursive {
                        path.strip_prefix(base_dir)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .to_string()
                    } else {
                        path.file_name()
                        .map(|f| f.to_string_lossy().to_string())
                        .unwrap_or_else(|| path.to_string_lossy().to_string())
                    };

                    let (is_valid, is_already_optimized) = check_png_file(&path, force_trials);

                    if !is_valid {
                        stats.non_pngs += 1;
                        let error_mark = console::style("✖").red().bright();
                        if let Some(pb) = scan_pb {
                            pb.println(format!("{} {} is not a valid PNG file", error_mark, rel_path));
                        } else {
                            eprintln!("{} {} is not a valid PNG file", error_mark, rel_path);
                        }
                        continue;
                    }

                    stats.valid_pngs += 1;

                    if is_already_optimized {
                        stats.already_optimized += 1;
                        if let Some(pb) = scan_pb {
                            pb.println(format!("- {} -> skipped (already optimized)", rel_path));
                        }
                        continue;
                    }

                    found_files.push(path);
                    if let Some(pb) = scan_pb {
                        pb.set_message(format!("{:>6}", found_files.len()));
                    }
                }
            } else {
                stats.non_pngs += 1;
            }
        }
    }
    Ok(())
}

/// Dynamically calculates column widths for Indicatif UI based on terminal width.
/// Returns (fn_width, trials_width, bar_width, pct_width)
fn calculate_column_widths(term_width: usize) -> (usize, usize, usize, usize) {
    let w = term_width.max(60);
    let fn_w = (w as f64 * 0.50) as usize;
    let trials_w = 20;
    let pct_w = 10;

    let fixed = 7;
    let used = fn_w + trials_w + pct_w + fixed;
    let bar_w = if w > used { w - used } else { 10 };
    (fn_w, trials_w, bar_w, pct_w)
}

/// Returns the current terminal column width, falling back to 80 if unavailable.
fn get_terminal_width() -> usize {
    console::Term::stdout().size().1 as usize
}

/// Formats applied reduction parameters and image reduction info for finished summary lines.
fn format_reduction_info(state: &FileState, zi: u8) -> String {
    let mut parts = Vec::new();

    let bit_depth_reduced = state.out_bit_depth < state.orig_bit_depth;
    let orig_ch = color_type_channels(state.orig_color_type);
    let out_ch = color_type_channels(state.out_color_type);
    let channels_reduced = out_ch < orig_ch || state.out_color_type != state.orig_color_type;

    if bit_depth_reduced || channels_reduced {
        let mut red_desc = Vec::new();
        if bit_depth_reduced {
            red_desc.push(format!("bit depth: {}->{}", state.orig_bit_depth, state.out_bit_depth));
        } else {
            red_desc.push(format!("{}-bit", state.out_bit_depth));
        }

        if state.out_color_type != state.orig_color_type {
            let orig_name = color_type_short_name(state.orig_color_type);
            let out_name = color_type_short_name(state.out_color_type);
            if orig_ch != out_ch && orig_ch > 0 && out_ch > 0 {
                red_desc.push(format!("channels: {}->{} ({}->{})", orig_ch, out_ch, orig_name, out_name));
            } else {
                red_desc.push(format!("color: {}->{}", orig_name, out_name));
            }
        } else {
            red_desc.push(color_type_short_name(state.out_color_type).to_string());
        }
        parts.push(red_desc.join(", "));
    }

    if let Some(ref best) = state.best_config {
        if zi == 2 {
            parts.push(format!("-zc{} -f{}", best.zc, best.f));
        } else {
            parts.push(format!("-zc{} -zm{} -zs{} -f{}", best.zc, best.zm, best.zs, best.f));
        }
    }

    if parts.is_empty() {
        "no reduction".to_string()
    } else {
        parts.join(", ")
    }
}

impl Scheduler {
    /// Formats and updates the single overall progress bar shared across all threads.
    /// Throttles UI updates to a maximum of 10 Hz (100 ms) unless overall progress is complete.
    fn update_overall_pb(&mut self, pb: &indicatif::ProgressBar, term_width: usize, force: bool) {
        let total_files = self.files.len();
        if total_files == 0 {
            pb.set_position(10000);
            return;
        }

        let mut total_progress_units = 0.0f64;
        let mut completed_files = 0usize;

        for f in &self.files {
            if f.is_processed || f.is_skipped {
                total_progress_units += 1.0;
                completed_files += 1;
            } else if f.total_scanlines > 0 {
                let frac = (f.completed_scanlines as f64 / f.total_scanlines as f64).min(1.0);
                total_progress_units += frac;
            } else if f.total_trials > 0 {
                let frac = (f.completed_trials as f64 / f.total_trials as f64).min(1.0);
                total_progress_units += frac;
            }
        }

        let overall_frac = total_progress_units / total_files as f64;

        if !force && overall_frac < 1.0 {
            if let Some(last) = self.last_overall_pb_update {
                if last.elapsed() < std::time::Duration::from_millis(100) {
                    return;
                }
            }
        }

        self.last_overall_pb_update = Some(Instant::now());

        let (fn_w, _, bar_w, pct_w) = calculate_column_widths(term_width);

        let status_str = format!(
            "Overall Progress: {}/{} {}",
            completed_files,
            total_files,
            format!("{:.2}%", overall_frac * 100.0)
        );
        let truncated_status = truncate_middle(&status_str, fn_w);
        let msg = format!("{:<width$}", truncated_status, width = fn_w);

        let savings_bytes = self.total_orig_bytes.saturating_sub(self.total_new_bytes);
        let savings_pct = if self.total_orig_bytes > 0 {
            (savings_bytes as f64 / self.total_orig_bytes as f64) * 100.0
        } else {
            0.0
        };
        let extra_str = format!("{} ({:.1}%)", format_bytes(savings_bytes as usize), savings_pct);
        let extra = format!("{:>width$}", extra_str, width = pct_w);

        let template = format!("{{spinner:.cyan.bold}} {{msg}} {{bar:{bar_w}.cyan.bold/cyan}} {extra}");
        let style = indicatif::ProgressStyle::with_template(&template)
        .unwrap()
        .tick_chars(".oOo.")
        .progress_chars("█▉▊▋▌▍▎▏ ");

        pb.set_style(style);
        pb.set_message(msg);

        let pos = (overall_frac * 10000.0) as u64;
        pb.set_position(pos.min(10000));
    }
}

/// Loads and decodes raw pixel data from disk or external file converters.
fn load_file_pixels(cli: &CliArgs, task: &FileTask) -> Result<(u32, u32, u8, u8, usize, Vec<u8>, usize, bool), String> {
    let file_path_str = task.in_path.to_string_lossy().to_string();
    let mut width = 0u32;
    let mut height = 0u32;
    let mut bit_depth = 0u8;
    let mut color_type = 0u8;
    let stride: usize;
    let raw_pixels: Vec<u8>;
    let orig_idat_size: usize;

    if task.is_external {
        let img = load_external_image(&file_path_str)?;
        width = img.width;
        height = img.height;
        bit_depth = img.bit_depth;
        color_type = img.color_type;
        stride = img.stride;
        raw_pixels = img.raw_pixels;
        orig_idat_size = img.orig_idat_size;
    } else {
        let c_file = CString::new(file_path_str.clone()).map_err(|_| "Invalid CString path".to_string())?;
        let mut stride_usize = 0;

        let dec = open_png(
            c_file.as_ptr(),
            true,
            &mut width,
            &mut height,
            &mut bit_depth,
            &mut color_type,
            &mut stride_usize,
        );

        if dec.is_null() {
            return Err(format!("Failed to decode PNG {}", file_path_str));
        }

        stride = stride_usize;
        png_set_count_idat_size(dec, true);

        if cli.nz {
            raw_pixels = Vec::new();
            orig_idat_size = png_get_idat_size(dec);
            close_png(dec);
        } else {
            let expected_size = stride * height as usize;
            let mut pixels = Vec::with_capacity(expected_size);

            loop {
                let res = decode_scanlines(dec, 1024);
                if res.size == 0 || res.data.is_null() {
                    break;
                }
                let chunk = unsafe { std::slice::from_raw_parts(res.data, res.size) };
                pixels.extend_from_slice(chunk);
            }
            orig_idat_size = png_get_idat_size(dec);
            raw_pixels = pixels;
            close_png(dec);
        }
    }

    Ok((width, height, bit_depth, color_type, stride, raw_pixels, orig_idat_size, false))
}

/// Intermediate payload produced by lazy decoding and reduction prior to trial dispatch.
struct PreparedData {
    total_trials: usize,
    total_scanlines: usize,
    best_size: usize,
    best_config: Option<TrialConfig>,
    orig_idat_size: usize,
    image_data: Option<Arc<Vec<u8>>>,
    shared_palette: Option<Arc<Vec<u8>>>,
    shared_trns: Option<Arc<Vec<u8>>>,
    width: u32,
    height: u32,
    orig_bit_depth: u8,
    orig_color_type: u8,
    out_bit_depth: u8,
    out_color_type: u8,
    trials: Vec<TrialConfig>,
    is_skipped: bool,
    error_msg: Option<String>,
}

/// Performs lazy decoding and reduction heuristics for a file task when activated.
fn prepare_file_data(cli: &CliArgs, task: &FileTask) -> PreparedData {
    let (width, height, bit_depth, color_type, stride, mut raw_pixels, orig_idat_size, is_skipped) =
    match load_file_pixels(cli, task) {
        Ok(res) => res,
        Err(err_msg) => {
            return PreparedData {
                total_trials: 0, total_scanlines: 0, best_size: usize::MAX, best_config: None,
                orig_idat_size: 0, image_data: None, shared_palette: None, shared_trns: None,
                width: 0, height: 0, orig_bit_depth: 0, orig_color_type: 0,
                out_bit_depth: 0, out_color_type: 0, trials: Vec::new(),
                is_skipped: true,
                error_msg: Some(err_msg),
            };
        }
    };

    if is_skipped || cli.nz {
        let trial = TrialConfig { zc: 1, zm: 8, zs: 0, f: 0 };
        return PreparedData {
            total_trials: 1, total_scanlines: 1, best_size: orig_idat_size, best_config: Some(trial),
            orig_idat_size, image_data: None, shared_palette: None, shared_trns: None,
            width, height, orig_bit_depth: bit_depth, orig_color_type: color_type,
            out_bit_depth: bit_depth, out_color_type: color_type, trials: Vec::new(),
            is_skipped,
            error_msg: None,
        };
    }

    let ReductionResult {
        out_color_type,
        out_bit_depth,
        final_palette,
        final_trns,
    } = reduce_image(cli, width, height, color_type, bit_depth, stride, &mut raw_pixels);

    let shared_palette = final_palette.map(Arc::new);
    let shared_trns = final_trns.map(Arc::new);
    let image_data = Some(Arc::new(raw_pixels));

    let (zc_list, zm_list, zs_list, f_list) = if cli.zi == 2 {
        let (def_zc, def_f) = get_zopfli_opt_combinations(cli.opt_level);
        (cli.zc.clone().unwrap_or(def_zc), vec![8], vec![0], cli.f.clone().unwrap_or(def_f))
    } else {
        let (def_zc, def_zm, def_zs, def_f) = get_opt_combinations(cli.opt_level, out_color_type, out_bit_depth);
        (
            cli.zc.clone().unwrap_or(def_zc),
         cli.zm.clone().unwrap_or(def_zm),
         cli.zs.clone().unwrap_or(def_zs),
         cli.f.clone().unwrap_or(def_f),
        )
    };

    let trials = build_trial_list(&zc_list, &zm_list, &zs_list, &f_list);
    let total_trials = trials.len();
    let total_scanlines = if cli.zi == 2 { 0 } else { total_trials * height as usize };

    PreparedData {
        total_trials, total_scanlines, best_size: usize::MAX, best_config: None,
        orig_idat_size, image_data, shared_palette, shared_trns,
        width, height, orig_bit_depth: bit_depth, orig_color_type: color_type,
        out_bit_depth, out_color_type, trials,
        is_skipped: false,
        error_msg: None,
    }
}

/// Encapsulates overall work distribution state and thread synchronization structures.
struct Scheduler {
    files: Vec<FileState>,
    active_indices: Vec<usize>,
    next_file_to_prepare: usize,
    finished_files: usize,
    total_orig_bytes: u64,
    total_new_bytes: u64,
    overall_pb: Option<indicatif::ProgressBar>,
    last_overall_pb_update: Option<Instant>,
    multi_progress: Option<Arc<indicatif::MultiProgress>>,
}

/// Defines a single work task assigned to a thread.
enum WorkTask {
    Trial { file_idx: usize, trial_idx: usize },
    Terminate,
}

/// Output writer: executes final stream encoding or file operations when file trials finish.
fn finalize_file_write(cli: &CliArgs, file_state: &mut FileState) -> u64 {
    if cli.simulate || file_state.is_skipped {
        return 0;
    }

    let orig_file_size = file_state.task.orig_size;
    let file_path_str = file_state.task.in_path.to_string_lossy().to_string();

    let best = match &file_state.best_config {
        Some(b) => b,
        None => return 0,
    };

    if !cli.force_reenc && !file_state.task.is_external && file_state.best_size >= file_state.orig_idat_size && !cli.nz {
        return 0;
    }

    let original_path = &file_state.task.in_path;
    let out_path = &file_state.task.out_path;

    let orig_metadata = fs::metadata(original_path).ok();
    let is_in_place = out_path == original_path;
    let old_path = PathBuf::from(format!("{}.bak.{}", file_path_str, std::process::id()));

    if is_in_place {
        if fs::rename(original_path, &old_path).is_err() {
            return 0;
        }
    }

    let input_source = if is_in_place { &old_path } else { original_path };
    let mut success = false;

    let opt_info = if cli.opt_level == 0 {
        "-o0".to_string()
    } else if cli.cmd_options.is_empty() {
        format!("{}{}{}{}", best.zc, best.zm, best.zs, best.f)
    } else {
        format!("{}\n{}{}{}{}", cli.cmd_options, best.zc, best.zm, best.zs, best.f)
    };

    if let Some(ref winning_bytes) = file_state.best_bytes {
        if fs::write(out_path, winning_bytes).is_ok() {
            success = out_path.exists();
        }
    } else if cli.nz && !file_state.task.is_external {
        if copy_png_idat_and_add_text(input_source, out_path, "optipng-rs", &opt_info).is_ok() {
            success = out_path.exists();
        }
    } else if let Some(ref img_data) = file_state.image_data {
        let c_out_path = CString::new(out_path.to_string_lossy().into_owned()).unwrap();
        let final_options = ZlibOptions {
            z_implementation: cli.zi,
            level: best.zc,
            strategy: best.zs,
            window_bits: 15,
            mem_level: best.zm,
            max_idat_size: 0,
            expected_idat_size: file_state.best_size,
        };

        let (pal_ptr, pal_len) = match &file_state.shared_palette {
            Some(pal) => (pal.as_ptr(), pal.len()),
            None => (std::ptr::null(), 0),
        };

        let (trns_ptr, trns_len) = match &file_state.shared_trns {
            Some(trns) => (trns.as_ptr(), trns.len()),
            None => (std::ptr::null(), 0),
        };

        let c_key = CString::new("optipng-rs").unwrap();
        let c_val = CString::new(opt_info).unwrap();
        let text_keys = [c_key.as_ptr()];
        let text_vals = [c_val.as_ptr()];

        let enc = open_png_encode(
            c_out_path.as_ptr(),
                                  file_state.width,
                                  file_state.height,
                                  file_state.out_bit_depth,
                                  file_state.out_color_type,
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
            let total_rows = file_state.height as usize;
            let row_bytes = if total_rows > 0 { img_data.len() / total_rows } else { 0 };
            let mut encoded_rows = 0usize;
            let chunk_rows = 250;

            while encoded_rows < total_rows {
                let rows_to_encode = (total_rows - encoded_rows).min(chunk_rows);
                let offset = encoded_rows * row_bytes;
                let ptr = unsafe { img_data.as_ptr().add(offset) };

                encode_scanlines(enc, ptr, rows_to_encode as u32);
                encoded_rows += rows_to_encode;
            }

            close_png_encode(enc);
            success = out_path.exists();
        }
    }

    if success {
        preserve_file_times(out_path, orig_metadata.as_ref());
        let actual_size = fs::metadata(out_path).map(|m| m.len()).unwrap_or(0);
        if is_in_place && !cli.backup {
            let _ = fs::remove_file(&old_path);
        }
        actual_size
    } else {
        if is_in_place {
            let _ = fs::rename(&old_path, original_path);
        }
        orig_file_size
    }
}

fn main() {
    let cli = parse_args();

    if cli.show_help {
        print_usage();
        std::process::exit(0);
    }

    // Missing input files
    if cli.files.is_empty() && cli.external_input.is_none() {
        if !cli.quiet {
            eprintln!("{}\n", format_error("Error: No input files specified."));
            print_usage();
        }
        std::process::exit(1);
    }

    let mut input_paths: Vec<(PathBuf, bool)> = Vec::new();
    let mut visited_dirs = HashSet::new();
    let mut visited_files = HashSet::new();

    let base_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    // Initialize evaluation progress bar (gray spinner, file number column, cyan bar in brackets)
    let scan_pb = if !cli.quiet {
        let pb = indicatif::ProgressBar::new_spinner();
        pb.enable_steady_tick(std::time::Duration::from_millis(100));
        let term_w = get_terminal_width();
        let bar_w = term_w.saturating_sub(12).max(10);
        let template = format!("{{spinner:.dim}} {{msg}} {{bar:{bar_w}.cyan.bold/cyan}}");
        let style = indicatif::ProgressStyle::with_template(&template)
        .unwrap()
        .tick_chars(".oOo.")
        .progress_chars("█▉▊▋▌▍▎▏ ");
        pb.set_style(style);
        pb.set_message(format!("{:>6}", 0));
        Some(pb)
    } else {
        None
    };

    let mut stats = ScanStats::default();

    if let Some(ref ext_in) = cli.external_input {
        let p = PathBuf::from(ext_in);
        let (is_valid, is_opt) = check_png_file(&p, cli.force_trials);
        if is_valid {
            stats.valid_pngs += 1;
            if is_opt {
                stats.already_optimized += 1;
            } else {
                input_paths.push((p, true));
            }
        } else {
            stats.non_pngs += 1;
            eprintln!("{}", format_error(&format!("'{}' is not a valid PNG file", ext_in)));
        }
    } else {
        let mut found_files = Vec::new();

        for target in &cli.files {
            let path = PathBuf::from(target);
            if path.is_dir() || target == "." {
                let _ = scan_directory(
                    &path,
                    1,
                    cli.max_depth,
                    cli.recursive,
                    cli.force_trials,
                    &base_dir,
                    &mut visited_dirs,
                    &mut visited_files,
                    &mut found_files,
                    scan_pb.as_ref(),
                                       &mut stats,
                );
            } else if path.is_file() {
                let canonical_file = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                if visited_files.insert(canonical_file) {
                    if let Some(ref pb) = scan_pb {
                        pb.tick();
                    }

                    let rel_path = if cli.recursive {
                        path.strip_prefix(&base_dir)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .to_string()
                    } else {
                        path.file_name()
                        .map(|f| f.to_string_lossy().to_string())
                        .unwrap_or_else(|| path.to_string_lossy().to_string())
                    };

                    let (is_valid, is_already_optimized) = check_png_file(&path, cli.force_trials);
                    if !is_valid {
                        stats.non_pngs += 1;
                        let error_mark = console::style("✖").red().bright();
                        if let Some(ref pb) = scan_pb {
                            pb.println(format!("{} {} is not a valid PNG file", error_mark, rel_path));
                        } else {
                            eprintln!("{} {} is not a valid PNG file", error_mark, rel_path);
                        }
                        continue;
                    }

                    stats.valid_pngs += 1;

                    if is_already_optimized {
                        stats.already_optimized += 1;
                        if let Some(ref pb) = scan_pb {
                            pb.println(format!("- {} -> skipped (already optimized)", rel_path));
                        }
                    } else {
                        found_files.push(path);
                        if let Some(ref pb) = scan_pb {
                            pb.set_message(format!("{:>6}", found_files.len()));
                        }
                    }
                }
            }
        }

        for f in found_files {
            input_paths.push((f, false));
        }
    }

    if let Some(ref pb) = scan_pb {
        pb.finish_and_clear();
    }

    // Print scan breakdown summary
    if !cli.quiet {
        println!("\nSCAN COMPLETED:");
        println!("  Valid PNG files found  : {}", stats.valid_pngs);
        println!("  Already optimized .... : {}", stats.already_optimized);
        println!("  Non-PNG / invalid files: {}", stats.non_pngs);
        println!("  Files to be processed  : {}\n", stats.valid_pngs - stats.already_optimized);
    }

    let is_multi_file = input_paths.len() > 1;

    let mut tasks: Vec<FileTask> = Vec::new();

    for (in_path, is_ext) in input_paths {
        let size = fs::metadata(&in_path).map(|m| m.len()).unwrap_or(0);

        let out_path = if let Some(ref out_arg) = cli.out_file {
            let out_p = PathBuf::from(out_arg);
            if is_multi_file || out_p.is_dir() || out_arg.ends_with('/') || out_arg.ends_with('\\') {
                if let Err(e) = fs::create_dir_all(&out_p) {
                    eprintln!("{}", format_error(&format!("Failed to create output directory {:?}: {}", out_p, e)));
                    std::process::exit(1);
                }
                out_p.join(in_path.file_name().unwrap_or_default())
            } else {
                out_p
            }
        } else if is_ext {
            in_path.with_extension("png")
        } else {
            in_path.clone()
        };

        tasks.push(FileTask {
            in_path,
            out_path,
            is_external: is_ext,
            orig_size: size,
        });
    }

    let task_count = tasks.len();
    let mut file_states = Vec::with_capacity(task_count);

    for task in tasks {
        let rel_path = if cli.recursive {
            task.in_path
            .strip_prefix(&base_dir)
            .unwrap_or(&task.in_path)
            .to_string_lossy()
            .to_string()
        } else {
            task.in_path
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| task.in_path.to_string_lossy().to_string())
        };

        file_states.push(FileState {
            task,
            rel_path,
            total_trials: 0,
            completed_trials: 0,
            total_scanlines: 0,
            completed_scanlines: 0,
            best_size: usize::MAX,
            best_config: None,
            best_bytes: None,
            orig_idat_size: 0,
            image_data: None,
            shared_palette: None,
            shared_trns: None,
            width: 0,
            height: 0,
            orig_bit_depth: 0,
            orig_color_type: 0,
            out_bit_depth: 0,
            out_color_type: 0,
            trials: Vec::new(),
                         next_trial_idx: 0,
                         is_skipped: false,
                         error_msg: None,
                         is_processed: false,
                         is_prepared: false,
                         is_preparing: false,
        });
    }

    let multi_progress = if !cli.quiet {
        Some(Arc::new(indicatif::MultiProgress::new()))
    } else {
        None
    };

    // Initialize single overall progress bar for all threads
    let overall_pb = multi_progress
        .as_ref()
        .map(|mp| {
            let pb = mp.add(indicatif::ProgressBar::new(10000));
            pb.enable_steady_tick(std::time::Duration::from_millis(100));
            pb
        });

    let mut scheduler_inner = Scheduler {
        files: file_states,
        active_indices: Vec::new(),
        next_file_to_prepare: 0,
        finished_files: 0,
        total_orig_bytes: 0,
        total_new_bytes: 0,
        overall_pb,
        last_overall_pb_update: None,
        multi_progress,
    };

    if let Some(pb) = scheduler_inner.overall_pb.clone() {
        let term_w = get_terminal_width();
        scheduler_inner.update_overall_pb(&pb, term_w, false);
    }

    run_multithreaded_pipeline(cli, scheduler_inner);
}

/// Worker loop orchestrator: manages trial execution, lazy file loading, memory bounds, and thread synchronization.
fn run_multithreaded_pipeline(cli: CliArgs, scheduler_inner: Scheduler) {
    let start_time = Instant::now();
    let total_files = scheduler_inner.files.len();

    let scheduler = Arc::new(Mutex::new(scheduler_inner));
    let condvar = Arc::new(Condvar::new());
    let cli_arc = Arc::new(cli);

    let mut handles = Vec::new();

    for _worker_id in 0..cli_arc.mt {
        let scheduler_clone = Arc::clone(&scheduler);
        let condvar_clone = Arc::clone(&condvar);
        let cli_ref = Arc::clone(&cli_arc);

        handles.push(thread::spawn(move || {
            let mut sys = System::new();

            loop {
                let task = {
                    let mut lock = scheduler_clone.lock().unwrap();

                    loop {
                        if lock.finished_files == total_files {
                            break WorkTask::Terminate;
                        }

                        let active = lock.active_indices.clone();
                        let mut found_trial = None;
                        for &idx in &active {
                            let state = &mut lock.files[idx];
                            if state.is_prepared && !state.is_preparing && state.next_trial_idx < state.trials.len() {
                                let trial_idx = state.next_trial_idx;
                                state.next_trial_idx += 1;
                                found_trial = Some(WorkTask::Trial { file_idx: idx, trial_idx });
                                break;
                            }
                        }

                        if let Some(work) = found_trial {
                            break work;
                        }

                        let any_preparing = lock.files.iter().any(|f| f.is_preparing);
                        if any_preparing {
                            lock = condvar_clone.wait(lock).unwrap();
                            continue;
                        }

                        let can_activate_new = lock.next_file_to_prepare < lock.files.len()
                        && is_memory_safe(&mut sys, cli_ref.memory_limit);

                        if can_activate_new {
                            let next_idx = lock.next_file_to_prepare;
                            lock.next_file_to_prepare += 1;
                            lock.files[next_idx].is_preparing = true;

                            let task = lock.files[next_idx].task.clone();
                            drop(lock);

                            let prep = prepare_file_data(&cli_ref, &task);
                            lock = scheduler_clone.lock().unwrap();
                            let mp = lock.multi_progress.clone();
                            let overall_pb = lock.overall_pb.clone();

                            let (has_error, is_skipped_or_empty, rel_path, orig_size, orig_color_type, orig_bit_depth, width, height) = {
                                let state = &mut lock.files[next_idx];
                                state.is_preparing = false;
                                state.is_prepared = true;
                                state.total_trials = prep.total_trials;
                                state.total_scanlines = prep.total_scanlines;
                                state.best_size = prep.best_size;
                                state.best_config = prep.best_config;
                                state.orig_idat_size = prep.orig_idat_size;
                                state.image_data = prep.image_data;
                                state.shared_palette = prep.shared_palette;
                                state.shared_trns = prep.shared_trns;
                                state.width = prep.width;
                                state.height = prep.height;
                                state.orig_bit_depth = prep.orig_bit_depth;
                                state.orig_color_type = prep.orig_color_type;
                                state.out_bit_depth = prep.out_bit_depth;
                                state.out_color_type = prep.out_color_type;
                                state.trials = prep.trials;
                                state.is_skipped = prep.is_skipped;
                                state.error_msg = prep.error_msg;

                                let has_error = state.error_msg.is_some();
                                let is_skipped_or_empty = state.is_skipped || state.trials.is_empty();
                                if has_error || is_skipped_or_empty {
                                    state.is_processed = true;
                                }

                                (
                                    has_error,
                                 is_skipped_or_empty,
                                 state.rel_path.clone(),
                                 state.task.orig_size,
                                 state.orig_color_type,
                                 state.orig_bit_depth,
                                 state.width,
                                 state.height,
                                )
                            };

                            if has_error {
                                let err = lock.files[next_idx].error_msg.as_ref().unwrap();
                                if let Some(ref mp_handle) = mp {
                                    let _ = mp_handle.println(format_error(&format!("{} - Error: {}", rel_path, err)));
                                }

                                lock.finished_files += 1;
                                lock.total_orig_bytes += orig_size;
                                lock.total_new_bytes += orig_size;

                                if let Some(ref opb) = overall_pb {
                                    let term_w = get_terminal_width();
                                    lock.update_overall_pb(opb, term_w, true);
                                }

                                condvar_clone.notify_all();
                                continue;
                            }

                            if is_skipped_or_empty {
                                if let Some(ref mp_handle) = mp {
                                    let _ = mp_handle.println(format!("- {} -> skipped (already optimized)", rel_path));
                                }

                                lock.finished_files += 1;
                                lock.total_orig_bytes += orig_size;
                                lock.total_new_bytes += orig_size;

                                if let Some(ref opb) = overall_pb {
                                    let term_w = get_terminal_width();
                                    lock.update_overall_pb(opb, term_w, true);
                                }

                                condvar_clone.notify_all();
                                continue;
                            }

                            if let Some(ref mp_handle) = mp {
                                let color_name = color_type_short_name(orig_color_type);
                                let _ = mp_handle.println(format!(
                                    "Opened {} -> {} x {} / {} bit / {}",
                                    rel_path, width, height, orig_bit_depth, color_name
                                ));
                            }

                            if let Some(ref opb) = overall_pb {
                                let term_w = get_terminal_width();
                                lock.update_overall_pb(opb, term_w, false);
                            }

                            let trial_idx = lock.files[next_idx].next_trial_idx;
                            lock.files[next_idx].next_trial_idx += 1;

                            lock.active_indices.push(next_idx);

                            condvar_clone.notify_all();
                            break WorkTask::Trial { file_idx: next_idx, trial_idx };
                        }

                        if lock.finished_files == total_files {
                            break WorkTask::Terminate;
                        }

                        lock = condvar_clone.wait(lock).unwrap();
                    }
                };

                match task {
                    WorkTask::Trial { file_idx, trial_idx } => {
                        let (
                            image_data, palette, trns, width, height,
                             out_bit_depth, out_color_type, trial,
                        ) = {
                            let lock = scheduler_clone.lock().unwrap();
                            let state = &lock.files[file_idx];
                            (
                                Arc::clone(state.image_data.as_ref().unwrap()),
                             state.shared_palette.clone(),
                             state.shared_trns.clone(),
                             state.width,
                             state.height,
                             state.out_bit_depth,
                             state.out_color_type,
                             state.trials[trial_idx].clone(),
                            )
                        };

                        let sched_cb = Arc::clone(&scheduler_clone);

                        let on_scanlines = |chunk_rows: usize| {
                            if cli_ref.zi == 1 {
                                let mut lock = sched_cb.lock().unwrap();
                                let state = &mut lock.files[file_idx];
                                state.completed_scanlines += chunk_rows;

                                if let Some(opb) = lock.overall_pb.clone() {
                                    let term_w = get_terminal_width();
                                    lock.update_overall_pb(&opb, term_w, false);
                                }
                            }
                        };

                        let (trial_idat_size, winning_bytes) = run_single_trial(
                            &image_data,
                            &palette,
                            &trns,
                            width,
                            height,
                            out_bit_depth,
                            out_color_type,
                            &trial,
                            cli_ref.opt_level,
                            &cli_ref.cmd_options,
                            cli_ref.zi,
                            Some(&on_scanlines),
                        );

                        let mut lock = scheduler_clone.lock().unwrap();
                        let mp = lock.multi_progress.clone();
                        {
                            let state = &mut lock.files[file_idx];
                            state.completed_trials += 1;

                            if trial_idat_size < state.best_size && trial_idat_size > 0 {
                                state.best_size = trial_idat_size;
                                state.best_config = Some(trial);
                                if cli_ref.zi == 2 {
                                    state.best_bytes = winning_bytes;
                                }
                            }
                        }

                        if let Some(ref opb) = lock.overall_pb.clone() {
                            let term_w = get_terminal_width();
                            lock.update_overall_pb(&opb, term_w, false);
                        }

                        if lock.files[file_idx].completed_trials == lock.files[file_idx].total_trials {
                            let actual_size = finalize_file_write(&cli_ref, &mut lock.files[file_idx]);
                            let orig_size = lock.files[file_idx].task.orig_size;

                            let (rel_path, is_skipped, red_str) = {
                                let state = &mut lock.files[file_idx];
                                state.is_processed = true;
                                state.image_data = None;
                                (
                                    state.rel_path.clone(),
                                 state.is_skipped,
                                 format_reduction_info(state, cli_ref.zi),
                                )
                            };

                            let final_size = if actual_size > 0 { actual_size } else { orig_size };

                            let sav_str = if is_skipped {
                                "skipped".to_string()
                            } else if final_size < orig_size {
                                let saved = orig_size - final_size;
                                let pct = (saved as f64 / orig_size as f64) * 100.0;
                                format!("saved {} ({:.1}%)", format_bytes(saved as usize), pct)
                            } else if final_size == orig_size {
                                "no size reduction".to_string()
                            } else {
                                let diff = final_size - orig_size;
                                let pct = (diff as f64 / orig_size as f64) * 100.0;
                                format!("+{} (+{:.1}%)", format_bytes(diff as usize), pct)
                            };

                            if let Some(ref mp_handle) = mp {
                                if is_skipped {
                                    let _ = mp_handle.println(format!("- {} -> skipped", rel_path));
                                } else {
                                    let check_mark = console::style("✓").green().bright();
                                    let _ = mp_handle.println(format!("{} {} [{}] -> {}", check_mark, rel_path, red_str, sav_str));
                                }
                            }

                            lock.finished_files += 1;
                            lock.total_orig_bytes += orig_size;
                            lock.total_new_bytes += final_size;

                            if let Some(ref opb) = lock.overall_pb.clone() {
                                let term_w = get_terminal_width();
                                lock.update_overall_pb(opb, term_w, true);
                            }

                            lock.active_indices.retain(|&i| i != file_idx);
                            condvar_clone.notify_all();
                        }
                    }
                    WorkTask::Terminate => {
                        break;
                    }
                }
            }
        }));
    }

    for h in handles {
        let _ = h.join();
    }

    let duration = start_time.elapsed();
    let final_sched = scheduler.lock().unwrap();

    if let Some(ref opb) = final_sched.overall_pb {
        opb.finish_and_clear();
    }

    if !cli_arc.quiet {
        let total_orig = final_sched.total_orig_bytes;
        let total_new = final_sched.total_new_bytes;
        let saved_bytes = total_orig.saturating_sub(total_new);
        let pct = if total_orig > 0 {
            (saved_bytes as f64 / total_orig as f64) * 100.0
        } else {
            0.0
        };

        println!("\nSUMMARY OF PROCESSED FILES");
        println!("  Files processed ..... : {}", final_sched.finished_files);
        println!("  Total original size . : {} bytes ({})", total_orig, format_bytes(total_orig as usize));
        println!("  Total new size ...... : {} bytes ({})", total_new, format_bytes(total_new as usize));
        println!(
            "  Total size decrease . : {} bytes ({}) ({:.2}%)",
                 saved_bytes,
                 format_bytes(saved_bytes as usize),
                     pct
        );
        println!("  Total processing time : {}", format_duration(duration));
    }
}