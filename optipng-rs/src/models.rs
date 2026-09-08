/**************************************
 * optipng-rs: Shared data structures *
 ************************************+*/

use std::path::PathBuf;

/// Represents the configuration for a single compression trial.
#[derive(Debug, Clone)]
pub struct TrialConfig {
    pub zc: i32,
    pub zm: i32,
    pub zs: i32,
    pub f: u8,
}

/// Contains all parsed command-line arguments.
pub struct CliArgs {
    pub files: Vec<String>,
    pub external_input: Option<String>,
    pub opt_level: u8,
    pub mt: usize,
    pub zi: u8,
    pub zc: Option<Vec<i32>>,
    pub zm: Option<Vec<i32>>,
    pub zs: Option<Vec<i32>>,
    pub f: Option<Vec<u8>>,
    pub backup: bool,
    pub simulate: bool,
    pub quiet: bool,
    pub nc: bool,
    pub nb: bool,
    pub np: bool,
    pub nx: bool,
    pub nz: bool,
    pub out_file: Option<String>,
    pub recursive: bool,
    pub max_depth: Option<usize>,
    pub show_help: bool,
    pub force_trials: bool,
    pub force_reenc: bool,
    pub cmd_options: String,
    pub memory_limit: f64,
}

/// A target file queued for processing.
#[derive(Debug, Clone)]
pub struct FileTask {
    pub in_path: PathBuf,
    pub out_path: PathBuf,
    pub is_external: bool,
    pub orig_size: u64,
}

/// Internal image representation after loading.
pub struct LoadedImage {
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    pub color_type: u8,
    pub stride: usize,
    pub raw_pixels: Vec<u8>,
    pub orig_idat_size: usize,
}

/// The result of bit-depth and color type reduction heuristics.
pub struct ReductionResult {
    pub out_color_type: u8,
    pub out_bit_depth: u8,
    pub final_palette: Option<Vec<u8>>,
    pub final_trns: Option<Vec<u8>>,
}

/// Holds runtime state, raw pixel buffers, and progress tracker for a single file task.
pub struct FileState {
    pub task: FileTask,
    pub rel_path: String,
    pub total_trials: usize,
    pub completed_trials: usize,
    pub total_scanlines: usize,
    pub completed_scanlines: usize,
    pub best_size: usize,
    pub best_config: Option<TrialConfig>,
    pub best_bytes: Option<Vec<u8>>,
    pub orig_idat_size: usize,
    pub image_data: Option<std::sync::Arc<Vec<u8>>>,
    pub shared_palette: Option<std::sync::Arc<Vec<u8>>>,
    pub shared_trns: Option<std::sync::Arc<Vec<u8>>>,
    pub width: u32,
    pub height: u32,
    pub orig_bit_depth: u8,
    pub orig_color_type: u8,
    pub out_bit_depth: u8,
    pub out_color_type: u8,
    pub trials: Vec<TrialConfig>,
    pub next_trial_idx: usize,
    pub pb: Option<indicatif::ProgressBar>,
    pub is_skipped: bool,
    pub is_processed: bool,
    pub is_prepared: bool,
    pub is_preparing: bool,
}