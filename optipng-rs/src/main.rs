/// optipng-rs — CLI entry point.
/// Parses arguments and delegates all work to the library.

mod cli;

use liboptipng_rs::OptimizationTask;
use liboptipng_rs::run_optimization;

fn main() {
    let cli: OptimizationTask = cli::parse_args();

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

    run_optimization(cli);
}
