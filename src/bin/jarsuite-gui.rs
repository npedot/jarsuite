// no console window on Windows: the tools it launches open their own if they need one
#![windows_subsystem = "windows"]

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use jarsuite::install::{Root, root_dir};

/// Window to update and launch a suite of Java tools released together.
#[derive(Parser)]
#[command(name = "jarsuite-gui", version, about)]
struct Cli {
    /// Installation folder (default: $JARSUITE_ROOT, then the folder of this executable)
    #[arg(long)]
    root: Option<PathBuf>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match root_dir(cli.root).and_then(|dir| jarsuite::gui::run(Root::new(dir))) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("jarsuite-gui: {err:#}");
            ExitCode::FAILURE
        }
    }
}
