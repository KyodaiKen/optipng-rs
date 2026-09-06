/***************************************************
 * optipng-rs: Command-line parsing and definitions *
 *************************************************+*/

use crate::models::CliArgs;
use crate::utils::{parse_ranges_i32, parse_ranges_u8};
use std::env;

pub fn parse_args() -> CliArgs {
    let raw_args: Vec<String> = env::args().skip(1).collect();
    let mut cli = CliArgs {
        files: Vec::new(),
        external_input: None,
        opt_level: 2,
        mt: 0,
        zi: 1,
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
                cmd_options: String::new(),
    };

    let mut opt_tokens: Vec<String> = Vec::new();
    let mut i = 0;

    while i < raw_args.len() {
        let arg = &raw_args[i];

        if arg == "-h" || arg == "--help" {
            cli.show_help = true;
            return cli;
        }

        if arg == "-e" {
            opt_tokens.push(arg.clone());
            i += 1;
            if i < raw_args.len() {
                cli.external_input = Some(raw_args[i].clone());
                i += 1;
            } else {
                eprintln!("Error: Option -e requires an input file argument.");
                cli.show_help = true;
                return cli;
            }
            continue;
        }

        if arg == "-out" {
            opt_tokens.push(arg.clone());
            i += 1;
            if i < raw_args.len() {
                cli.out_file = Some(raw_args[i].clone());
                i += 1;
            }
            continue;
        }

        if arg == "-R" {
            cli.recursive = true;
            opt_tokens.push(arg.clone());
            i += 1;
            if i < raw_args.len() && raw_args[i].chars().all(|c| c.is_ascii_digit()) {
                cli.max_depth = raw_args[i].parse::<usize>().ok();
                opt_tokens.push(raw_args[i].clone());
                i += 1;
            }
            continue;
        }

        if arg.starts_with("-R") && arg.len() > 2 && arg[2..].chars().all(|c| c.is_ascii_digit()) {
            cli.recursive = true;
            cli.max_depth = arg[2..].parse::<usize>().ok();
            opt_tokens.push(arg.clone());
            i += 1;
            continue;
        }

        if arg == "-ft" {
            cli.force_trials = true;
            opt_tokens.push(arg.clone());
            i += 1;
            continue;
        }

        if arg == "-force" {
            cli.force_reenc = true;
            opt_tokens.push(arg.clone());
            i += 1;
            continue;
        }

        if arg.starts_with("-o") && arg.len() > 2 && arg[2..].chars().all(|c| c.is_ascii_digit()) {
            let level: u8 = arg[2..].parse().unwrap_or(2);
            cli.opt_level = level.min(7);
            opt_tokens.push(arg.clone());
            i += 1;
            continue;
        }

        if arg.starts_with("-mt") && arg.len() > 3 && arg[3..].chars().all(|c| c.is_ascii_digit()) {
            cli.mt = arg[3..].parse().unwrap_or(0);
            opt_tokens.push(arg.clone());
            i += 1;
            continue;
        }

        if arg.starts_with("-zi") {
            let val_str = if arg.len() > 3 {
                opt_tokens.push(arg.clone());
                Some(arg[3..].to_string())
            } else {
                opt_tokens.push(arg.clone());
                i += 1;
                if i < raw_args.len() {
                    opt_tokens.push(raw_args[i].clone());
                    Some(raw_args[i].clone())
                } else {
                    None
                }
            };

            if let Some(v) = val_str {
                if v.contains(',') || v.contains('-') {
                    eprintln!("Error: Options like -zi1,2 or -zi1-2 are not allowed. Select either -zi1 or -zi2.");
                    std::process::exit(1);
                }
                match v.parse::<u8>() {
                    Ok(1) => cli.zi = 1,
                    Ok(2) => cli.zi = 2,
                    _ => {
                        eprintln!("Error: Invalid value for -zi. Supported values are 1 (zlib) or 2 (Zöpfli).");
                        std::process::exit(1);
                    }
                }
            }
            i += 1;
            continue;
        }

        if arg.starts_with("-zc") {
            let val_str = if arg.len() > 3 {
                opt_tokens.push(arg.clone());
                Some(arg[3..].to_string())
            } else {
                opt_tokens.push(arg.clone());
                i += 1;
                if i < raw_args.len() {
                    opt_tokens.push(raw_args[i].clone());
                    Some(raw_args[i].clone())
                } else {
                    None
                }
            };
            if let Some(v) = val_str {
                let mut parsed = parse_ranges_i32(&v);
                cli.zc.get_or_insert_with(Vec::new).append(&mut parsed);
            }
            i += 1;
            continue;
        }

        if arg.starts_with("-zm") {
            let val_str = if arg.len() > 3 {
                opt_tokens.push(arg.clone());
                Some(arg[3..].to_string())
            } else {
                opt_tokens.push(arg.clone());
                i += 1;
                if i < raw_args.len() {
                    opt_tokens.push(raw_args[i].clone());
                    Some(raw_args[i].clone())
                } else {
                    None
                }
            };
            if let Some(v) = val_str {
                let mut parsed = parse_ranges_i32(&v);
                cli.zm.get_or_insert_with(Vec::new).append(&mut parsed);
            }
            i += 1;
            continue;
        }

        if arg.starts_with("-zs") {
            let val_str = if arg.len() > 3 {
                opt_tokens.push(arg.clone());
                Some(arg[3..].to_string())
            } else {
                opt_tokens.push(arg.clone());
                i += 1;
                if i < raw_args.len() {
                    opt_tokens.push(raw_args[i].clone());
                    Some(raw_args[i].clone())
                } else {
                    None
                }
            };
            if let Some(v) = val_str {
                let mut parsed = parse_ranges_i32(&v);
                cli.zs.get_or_insert_with(Vec::new).append(&mut parsed);
            }
            i += 1;
            continue;
        }

        if arg.starts_with("-f") {
            let val_str = if arg.len() > 2 {
                opt_tokens.push(arg.clone());
                Some(arg[2..].to_string())
            } else {
                opt_tokens.push(arg.clone());
                i += 1;
                if i < raw_args.len() {
                    opt_tokens.push(raw_args[i].clone());
                    Some(raw_args[i].clone())
                } else {
                    None
                }
            };
            if let Some(v) = val_str {
                let mut parsed = parse_ranges_u8(&v);
                cli.f.get_or_insert_with(Vec::new).append(&mut parsed);
            }
            i += 1;
            continue;
        }

        match arg.as_str() {
            "-o" => {
                opt_tokens.push(arg.clone());
                i += 1;
                if i < raw_args.len() {
                    opt_tokens.push(raw_args[i].clone());
                    let level: u8 = raw_args[i].parse().unwrap_or(2);
                    cli.opt_level = level.min(7);
                    i += 1;
                }
            }
            "-mt" => {
                opt_tokens.push(arg.clone());
                i += 1;
                if i < raw_args.len() {
                    opt_tokens.push(raw_args[i].clone());
                    cli.mt = raw_args[i].parse().unwrap_or(0);
                    i += 1;
                }
            }
            "-backup" | "-keep" => { cli.backup = true; opt_tokens.push(arg.clone()); i += 1; }
            "-simulate" => { cli.simulate = true; opt_tokens.push(arg.clone()); i += 1; }
            "-quiet" | "-silent" => { cli.quiet = true; opt_tokens.push(arg.clone()); i += 1; }
            "-nc" => { cli.nc = true; opt_tokens.push(arg.clone()); i += 1; }
            "-nb" => { cli.nb = true; opt_tokens.push(arg.clone()); i += 1; }
            "-np" => { cli.np = true; opt_tokens.push(arg.clone()); i += 1; }
            "-nx" => { cli.nx = true; opt_tokens.push(arg.clone()); i += 1; }
            "-nz" => { cli.nz = true; opt_tokens.push(arg.clone()); i += 1; }
            "--" => {
                opt_tokens.push(arg.clone());
                i += 1;
                cli.files.extend(raw_args[i..].iter().cloned());
                break;
            }
            _ => {
                if arg.starts_with('-') {
                    opt_tokens.push(arg.clone());
                } else {
                    cli.files.push(arg.clone());
                }
                i += 1;
            }
        }
    }

    if cli.opt_level == 0 {
        cli.nx = true;
        cli.nz = true;
    }

    cli.cmd_options = opt_tokens.join(" ");

    if let Some(ref mut list) = cli.zc { list.sort_unstable(); list.dedup(); }
    if let Some(ref mut list) = cli.zm { list.sort_unstable(); list.dedup(); }
    if let Some(ref mut list) = cli.zs { list.sort_unstable(); list.dedup(); }
    if let Some(ref mut list) = cli.f  { list.sort_unstable(); list.dedup(); }

    if cli.mt == 0 {
        let available = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        cli.mt = ((available * 3) / 4).max(1);
    }
    cli
}

pub fn print_usage() {
    let version = env!("CARGO_PKG_VERSION");
    println!(
        r#"optipng-rs v{version} - High-performance parallel PNG optimizer and converter

        USAGE:
        optipng-rs [options] <file1.png> [file2.png ...]
        optipng-rs [options] . [-R[depth]]
        optipng-rs [options] -e <input_file> [output.png]

        GENERAL OPTIONS:
        -h, --help         Print this help message
        -quiet, -silent    Quiet mode (suppress non-error output)
    -mt <threads>      Number of worker threads (default: 75% of CPUs)
    -R[N], -R [N]      Recursively scan directories for *.png files (N = max depth)
    -backup, -keep     Keep a backup copy of original files (.bak)
    -simulate          Simulation mode (run trials only, skip writing files)
    -force             Force file write even if compressed size increases
    -ft                Force trials even if file was previously optimized

    OPTIMIZATION OPTIONS:
    -o <level>         Optimization level 0-7 (default: 2)
    -zi <1|2>          Encoder implementation: 1 = zlib (default), 2 = Zöpfli
    -zc <levels>       zlib compression levels (e.g., -zc1-9)
    -zm <levels>       zlib memory levels (e.g., -zm1-9)
    -zs <strategies>   zlib compression strategies (e.g., -zs0-3)
    -f <filters>       PNG delta filter algorithms (e.g., -f0,5)
    -nz                No IDAT recoding (fast path / zero re-compression)

    INPUT / OUTPUT OPTIONS:
    -e <file>          External input file format (TGA, PPM, PGM, PAM)
    -out <path>        Output file path (single file) or output directory (multi-file)"#
    );
}