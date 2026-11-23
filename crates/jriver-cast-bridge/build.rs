//! Gives the Windows exes their icon (Start menu, Add or Remove Programs,
//! Explorer). The image comes from `src/icon.rs`, the tray icon's drawing;
//! this writes it as a compiled Windows resource (`.res`), which the MSVC
//! linker takes as an input, so no resource compiler is needed.

#![forbid(unsafe_code)]

use std::{env, fs, path::PathBuf};

#[path = "src/icon.rs"]
mod icon;

/// The sizes Windows picks from (small icons up to the large Explorer view).
const SIZES: [u32; 7] = [16, 20, 24, 32, 48, 64, 256];
const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
/// The group's resource ID: the lowest, so Windows shows it for the exe.
const GROUP_ID: u16 = 1;
const LANG_NEUTRAL: u16 = 0;

fn main() {
  println!("cargo:rerun-if-changed=build.rs");
  println!("cargo:rerun-if-changed=src/icon.rs");
  build_info();
  let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
  let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
  if target_os != "windows" || target_env != "msvc" {
    return;
  }
  let out = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
  let res = out.join("icon.res");
  fs::write(&res, icon_res()).expect("write icon.res");
  println!("cargo:rustc-link-arg-bins={}", res.display());
}

/// For the About window: `BRIDGE_COMMIT` (short Git commit, `+` when the
/// tree had changes) and `BRIDGE_BUILD_DATE` (UTC, `YYYY-MM-DD`).
fn build_info() {
  // A new commit or checkout changes HEAD or the index. (In a worktree
  // `.git` is a file: watching it would rerun this on every build.)
  if std::path::Path::new("../../.git").is_dir() {
    for git in ["../../.git/HEAD", "../../.git/index"] {
      println!("cargo:rerun-if-changed={git}");
    }
  }
  let git = |args: &[&str]| {
    std::process::Command::new("git")
      .args(args)
      .output()
      .ok()
      .filter(|out| out.status.success())
      .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
  };
  let commit = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".to_owned());
  // `--no-optional-locks`: don't rewrite the index (which would rerun this).
  let dirty = git(&[
    "--no-optional-locks",
    "status",
    "--porcelain",
    "--untracked-files=no",
  ])
  .is_some_and(|status| !status.is_empty());
  println!(
    "cargo:rustc-env=BRIDGE_COMMIT={commit}{}",
    if dirty { "+" } else { "" }
  );
  let days = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map_or(0, |since| since.as_secs() / 86_400);
  let (year, month, day) = civil_from_days(days as i64);
  println!("cargo:rustc-env=BRIDGE_BUILD_DATE={year:04}-{month:02}-{day:02}");
}

/// Days since 1970-01-01 to a calendar date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
  let z = days + 719_468;
  let era = z.div_euclid(146_097);
  let doe = z.rem_euclid(146_097);
  let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
  let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
  let mp = (5 * doy + 2) / 153;
  let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
  let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
  let year = yoe + era * 400 + i64::from(month <= 2);
  (year, month, day)
}

/// A `.res` file holding one icon per size and the icon group naming them.
fn icon_res() -> Vec<u8> {
  let mut res = Vec::new();
  // A .res file starts with an empty entry.
  resource(&mut res, 0, 0, 0, &[]);
  let mut group = Vec::new();
  group.extend(0u16.to_le_bytes()); // reserved
  group.extend(1u16.to_le_bytes()); // type: icon
  group.extend((SIZES.len() as u16).to_le_bytes());
  for (i, size) in SIZES.into_iter().enumerate() {
    let id = i as u16 + 1;
    let image = dib(size);
    resource(&mut res, RT_ICON, id, 0x1010, &image);
    // GRPICONDIRENTRY: 256 is written as 0.
    let side = if size >= 256 { 0 } else { size as u8 };
    group.extend([side, side, 0, 0]);
    group.extend(1u16.to_le_bytes()); // planes
    group.extend(32u16.to_le_bytes()); // bits per pixel
    group.extend((image.len() as u32).to_le_bytes());
    group.extend(id.to_le_bytes());
  }
  resource(&mut res, RT_GROUP_ICON, GROUP_ID, 0x1030, &group);
  res
}

/// One resource entry with a numeric type and name, padded to 4 bytes.
fn resource(res: &mut Vec<u8>, kind: u16, name: u16, flags: u16, data: &[u8]) {
  res.extend((data.len() as u32).to_le_bytes()); // data size
  res.extend(32u32.to_le_bytes()); // header size
  res.extend(0xFFFFu16.to_le_bytes());
  res.extend(kind.to_le_bytes());
  res.extend(0xFFFFu16.to_le_bytes());
  res.extend(name.to_le_bytes());
  res.extend(0u32.to_le_bytes()); // data version
  res.extend(flags.to_le_bytes());
  res.extend(LANG_NEUTRAL.to_le_bytes());
  res.extend(0u32.to_le_bytes()); // version
  res.extend(0u32.to_le_bytes()); // characteristics
  res.extend(data);
  while !res.len().is_multiple_of(4) {
    res.push(0);
  }
}

/// The badge as an icon image: a 32-bit bottom-up DIB (BGRA, alpha used)
/// followed by an all-zero AND mask, as icons store it.
fn dib(size: u32) -> Vec<u8> {
  let rgba = icon::badge_rgba(size);
  let n = size as usize;
  let mask_row = n.div_ceil(32) * 4;
  let mut image = Vec::with_capacity(40 + n * n * 4 + mask_row * n);
  image.extend(40u32.to_le_bytes()); // BITMAPINFOHEADER size
  image.extend((size as i32).to_le_bytes());
  image.extend((2 * size as i32).to_le_bytes()); // colour + mask
  image.extend(1u16.to_le_bytes()); // planes
  image.extend(32u16.to_le_bytes()); // bits per pixel
  image.extend(0u32.to_le_bytes()); // BI_RGB
  image.extend(((n * n * 4 + mask_row * n) as u32).to_le_bytes());
  image.extend([0u8; 16]); // resolution and palette: unused
  for y in (0..n).rev() {
    for x in 0..n {
      let i = (y * n + x) * 4;
      image.extend([rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
    }
  }
  image.resize(image.len() + mask_row * n, 0);
  image
}
