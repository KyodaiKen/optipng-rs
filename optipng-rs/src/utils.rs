/*************************************************************
 * optipng-rs: Formatting, file times, callbacks, and helpers *
 **************************************************************/

use std::ffi::c_void;
use std::fs::{self, FileTimes, Metadata};
use std::path::Path;
use std::time::Duration;
use sysinfo::System;

/// Truncates a string in the middle using an ellipsis if it exceeds max_width.
pub fn truncate_middle(s: &str, max_width: usize) -> String {
    if s.chars().count() <= max_width {
        return s.to_string();
    }
    if max_width <= 1 {
        return "…".to_string();
    }
    let keep_len = (max_width - 1) / 2;
    let front: String = s.chars().take(keep_len).collect();
    let back: String = s.chars().skip(s.chars().count() - keep_len).collect();
    format!("{}…{}", front, back)
}

/// Checks if current memory consumption is strictly below the given percentage limit.
pub fn is_memory_safe(sys: &mut System, limit_pct: f64) -> bool {
    sys.refresh_memory();
    let total = sys.total_memory() as f64;
    let used = sys.used_memory() as f64;
    if total <= 0.0 {
        return true;
    }
    (used / total) * 100.0 <= limit_pct
}

/// Returns short color type names for concise CLI progress output.
pub fn color_type_short_name(color_type: u8) -> &'static str {
    match color_type {
        0 => "Gray",
        2 => "RGB",
        3 => "Palette",
        4 => "Gray+Alpha",
        6 => "RGBA",
        _ => "Unknown",
    }
}

/// Returns the number of color/alpha channels for a given PNG color type.
pub fn color_type_channels(color_type: u8) -> u8 {
    match color_type {
        0 => 1, // Grayscale
        2 => 3, // RGB
        3 => 1, // Palette index
        4 => 2, // Gray + Alpha
        6 => 4, // RGBA
        _ => 0,
    }
}

pub fn preserve_file_times(target_path: &Path, orig_metadata: Option<&Metadata>) {
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

pub fn parse_ranges_i32(input: &str) -> Vec<i32> {
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

pub fn parse_ranges_u8(input: &str) -> Vec<u8> {
    parse_ranges_i32(input)
    .into_iter()
    .filter_map(|v| u8::try_from(v).ok())
    .collect()
}

// FORMATTING
pub fn format_bytes(bytes: usize) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB", "PB", "EB", "ZB", "YB"];
    if bytes == 0 {
        return "0B".to_string();
    }
    let base = 1024f64;
    let bytes_f = bytes as f64;
    let digit = (bytes_f.log(base)).floor() as usize;
    let digit = digit.min(UNITS.len() - 1);

    if digit == 0 {
        format!("{}{}", bytes, UNITS[0])
    } else {
        let value = bytes_f / base.powi(digit as i32);
        format!("{:.2}{}", value, UNITS[digit])
    }
}

pub fn format_duration(duration: Duration) -> String {
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

// CALLBACKS
pub type PngWriteCallback = unsafe extern "C" fn(*mut c_void, *const u8, usize) -> usize;

pub unsafe extern "C" fn buffer_write_cb(user_data: *mut c_void, buf: *const u8, len: usize) -> usize {
    if !user_data.is_null() && !buf.is_null() {
        let vec = unsafe { &mut *(user_data as *mut Vec<u8>) };
        let slice = unsafe { std::slice::from_raw_parts(buf, len) };
        vec.extend_from_slice(slice);
    }
    len
}

pub unsafe extern "C" fn counter_write_cb(user_data: *mut c_void, _buf: *const u8, len: usize) -> usize {
    if !user_data.is_null() && !_buf.is_null() {
        let counter = unsafe { &mut *(user_data as *mut usize) };
        *counter += len;
    }
    len
}