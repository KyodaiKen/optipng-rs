/*************************************
* optipng-rs: Shared data structures *
************************************+*/

#[derive(Debug, Clone)]
pub struct TrialConfig {
    pub zc: i32,
    pub zm: i32,
    pub zs: i32,
    pub f: u8,
}

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
    pub out_dir: Option<String>,
    pub show_help: bool,
    pub force_trials: bool,
    pub force_reenc: bool,
    pub cmd_options: String,
}

pub struct LoadedImage {
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    pub color_type: u8,
    pub stride: usize,
    pub raw_pixels: Vec<u8>,
    pub orig_idat_size: usize,
}