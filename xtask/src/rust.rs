use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{bail, Context, Result};
use clap::{Subcommand, ValueEnum};

use crate::process;

#[derive(Debug, Subcommand)]
pub enum Command {
  Fmt {
    #[arg(long)]
    check: bool,
  },
  Check {
    crates: Vec<Crate>,
  },
  Clippy {
    crates: Vec<Crate>,
  },
  /// Run tests with Linux XDG_CONFIG_HOME isolated in a temporary directory.
  Test {
    crates: Vec<Crate>,
  },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Crate {
  Jriver,
  Bridge,
  Xtask,
}

impl Crate {
  const ALL: [Self; 3] = [Self::Jriver, Self::Bridge, Self::Xtask];

  fn package(self) -> &'static str {
    match self {
      Self::Jriver => "jriver-mcws",
      Self::Bridge => "jriver-cast-bridge",
      Self::Xtask => "xtask",
    }
  }
}

fn packages(selected: &[Crate]) -> BTreeSet<&'static str> {
  let selected = if selected.is_empty() {
    &Crate::ALL
  } else {
    selected
  };
  selected.iter().map(|selector| selector.package()).collect()
}

fn cargo_command(action: &str, packages: &BTreeSet<&str>) -> std::process::Command {
  let mut command = process::cargo();
  command.args([action, "--manifest-path", "Cargo.toml"]);
  for package in packages {
    command.args(["--package", package]);
  }
  command
}

pub fn run(command: Command) -> Result<()> {
  match command {
    Command::Fmt { check } => format(check),
    Command::Check { crates } => run_selected("check", &crates),
    Command::Clippy { crates } => run_selected("clippy", &crates),
    Command::Test { crates } => run_selected("test", &crates),
  }
}

pub fn format(check: bool) -> Result<()> {
  let mut command = cargo_command("fmt", &packages(&[]));
  if check {
    command.args(["--", "--check"]);
  }
  process::run(&mut command)?;
  Ok(())
}

fn isolate_configuration(command: &mut std::process::Command, directory: &Path) {
  // dirs::config_dir honors this on Linux; native Windows/macOS paths are
  // unchanged. Do not change HOME and accidentally redirect toolchain state.
  command.env("XDG_CONFIG_HOME", directory);
}

fn run_selected(action: &str, selected: &[Crate]) -> Result<()> {
  let isolation = (action == "test")
    .then(|| {
      tempfile::Builder::new()
        .prefix("jriver-cast-bridge-test-config-")
        .tempdir()
    })
    .transpose()
    .context("Creating isolated test configuration")?;
  let mut command = cargo_command(action, &packages(selected));
  match action {
    "clippy" => {
      command.args([
        "--all-targets",
        "--all-features",
        "--no-deps",
        "--",
        "-D",
        "warnings",
      ]);
    }
    "test" => {
      if let Some(directory) = &isolation {
        isolate_configuration(&mut command, directory.path());
      }
    }
    "check" => {}
    _ => bail!("Unsupported Rust action: {action}"),
  }
  process::run(&mut command)?;
  Ok(())
}

pub fn check_all() -> Result<()> {
  let mut failed = Vec::new();
  for label in ["rust fmt --check", "rust clippy", "xtask tests"] {
    let result = match label {
      "rust fmt --check" => format(true),
      "rust clippy" => run_selected("clippy", &[]),
      _ => run_selected("test", &[Crate::Xtask]),
    };
    match result {
      Ok(()) => eprintln!("\n=== {label} [OK] ==="),
      Err(error) => {
        eprintln!("\n=== {label} [FAILED] ===\n{error:#}");
        failed.push(label);
      }
    }
    if process::interrupted() {
      bail!("Check interrupted after cleanup");
    }
  }
  if !failed.is_empty() {
    bail!("Check failed: {}", failed.join(", "));
  }
  Ok(())
}

pub fn test_all() -> Result<()> {
  run_selected("test", &[])
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn overlapping_selectors_are_deduplicated() {
    let selected = packages(&[Crate::Bridge, Crate::Jriver, Crate::Bridge]);
    assert_eq!(
      selected,
      BTreeSet::from(["jriver-cast-bridge", "jriver-mcws"])
    );
  }

  #[test]
  fn no_selection_means_every_maintained_crate() {
    let selected = packages(&[]);
    assert_eq!(selected.len(), Crate::ALL.len());
    assert!(selected.contains("xtask"));
  }
}
