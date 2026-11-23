//! Matching the file the bridge asked MC to play against what MC reports.

/// True when `reported` (MC's `Filename`) is the media the bridge requested.
///
/// Windows paths compare case-insensitively with either slash. A Blu-ray
/// requested as `...\BDMV\index.bdmv` is reported by MC as
/// `...\BDMV\index.bluray;<title>`, so both are reduced to the disc folder.
pub fn same_media(requested: &str, reported: &str) -> bool {
  let requested = normalize(requested);
  let reported = normalize(reported);
  if requested == reported {
    return true;
  }
  match (bluray_root(&requested), bluray_root(&reported)) {
    (Some(a), Some(b)) => a == b,
    _ => false,
  }
}

fn normalize(path: &str) -> String {
  path
    .trim()
    .replace('/', "\\")
    .trim_end_matches('\\')
    .to_lowercase()
}

/// The disc folder of a normalized Blu-ray path, if it is one.
fn bluray_root(path: &str) -> Option<&str> {
  if let Some(root) = path.strip_suffix(r"\bdmv\index.bdmv") {
    return Some(root);
  }
  let without_title = match path.rsplit_once(';') {
    Some((before, title)) if !title.is_empty() && title.bytes().all(|b| b.is_ascii_digit()) => {
      before
    }
    Some(_) => return None,
    None => path,
  };
  without_title.strip_suffix(r"\bdmv\index.bluray")
}

#[cfg(test)]
mod tests {
  use super::*;

  const DISC: &str = r"\\nas\movies\Star Trek\Star Trek Into Darkness\Star.Trek.Into.Darkness.2013.1080p.BluRay.AVC.TrueHD.7.1-PublicHD";

  #[test]
  fn identical_and_case_or_slash_variants_match() {
    let path = r"\\nas\movies\Avatar\Avatar.mkv";
    assert!(same_media(path, path));
    assert!(same_media(path, r"\\nas\MOVIES\avatar\AVATAR.MKV"));
    assert!(same_media(path, "//nas/movies/Avatar/Avatar.mkv"));
  }

  #[test]
  fn different_files_do_not_match() {
    assert!(!same_media(
      r"\\nas\movies\A\a.mkv",
      r"\\nas\movies\A\b.mkv"
    ));
    // Same file under a drive letter is a different library entry in MC.
    assert!(!same_media(r"\\nas\movies\a.mkv", r"M:\a.mkv"));
  }

  #[test]
  fn bluray_index_matches_mc_title_path() {
    // Observed on MC 36.0.51: index.bdmv is reported as index.bluray;1.
    let requested = format!(r"{DISC}\BDMV\index.bdmv");
    let reported = format!(r"{DISC}\BDMV\index.bluray;1");
    assert!(same_media(&requested, &reported));
    assert!(same_media(
      &requested,
      &format!(r"{DISC}\BDMV\index.bluray;12")
    ));
    assert!(same_media(
      &requested,
      &format!(r"{DISC}\BDMV\index.bluray")
    ));
  }

  #[test]
  fn bluray_of_another_disc_does_not_match() {
    let requested = format!(r"{DISC}\BDMV\index.bdmv");
    assert!(!same_media(
      &requested,
      r"\\nas\movies\Other\BDMV\index.bluray;1"
    ));
    assert!(!same_media(
      &requested,
      &format!(r"{DISC}\BDMV\index.bluray;x")
    ));
  }
}
