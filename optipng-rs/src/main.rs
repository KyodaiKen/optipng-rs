/// optipng-rs — CLI entry point.
///
/// Parses arguments and delegates all work to the library. The library is
/// UI-agnostic: it only emits `PipelineEvent`s through `event_callback`.
/// This binary owns all console rendering — spinners, progress bars, colors.

mod cli;

use std::sync::{Arc, Mutex};

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use liboptipng_rs::{
    color_type_short_name, format_bytes, format_duration, optimize_files, truncate_middle,
    OptimizationTask, PipelineEvent,
};

fn main() {
    let mut cli: OptimizationTask = cli::parse_args();

    if cli.show_help {
        cli::print_usage();
        std::process::exit(0);
    }

    if cli.files.is_empty() && cli.external_input.is_none() {
        if !cli.quiet {
            eprintln!("\nError: No input files specified.");
            cli::print_usage();
        }
        std::process::exit(1);
    }

    // Quiet mode = no callback installed, so the library emits nothing.
    if !cli.quiet {
        let ui = Arc::new(Mutex::new(Ui::new()));
        cli.event_callback = Some(Box::new(move |ev: &PipelineEvent| {
            ui.lock().unwrap().render(ev);
        }));
    }

    optimize_files(cli);
}

// ─── Console rendering ─────────────────────────────────────────────────────

fn terminal_width() -> usize {
    console::Term::stdout().size().1 as usize
}

/// Column widths for the overall progress bar (mirrors the old library-side
/// layout: ~50% for the status text, ~10 for the savings, rest for the bar).
fn column_widths(term_width: usize) -> (usize, usize, usize) {
    let w = term_width.max(60);
    let fn_w = (w as f64 * 0.50) as usize;
    let pct_w = 10;
    let used = fn_w + pct_w + 7;
    let bar_w = if w > used { w - used } else { 10 };
    (fn_w, bar_w, pct_w)
}

/// Owns all indicatif / console state for the lifetime of a run.
struct Ui {
    scan: Option<ProgressBar>,
    multi: Option<MultiProgress>,
    overall: Option<ProgressBar>,
}

impl Ui {
    fn new() -> Self {
        Self {
            scan: None,
            multi: None,
            overall: None,
        }
    }

    fn ensure_scan(&mut self) -> &ProgressBar {
        if self.scan.is_none() {
            let pb = ProgressBar::new_spinner();
            pb.enable_steady_tick(std::time::Duration::from_millis(100));
            let bar_w = terminal_width().saturating_sub(12).max(10);
            let template = format!("{{spinner:.dim}} {{msg}} \u{e0b2}{{bar:{bar_w}.cyan.bold/cyan}}\u{e0b0}");
            pb.set_style(
                ProgressStyle::with_template(&template)
                    .unwrap()
                    .tick_chars(".oOo.")
                    .progress_chars("█▉▊▋▌▍▎▏ "),
            );
            pb.set_message(format!("{:>6}", 0));
            self.scan = Some(pb);
        }
        self.scan.as_ref().unwrap()
    }

    fn ensure_multi(&mut self) -> &MultiProgress {
        if self.multi.is_none() {
            self.multi = Some(MultiProgress::new());
        }
        self.multi.as_ref().unwrap()
    }

    fn ensure_overall(&mut self) -> &ProgressBar {
        if self.overall.is_none() {
            let multi = self.ensure_multi();
            let pb = multi.add(ProgressBar::new(10000));
            pb.enable_steady_tick(std::time::Duration::from_millis(100));
            self.overall = Some(pb);
        }
        self.overall.as_ref().unwrap()
    }

    fn render(&mut self, ev: &PipelineEvent) {
        match ev {
            PipelineEvent::ScanTick { found } => {
                let pb = self.ensure_scan();
                pb.tick();
                pb.set_message(format!("{:>6}", found));
            }
            PipelineEvent::ScanNonPng { rel_path } => {
                let pb = self.ensure_scan();
                let error_mark = console::style("✖").red().bright();
                let _ = pb.println(format!("{} {} is not a valid PNG file", error_mark, rel_path));
            }
            PipelineEvent::ScanSkipped { rel_path } => {
                let pb = self.ensure_scan();
                let _ = pb.println(format!("- {} -> skipped (already optimized)", rel_path));
            }
            PipelineEvent::ScanDone {
                valid_pngs,
                already_optimized,
                non_pngs,
                to_process,
            } => {
                if let Some(pb) = self.scan.take() {
                    pb.finish_and_clear();
                }
                println!("\nSCAN COMPLETED:");
                println!("  Valid PNG files found  : {}", valid_pngs);
                println!("  Already optimized .... : {}", already_optimized);
                println!("  Non-PNG / invalid files: {}", non_pngs);
                println!("  Files to be processed  : {}\n", to_process);
            }
            PipelineEvent::FileOpened {
                rel_path,
                width,
                height,
                bit_depth,
                color_type,
            } => {
                let multi = self.ensure_multi();
                let color_name = color_type_short_name(*color_type);
                let _ = multi.println(format!(
                    "Opened {} -> {} x {} / {} bit / {}",
                    rel_path, width, height, bit_depth, color_name
                ));
            }
            PipelineEvent::FileError { rel_path, message } => {
                let multi = self.ensure_multi();
                let error_mark = console::style("✖").red().bright();
                let _ = multi.println(format!("{} {} - Error: {}", error_mark, rel_path, message));
            }
            PipelineEvent::Progress {
                completed_files,
                total_files,
                overall_frac,
                savings_bytes,
                savings_pct,
            } => {
                let pb = self.ensure_overall();
                let (fn_w, bar_w, pct_w) = column_widths(terminal_width());

                let status_str = format!(
                    "Overall Progress: {}/{} {:.2}%",
                    completed_files,
                    total_files,
                    overall_frac * 100.0
                );
                let truncated_status = truncate_middle(&status_str, fn_w);
                let msg = format!("{:<width$}", truncated_status, width = fn_w);

                let extra_str = format!("{} ({:.1}%)", format_bytes(*savings_bytes as usize), savings_pct);
                let extra = format!("{:>width$}", extra_str, width = pct_w);

                let template = format!("{{spinner:.cyan.bold}} {{msg}} \u{e0b2}{{bar:{bar_w}.cyan.bold/cyan}}\u{e0b0} {extra}");
                pb.set_style(
                    ProgressStyle::with_template(&template)
                        .unwrap()
                        .tick_chars(".oOo.")
                        .progress_chars("█▉▊▋▌▍▎▏ "),
                );
                pb.set_message(msg);

                let pos = (overall_frac * 10000.0) as u64;
                pb.set_position(pos.min(10000));
            }
            PipelineEvent::FileDone {
                rel_path,
                orig_bytes,
                final_bytes,
                reduction,
                skipped,
            } => {
                let multi = self.ensure_multi();
                if *skipped {
                    let _ = multi.println(format!("- {} -> skipped", rel_path));
                } else {
                    let sav_str = if *final_bytes < *orig_bytes {
                        let saved = orig_bytes - final_bytes;
                        let pct = (saved as f64 / *orig_bytes as f64) * 100.0;
                        format!("saved {} ({:.1}%)", format_bytes(saved as usize), pct)
                    } else if *final_bytes == *orig_bytes {
                        "no size reduction".to_string()
                    } else {
                        let diff = final_bytes - orig_bytes;
                        let pct = (diff as f64 / *orig_bytes as f64) * 100.0;
                        format!("+{} (+{:.1}%)", format_bytes(diff as usize), pct)
                    };
                    let red_str = reduction.clone().unwrap_or_else(|| "no reduction".to_string());
                    let check_mark = console::style("✓").green().bright();
                    let _ = multi.println(format!("{} {} [{}] -> {}", check_mark, rel_path, red_str, sav_str));
                }
            }
            PipelineEvent::Summary {
                files_processed,
                total_orig_bytes,
                total_new_bytes,
                duration_secs,
            } => {
                if let Some(multi) = self.multi.take() {
                    let _ = multi.clear();
                }
                if let Some(pb) = self.overall.take() {
                    pb.finish_and_clear();
                }
                let saved_bytes = total_orig_bytes.saturating_sub(*total_new_bytes);
                let pct = if *total_orig_bytes > 0 {
                    (saved_bytes as f64 / *total_orig_bytes as f64) * 100.0
                } else {
                    0.0
                };
                println!("\nSUMMARY OF PROCESSED FILES");
                println!("  Files processed ..... : {}", files_processed);
                println!(
                    "  Total original size . : {} bytes ({})",
                    total_orig_bytes,
                    format_bytes(*total_orig_bytes as usize)
                );
                println!(
                    "  Total new size ...... : {} bytes ({})",
                    total_new_bytes,
                    format_bytes(*total_new_bytes as usize)
                );
                println!(
                    "  Total size decrease . : {} bytes ({}) ({:.2}%)",
                    saved_bytes,
                    format_bytes(saved_bytes as usize),
                    pct
                );
                println!(
                    "  Total processing time : {}",
                    format_duration(std::time::Duration::from_secs_f64(*duration_secs))
                );
            }
            PipelineEvent::Fatal { message } => {
                let error_mark = console::style("✖").red().bright();
                eprintln!("{} {}", error_mark, message);
                std::process::exit(1);
            }
        }
    }
}
