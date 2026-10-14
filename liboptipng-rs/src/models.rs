/// Shared data structures for liboptipng.

use std::path::PathBuf;

/// Represents the configuration for a single compression trial.
#[derive(Debug, Clone)]
pub struct TrialConfig {
    pub zc: i32,
    pub zm: i32,
    pub zs: i32,
    pub f: u8,
}

/// Configuration for an optimization run. Library-neutral: a file-based task
/// fills `files`, an in-memory task fills `in_memory_source`.
pub struct OptimizationTask {
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
    pub memory_limit: f64,
    /// Optional event sink. The library is UI‑agnostic: it only *emits*
    /// `PipelineEvent`s. The owning application (CLI / FFI) decides how to
    /// render them. Install `None` for quiet runs so nothing is emitted.
    /// Called from any worker thread, so it must be `Send + Sync`.
    pub event_callback: Option<Box<dyn Fn(&PipelineEvent) + Send + Sync>>,
    /// When true (default), apply color/bit-depth reduction heuristics before
    /// compression trials. When false, compress the image exactly as given.
    pub reduce_format: bool,
    /// When set, optimise this in-memory image instead of `files`.
    pub in_memory_source: Option<LoadedImage>,
}

impl Default for OptimizationTask {
    fn default() -> Self {
        Self {
            files: Vec::new(),
            external_input: None,
            opt_level: 2,
            mt: 4,
            zi: 0,
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
            recursive: false,
            max_depth: None,
            show_help: false,
            force_trials: false,
            force_reenc: false,
            memory_limit: 90.0,
            event_callback: None,
            reduce_format: true,
            in_memory_source: None,
        }
    }
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

/// Holds runtime state and pixel buffers for a single file task.
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
    pub is_skipped: bool,
    pub error_msg: Option<String>,
    pub is_processed: bool,
    pub is_prepared: bool,
    pub is_preparing: bool,
}

// ─── Pipeline events (library → application feedback channel) ──────────────

/// Discrete, render-agnostic feedback emitted by the pipeline. The application
/// owns all styling, bars and console I/O; the library only supplies the data.
#[derive(Debug, Clone)]
pub enum PipelineEvent {
    /// A file was visited during scanning; `found` is the running count of
    /// valid, non-optimized files queued so far (drives the scan spinner).
    ScanTick { found: usize },
    /// A file was found but is not a valid PNG.
    ScanNonPng { rel_path: String },
    /// A file was found but is already optimized (skipped during scan).
    ScanSkipped { rel_path: String },
    /// Scanning finished.
    ScanDone {
        valid_pngs: usize,
        already_optimized: usize,
        non_pngs: usize,
        to_process: usize,
    },
    /// A file's pixels were decoded and reduction applied.
    FileOpened {
        rel_path: String,
        width: u32,
        height: u32,
        bit_depth: u8,
        color_type: u8,
    },
    /// A file failed to decode / prepare.
    FileError { rel_path: String, message: String },
    /// Overall progress snapshot (drives the live progress bar in the CLI).
    Progress {
        completed_files: usize,
        total_files: usize,
        overall_frac: f64, // 0.0..=1.0
        savings_bytes: u64,
        savings_pct: f64,  // 0.0..=100.0
    },
    /// A file finished optimization.
    FileDone {
        rel_path: String,
        orig_bytes: u64,
        final_bytes: u64,
        reduction: Option<String>,
        skipped: bool,
    },
    /// The whole run finished (drives the summary block).
    Summary {
        files_processed: usize,
        total_orig_bytes: u64,
        total_new_bytes: u64,
        duration_secs: f64,
    },
    /// An unrecoverable setup error; the application should print + exit non-zero.
    Fatal { message: String },
}
