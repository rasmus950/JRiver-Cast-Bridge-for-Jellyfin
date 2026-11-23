use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

mod process;
mod rust;

#[derive(Parser)]
#[command(
  name = "cargo xtask",
  about = "jriver-cast-bridge build and maintenance tasks"
)]
struct Cli {
  #[command(subcommand)]
  command: Task,
}

#[derive(Subcommand)]
enum Task {
  /// Format check, clippy and the xtask tests.
  Check,
  /// Run every crate's tests.
  Test,
  Fmt {
    #[arg(long)]
    check: bool,
  },
  #[command(subcommand)]
  Rust(rust::Command),
}

pub fn root() -> &'static Path {
  Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/.."))
}

fn execute(cli: Cli) -> Result<()> {
  std::env::set_current_dir(root()).context("Entering repository root")?;
  match cli.command {
    Task::Check => rust::check_all(),
    Task::Test => rust::test_all(),
    Task::Fmt { check } => rust::format(check),
    Task::Rust(task) => rust::run(task),
  }
}

fn main() -> ExitCode {
  let cli = Cli::parse();
  let result = process::install_signal_handler().and_then(|()| execute(cli));
  match result {
    Ok(()) => ExitCode::SUCCESS,
    Err(error) => {
      eprintln!("{error:#}");
      if process::interrupted() {
        ExitCode::from(130)
      } else {
        ExitCode::FAILURE
      }
    }
  }
}
