//! Server path → HTPC path mapping (SPEC.md "Path mapping").
//!
//! Rules are ordered prefixes; the first match wins. Prefixes match whole
//! path components, case-insensitively, with either slash. Targets must be
//! UNC paths: MC imports every file it plays, and the same file under a drive
//! letter and a UNC path becomes two library entries.

use std::{fmt, io, time::Duration};

use serde::Deserialize;

/// One `[[path_map]]` entry from the config.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathRule {
  /// Server-side prefix as Jellyfin reports it, e.g. `/media/movies`.
  pub from: String,
  /// UNC prefix the HTPC opens, e.g. `\\nas\movies`.
  pub to: String,
}

/// What kind of media the server path points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaShape {
  /// A single file: MKV, M2TS, MP4, ISO, ...
  File,
  /// A Blu-ray folder (Jellyfin `VideoType` `BluRay`). Jellyfin's path is the
  /// release folder; MC needs `BDMV\index.bdmv` (the bare folder fails with
  /// HTTP 500).
  BluRayFolder,
}

impl MediaShape {
  /// From Jellyfin's `VideoType` (`VideoFile`, `Iso`, `Dvd`, `BluRay`).
  pub fn from_video_type(video_type: Option<&str>) -> Self {
    match video_type {
      Some(t) if t.eq_ignore_ascii_case("BluRay") => Self::BluRayFolder,
      _ => Self::File,
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MapError {
  #[error("path_map rule {index}: {reason}")]
  InvalidRule { index: usize, reason: String },
  #[error("no path_map rule matches the server path {server_path:?}")]
  NoRule { server_path: String },
  #[error("server path {server_path:?} can't be mapped safely: {reason}")]
  Unsafe { server_path: String, reason: String },
}

#[derive(Debug, Clone)]
struct Rule {
  /// `from`, split into components, lower-cased for matching.
  from_components: Vec<String>,
  /// `to`, with backslashes and no trailing separator.
  to: String,
}

/// The ordered rule list.
#[derive(Debug, Clone)]
pub struct PathMapper {
  rules: Vec<Rule>,
}

impl PathMapper {
  pub fn new(rules: &[PathRule]) -> Result<Self, MapError> {
    let rules = rules
      .iter()
      .enumerate()
      .map(|(index, rule)| {
        let invalid = |reason: &str| MapError::InvalidRule {
          index,
          reason: format!("{reason} (from = {:?}, to = {:?})", rule.from, rule.to),
        };
        let from_components = components(&rule.from);
        if from_components.is_empty() {
          return Err(invalid("`from` is empty"));
        }
        if from_components.iter().any(|c| c == "." || c == "..") {
          return Err(invalid("`from` must not contain . or .."));
        }
        let to = rule.to.trim().replace('/', "\\");
        let to = to.trim_end_matches('\\');
        if !is_unc(to) {
          return Err(invalid(
            "`to` must be a UNC path like \\\\server\\share (drive letters create duplicate MC library entries)",
          ));
        }
        Ok(Rule {
          from_components: from_components.iter().map(|c| c.to_lowercase()).collect(),
          to: to.to_owned(),
        })
      })
      .collect::<Result<Vec<_>, _>>()?;
    Ok(Self { rules })
  }

  pub fn is_empty(&self) -> bool {
    self.rules.is_empty()
  }

  /// Map a server path to the path JRiver should open.
  pub fn map(&self, server_path: &str, shape: MediaShape) -> Result<String, MapError> {
    let parts = components(server_path);
    let rule = self
      .rules
      .iter()
      .find(|rule| {
        parts.len() >= rule.from_components.len()
          && rule
            .from_components
            .iter()
            .zip(&parts)
            .all(|(from, part)| *from == part.to_lowercase())
      })
      .ok_or_else(|| MapError::NoRule {
        server_path: server_path.to_owned(),
      })?;

    let rest = &parts[rule.from_components.len()..];
    for part in rest {
      if part == "." || part == ".." {
        return Err(MapError::Unsafe {
          server_path: server_path.to_owned(),
          reason: "contains . or ..".to_owned(),
        });
      }
      if let Some(bad) = part
        .chars()
        .find(|c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') || c.is_control())
      {
        return Err(MapError::Unsafe {
          server_path: server_path.to_owned(),
          reason: format!("component {part:?} contains {bad:?}, which Windows paths can't hold"),
        });
      }
    }

    let mut mapped = rule.to.clone();
    for part in rest {
      mapped.push('\\');
      mapped.push_str(part);
    }
    if shape == MediaShape::BluRayFolder {
      let lower = mapped.to_lowercase();
      if lower.ends_with(r"\bdmv\index.bdmv") {
        // Already points at the index.
      } else if lower.ends_with(r"\bdmv") {
        mapped.push_str(r"\index.bdmv");
      } else {
        mapped.push_str(r"\BDMV\index.bdmv");
      }
    }
    Ok(mapped)
  }
}

/// Non-empty path components, splitting on either slash.
fn components(path: &str) -> Vec<String> {
  path
    .trim()
    .split(['/', '\\'])
    .filter(|c| !c.is_empty())
    .map(str::to_owned)
    .collect()
}

fn is_unc(path: &str) -> bool {
  let Some(rest) = path.strip_prefix(r"\\") else {
    return false;
  };
  let mut parts = rest.split('\\');
  // `\\?\C:\…` and `\\.\C:\…` are Windows device paths to a drive letter,
  // not network shares.
  matches!(
    (parts.next(), parts.next()),
    (Some(server), Some(share))
      if !server.is_empty()
        && !matches!(server, "?" | ".")
        && !server.contains(':')
        && !share.is_empty()
        && !share.contains(':')
  )
}

/// Why a mapped path can't be played.
#[derive(Debug, thiserror::Error)]
pub enum ExistsError {
  #[error("{path} does not exist (or the share denies access)")]
  Missing { path: String },
  #[error("checking {path} took longer than {timeout:?} (is the share reachable?)")]
  Timeout { path: String, timeout: Duration },
  #[error(
    "can't log in to the share for {path}: Windows' saved login for the file server was rejected (open the share in Explorer and log in again)"
  )]
  ShareLogin { path: String },
  #[error("checking {path} failed: {source}")]
  Io {
    path: String,
    #[source]
    source: io::Error,
  },
}

/// Windows `ERROR_LOGON_FAILURE`: "The user name or password is incorrect."
const ERROR_LOGON_FAILURE: i32 = 1326;

/// Check that `path` exists without blocking the async runtime. A
/// disconnected share can stall the filesystem call, hence the timeout.
pub async fn check_exists(path: &str, timeout: Duration) -> Result<(), ExistsError> {
  match tokio::time::timeout(timeout, tokio::fs::try_exists(path)).await {
    Ok(Ok(true)) => Ok(()),
    Ok(Ok(false)) => Err(ExistsError::Missing {
      path: path.to_owned(),
    }),
    Ok(Err(source)) if source.raw_os_error() == Some(ERROR_LOGON_FAILURE) => {
      Err(ExistsError::ShareLogin {
        path: path.to_owned(),
      })
    }
    Ok(Err(source)) => Err(ExistsError::Io {
      path: path.to_owned(),
      source,
    }),
    Err(_) => Err(ExistsError::Timeout {
      path: path.to_owned(),
      timeout,
    }),
  }
}

impl fmt::Display for MediaShape {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(match self {
      Self::File => "file",
      Self::BluRayFolder => "Blu-ray folder",
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn rule(from: &str, to: &str) -> PathRule {
    PathRule {
      from: from.to_owned(),
      to: to.to_owned(),
    }
  }

  /// Rules as in the example config.
  fn htpc() -> PathMapper {
    PathMapper::new(&[
      rule("/media/movies", r"\\nas\movies"),
      rule("/media/tv", r"\\nas\tv"),
    ])
    .unwrap()
  }

  #[test]
  fn maps_the_recon_test_items() {
    let m = htpc();
    assert_eq!(
      m.map(
        "/media/movies/Avatar - Fire and Ash (2025)/Avatar.Fire.And.Ash.2025.2160p.UHD.BluRay.REMUX.DV.P7.HDR.MULTi.TrueHD.Atmos.H265-BEN.THE.MEN/Avatar.Fire.And.Ash.2025.2160p.UHD.BluRay.REMUX.DV.P7.HDR.MULTi[Ben The Men].mkv",
        MediaShape::File
      )
      .unwrap(),
      r"\\nas\movies\Avatar - Fire and Ash (2025)\Avatar.Fire.And.Ash.2025.2160p.UHD.BluRay.REMUX.DV.P7.HDR.MULTi.TrueHD.Atmos.H265-BEN.THE.MEN\Avatar.Fire.And.Ash.2025.2160p.UHD.BluRay.REMUX.DV.P7.HDR.MULTi[Ben The Men].mkv"
    );
    assert_eq!(
      m.map(
        "/media/tv/Private Eyes West Coast/Season 1/Private Eyes West Coast - S01E02 - Shroom with a View WEBDL-1080p.mkv",
        MediaShape::File
      )
      .unwrap(),
      r"\\nas\tv\Private Eyes West Coast\Season 1\Private Eyes West Coast - S01E02 - Shroom with a View WEBDL-1080p.mkv"
    );
    assert_eq!(
      m.map(
        "/media/movies/Star Trek/Star Trek Into Darkness/Star.Trek.Into.Darkness.2013.1080p.BluRay.AVC.TrueHD.7.1-PublicHD",
        MediaShape::BluRayFolder
      )
      .unwrap(),
      r"\\nas\movies\Star Trek\Star Trek Into Darkness\Star.Trek.Into.Darkness.2013.1080p.BluRay.AVC.TrueHD.7.1-PublicHD\BDMV\index.bdmv"
    );
  }

  #[test]
  fn iso_is_a_plain_file() {
    assert_eq!(
      htpc()
        .map("/media/movies/Film/film.iso", MediaShape::File)
        .unwrap(),
      r"\\nas\movies\Film\film.iso"
    );
  }

  #[test]
  fn first_matching_rule_wins() {
    let m = PathMapper::new(&[
      rule("/media/movies/4k", r"\\nas\uhd"),
      rule("/media/movies", r"\\nas\movies"),
      rule("/media/movies/4k", r"\\never\used"),
    ])
    .unwrap();
    assert_eq!(
      m.map("/media/movies/4k/a.mkv", MediaShape::File).unwrap(),
      r"\\nas\uhd\a.mkv"
    );
    assert_eq!(
      m.map("/media/movies/hd/a.mkv", MediaShape::File).unwrap(),
      r"\\nas\movies\hd\a.mkv"
    );
  }

  #[test]
  fn prefix_matches_whole_components_only() {
    let err = htpc()
      .map("/media/movies2/a.mkv", MediaShape::File)
      .unwrap_err();
    assert!(matches!(err, MapError::NoRule { .. }), "{err:?}");
    assert!(matches!(
      htpc().map("/srv/other/a.mkv", MediaShape::File),
      Err(MapError::NoRule { .. })
    ));
  }

  #[test]
  fn normalises_slashes_case_and_trailing_separators() {
    let m = PathMapper::new(&[rule("/Media/Movies/", "//nas/movies/")]).unwrap();
    assert_eq!(
      m.map(r"\media\MOVIES\\A//b.mkv", MediaShape::File).unwrap(),
      r"\\nas\movies\A\b.mkv"
    );
    // The prefix itself maps to the share root.
    assert_eq!(
      m.map("/media/movies", MediaShape::File).unwrap(),
      r"\\nas\movies"
    );
  }

  #[test]
  fn keeps_the_case_of_the_remainder() {
    assert_eq!(
      htpc()
        .map("/MEDIA/Movies/Some Film/Some.Film.MKV", MediaShape::File)
        .unwrap(),
      r"\\nas\movies\Some Film\Some.Film.MKV"
    );
  }

  #[test]
  fn bluray_index_is_not_appended_twice() {
    let m = htpc();
    assert_eq!(
      m.map(
        "/media/movies/Disc/BDMV/index.bdmv",
        MediaShape::BluRayFolder
      )
      .unwrap(),
      r"\\nas\movies\Disc\BDMV\index.bdmv"
    );
    assert_eq!(
      m.map("/media/movies/Disc/BDMV", MediaShape::BluRayFolder)
        .unwrap(),
      r"\\nas\movies\Disc\BDMV\index.bdmv"
    );
  }

  #[test]
  fn rejects_traversal_and_characters_windows_cant_hold() {
    for bad in [
      "/media/movies/../secret.mkv",
      "/media/movies/./a.mkv",
      "/media/movies/a|b.mkv",
      "/media/movies/what?.mkv",
      "/media/movies/c:d.mkv",
      "/media/movies/a\u{7}.mkv",
    ] {
      assert!(
        matches!(
          htpc().map(bad, MediaShape::File),
          Err(MapError::Unsafe { .. })
        ),
        "{bad:?} should be refused"
      );
    }
  }

  #[test]
  fn rules_must_target_unc_and_have_a_source() {
    for (from, to) in [
      ("/media/movies", r"M:\"),
      ("/media/movies", r"M:\movies"),
      ("/media/movies", r"\\nas"),
      ("/media/movies", r"\\?\C:\Movies"),
      ("/media/movies", r"\\.\C:\Movies"),
      ("/media/movies", r"\\?\UNC:\x"),
      ("/media/movies", r"\\\movies"),
      ("/media/movies", ""),
      ("", r"\\nas\movies"),
      ("/", r"\\nas\movies"),
      ("/media/../movies", r"\\nas\movies"),
    ] {
      assert!(
        matches!(
          PathMapper::new(&[rule(from, to)]),
          Err(MapError::InvalidRule { index: 0, .. })
        ),
        "{from:?} -> {to:?} should be rejected"
      );
    }
  }

  #[test]
  fn no_rules_means_nothing_maps() {
    let m = PathMapper::new(&[]).unwrap();
    assert!(m.is_empty());
    assert!(matches!(
      m.map("/media/movies/a.mkv", MediaShape::File),
      Err(MapError::NoRule { .. })
    ));
  }

  #[test]
  fn shape_from_jellyfin_video_type() {
    assert_eq!(
      MediaShape::from_video_type(Some("BluRay")),
      MediaShape::BluRayFolder
    );
    assert_eq!(
      MediaShape::from_video_type(Some("bluray")),
      MediaShape::BluRayFolder
    );
    assert_eq!(
      MediaShape::from_video_type(Some("VideoFile")),
      MediaShape::File
    );
    assert_eq!(MediaShape::from_video_type(Some("Iso")), MediaShape::File);
    assert_eq!(MediaShape::from_video_type(None), MediaShape::File);
  }

  #[tokio::test]
  async fn existence_check_finds_files_and_reports_missing_ones() {
    let dir = std::env::temp_dir();
    let file = dir.join(format!("bridge-exists-{}.tmp", std::process::id()));
    std::fs::write(&file, b"x").unwrap();
    let path = file.to_string_lossy().into_owned();
    check_exists(&path, Duration::from_secs(5)).await.unwrap();
    std::fs::remove_file(&file).unwrap();
    assert!(matches!(
      check_exists(&path, Duration::from_secs(5)).await,
      Err(ExistsError::Missing { .. })
    ));
  }
}
