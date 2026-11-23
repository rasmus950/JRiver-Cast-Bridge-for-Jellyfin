//! Which file a cast item means (SPEC.md "Find the file").
//!
//! The server path comes from the media source the sender picked
//! (`MediaSourceId`), else the item's first source, else the item itself.
//! Only files on the server's disk qualify: anything else would need a
//! stream, and the bridge never streams.

use crate::{
  jellyfin::{Item, MediaStream},
  path_map::MediaShape,
};

/// What to hand the path mapper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayTarget {
  pub server_path: String,
  pub shape: MediaShape,
  /// The media source being played, for the playback reports.
  pub media_source_id: String,
  /// The source's tracks, for picking the audio and subtitles (none for a
  /// disc folder).
  pub streams: Vec<MediaStream>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
  #[error("{name} is not a video (media type {media_type:?})")]
  NotVideo { name: String, media_type: String },
  #[error("{name} has no media source {media_source_id:?}")]
  NoSuchSource {
    name: String,
    media_source_id: String,
  },
  #[error("{name} is not a file on the server (protocol {protocol:?}); the bridge never streams")]
  NotAFile { name: String, protocol: String },
  #[error("{name} is a {video_type} image, which the bridge can't play yet (only files and Blu-ray folders)")]
  Unsupported { name: String, video_type: String },
  #[error("{name} has no file path")]
  NoPath { name: String },
}

pub fn select_target(
  item: &Item,
  media_source_id: Option<&str>,
) -> Result<PlayTarget, ResolveError> {
  let name = item.name.clone().unwrap_or_else(|| item.id.clone());
  if let Some(media_type) = item.media_type.as_deref() {
    if !media_type.eq_ignore_ascii_case("Video") {
      return Err(ResolveError::NotVideo {
        name,
        media_type: media_type.to_owned(),
      });
    }
  }

  let sources = item.media_sources.as_deref().unwrap_or_default();
  let requested = media_source_id.filter(|id| !id.is_empty());
  let source = match requested {
    Some(wanted) => Some(
      sources
        .iter()
        .find(|s| same_id(&s.id, wanted))
        .ok_or_else(|| ResolveError::NoSuchSource {
          name: name.clone(),
          media_source_id: wanted.to_owned(),
        })?,
    ),
    None => sources.first(),
  };
  let (path, protocol, video_type, source_id, streams) = match source {
    Some(source) => (
      source.path.as_deref(),
      source.protocol.as_deref(),
      source.video_type.as_deref(),
      source.id.clone(),
      source.media_streams.clone(),
    ),
    None => (
      item.path.as_deref(),
      None,
      item.video_type.as_deref(),
      item.id.clone(),
      Vec::new(),
    ),
  };

  if let Some(protocol) = protocol {
    if !protocol.eq_ignore_ascii_case("File") {
      return Err(ResolveError::NotAFile {
        name,
        protocol: protocol.to_owned(),
      });
    }
  }

  let shape = match video_type {
    None => MediaShape::File,
    Some(t) if t.eq_ignore_ascii_case("VideoFile") => MediaShape::File,
    Some(t) if t.eq_ignore_ascii_case("BluRay") => MediaShape::BluRayFolder,
    // `Iso` and `Dvd` (not played: SPEC.md "Path mapping"), or
    // anything newer.
    Some(t) => {
      return Err(ResolveError::Unsupported {
        name,
        video_type: t.to_owned(),
      })
    }
  };

  let server_path = path
    .map(str::trim)
    .filter(|p| !p.is_empty())
    .ok_or_else(|| ResolveError::NoPath { name: name.clone() })?;

  Ok(PlayTarget {
    server_path: server_path.to_owned(),
    shape,
    media_source_id: source_id,
    // How MC orders a disc's tracks isn't known: no track numbers for it.
    streams: if shape == MediaShape::File {
      streams
    } else {
      Vec::new()
    },
  })
}

/// IDs compare without case or dashes (`N` and `D` GUID formats).
fn same_id(a: &str, b: &str) -> bool {
  let strip = |s: &str| {
    s.chars()
      .filter(|c| *c != '-')
      .map(|c| c.to_ascii_lowercase())
      .collect::<String>()
  };
  strip(a) == strip(b)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::jellyfin::MediaSourceInfo;

  fn source(id: &str, path: &str, video_type: &str) -> MediaSourceInfo {
    MediaSourceInfo {
      id: id.to_owned(),
      path: Some(path.to_owned()),
      protocol: Some("File".to_owned()),
      video_type: Some(video_type.to_owned()),
      media_streams: Vec::new(),
    }
  }

  fn movie(sources: Vec<MediaSourceInfo>) -> Item {
    Item {
      id: "fc6b41643b6a10ebf8c0ef947a2d06c8".to_owned(),
      name: Some("Avatar".to_owned()),
      item_type: Some("Movie".to_owned()),
      media_type: Some("Video".to_owned()),
      path: Some("/media/movies/Avatar/a.mkv".to_owned()),
      video_type: Some("VideoFile".to_owned()),
      run_time_ticks: Some(1),
      media_sources: Some(sources),
    }
  }

  #[test]
  fn plays_the_first_source_by_default() {
    let item = movie(vec![
      source("aaaa", "/media/movies/Avatar/a.mkv", "VideoFile"),
      source("bbbb", "/media/movies/Avatar/b.mkv", "VideoFile"),
    ]);
    let target = select_target(&item, None).unwrap();
    assert_eq!(target.server_path, "/media/movies/Avatar/a.mkv");
    assert_eq!(target.shape, MediaShape::File);
    assert_eq!(target.media_source_id, "aaaa");
  }

  #[test]
  fn plays_the_requested_version() {
    let item = movie(vec![
      source("aaaa", "/media/movies/Avatar/a.mkv", "VideoFile"),
      source("bbbb-cccc", "/media/movies/Avatar/b.mkv", "VideoFile"),
    ]);
    let target = select_target(&item, Some("BBBBCCCC")).unwrap();
    assert_eq!(target.server_path, "/media/movies/Avatar/b.mkv");
    assert_eq!(target.media_source_id, "bbbb-cccc");
    assert!(matches!(
      select_target(&item, Some("dddd")),
      Err(ResolveError::NoSuchSource { .. })
    ));
  }

  #[test]
  fn blu_ray_folders_are_marked_and_get_no_track_list() {
    let stream = MediaStream {
      index: 1,
      kind: "Audio".to_owned(),
      ..MediaStream::default()
    };
    let mut disc = source("aaaa", "/media/movies/Star Trek/X", "BluRay");
    disc.media_streams = vec![stream.clone()];
    let target = select_target(&movie(vec![disc]), None).unwrap();
    assert_eq!(target.shape, MediaShape::BluRayFolder);
    assert_eq!(target.server_path, "/media/movies/Star Trek/X");
    assert!(
      target.streams.is_empty(),
      "MC's order for a disc isn't known"
    );

    let mut file = source("bbbb", "/media/movies/Avatar/a.mkv", "VideoFile");
    file.media_streams = vec![stream.clone()];
    let target = select_target(&movie(vec![file]), None).unwrap();
    assert_eq!(target.streams, vec![stream]);
  }

  #[test]
  fn isos_dvds_and_streams_are_refused() {
    for video_type in ["Iso", "Dvd"] {
      let item = movie(vec![source("aaaa", "/media/movies/x.iso", video_type)]);
      assert!(matches!(
        select_target(&item, None),
        Err(ResolveError::Unsupported { .. })
      ));
    }
    let mut stream = source("aaaa", "http://example/x", "VideoFile");
    stream.protocol = Some("Http".to_owned());
    assert!(matches!(
      select_target(&movie(vec![stream]), None),
      Err(ResolveError::NotAFile { .. })
    ));
  }

  #[test]
  fn falls_back_to_the_item_path() {
    let mut item = movie(vec![]);
    item.media_sources = None;
    let target = select_target(&item, None).unwrap();
    assert_eq!(target.server_path, "/media/movies/Avatar/a.mkv");
    assert_eq!(target.media_source_id, item.id);
    item.path = Some("  ".to_owned());
    assert!(matches!(
      select_target(&item, None),
      Err(ResolveError::NoPath { .. })
    ));
  }

  #[test]
  fn audio_is_refused() {
    let mut item = movie(vec![]);
    item.media_type = Some("Audio".to_owned());
    assert!(matches!(
      select_target(&item, None),
      Err(ResolveError::NotVideo { .. })
    ));
  }
}
