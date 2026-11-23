//! Mapping Jellyfin's audio and subtitle choice to JRiver's track commands
//! (SPEC.md "Select tracks").
//!
//! Jellyfin numbers a file's streams across all of them (video, audio,
//! subtitles, then external subtitle files), and -1 means no subtitles.
//! JRiver's `MCC_SET_AUDIO_STREAM` and `MCC_SET_SUBTITLES` take a track's
//! position among the file's tracks of its kind. Measured on
//! MC 36: audio tracks count from 0, subtitle tracks
//! from 1, and subtitles 0 turns them off. How MC lists external subtitle
//! files isn't known, so with one beside the file only "off" is sent for
//! subtitles. Disc folders aren't mapped at all (see the controller).

use crate::jellyfin::MediaStream;

/// Jellyfin's subtitle index for "no subtitles".
pub const NO_SUBTITLES: i32 = -1;
/// JRiver's subtitle number for "off".
const JRIVER_SUBTITLES_OFF: i32 = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
  Audio,
  Subtitle,
}

impl TrackKind {
  fn jellyfin_type(self) -> &'static str {
    match self {
      Self::Audio => "Audio",
      Self::Subtitle => "Subtitle",
    }
  }

  /// JRiver's number for the file's first track of this kind.
  fn first_track(self) -> i32 {
    match self {
      Self::Audio => 0,
      Self::Subtitle => 1,
    }
  }

  pub fn label(self) -> &'static str {
    match self {
      Self::Audio => "audio",
      Self::Subtitle => "subtitles",
    }
  }
}

/// The audio and subtitle stream, as Jellyfin indexes them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrackChoice {
  pub audio: Option<i32>,
  pub subtitles: Option<i32>,
}

impl TrackChoice {
  pub fn get(self, kind: TrackKind) -> Option<i32> {
    match kind {
      TrackKind::Audio => self.audio,
      TrackKind::Subtitle => self.subtitles,
    }
  }

  pub fn set(&mut self, kind: TrackKind, index: Option<i32>) {
    match kind {
      TrackKind::Audio => self.audio = index,
      TrackKind::Subtitle => self.subtitles = index,
    }
  }
}

/// The file's own tracks of `kind` (not external files), in Jellyfin's
/// order.
pub fn embedded(streams: &[MediaStream], kind: TrackKind) -> Vec<&MediaStream> {
  let mut tracks: Vec<&MediaStream> = streams
    .iter()
    .filter(|stream| stream.kind == kind.jellyfin_type() && !stream.is_external)
    .collect();
  tracks.sort_by_key(|stream| stream.index);
  tracks
}

/// JRiver's number for Jellyfin's stream `index` (or for no subtitles), or
/// `None` if it isn't one of the file's own tracks of `kind` (unknown, or an
/// external file). Without a track list (a disc folder, or a lookup that
/// failed) nothing is mapped, not even "off".
pub fn jriver_track(streams: &[MediaStream], kind: TrackKind, index: i32) -> Option<i32> {
  if streams.is_empty() {
    return None;
  }
  if kind == TrackKind::Subtitle && index == NO_SUBTITLES {
    return Some(JRIVER_SUBTITLES_OFF);
  }
  // Where MC lists external subtitle files among the file's own isn't
  // known: they could shift every number, and a wrong track is worse than
  // none.
  if kind == TrackKind::Subtitle
    && streams
      .iter()
      .any(|stream| stream.is_external && stream.kind == kind.jellyfin_type())
  {
    return None;
  }
  let position = embedded(streams, kind)
    .iter()
    .position(|stream| stream.index == index)?;
  i32::try_from(position).ok().map(|p| p + kind.first_track())
}

/// Jellyfin's index of what a player shows of `kind` when not told: the
/// track the file marks default, else the first audio track and no
/// subtitles. `None`: the file has no audio track.
pub fn default_index(streams: &[MediaStream], kind: TrackKind) -> Option<i32> {
  let tracks = embedded(streams, kind);
  if let Some(marked) = tracks.iter().find(|stream| stream.is_default) {
    return Some(marked.index);
  }
  match kind {
    TrackKind::Audio => tracks.first().map(|stream| stream.index),
    TrackKind::Subtitle => Some(NO_SUBTITLES),
  }
}

/// What a cast's start does about `kind` (SPEC.md "Select tracks").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartPick {
  /// Nothing asked, or no track list (a disc folder): JRiver's own choice
  /// stays.
  Keep,
  /// Send JRiver this number.
  Select(i32),
  /// Jellyfin asked for a stream JRiver's number isn't known for.
  Unmapped(i32),
}

/// Whatever Jellyfin asks for is sent, also the file's default: MC
/// remembers the tracks last used in each file, so its choice may differ.
pub fn start_pick(streams: &[MediaStream], kind: TrackKind, requested: Option<i32>) -> StartPick {
  let Some(index) = requested else {
    return StartPick::Keep;
  };
  if streams.is_empty() {
    return StartPick::Keep;
  }
  match jriver_track(streams, kind, index) {
    Some(track) => StartPick::Select(track),
    None => StartPick::Unmapped(index),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn stream(index: i32, kind: &str, default: bool, external: bool) -> MediaStream {
    MediaStream {
      index,
      kind: kind.to_owned(),
      is_default: default,
      is_external: external,
      ..MediaStream::default()
    }
  }

  /// A typical remux: video, three audio tracks (the second marked
  /// default), two subtitles.
  fn remux() -> Vec<MediaStream> {
    vec![
      stream(0, "Video", true, false),
      stream(1, "Audio", false, false),
      stream(2, "Audio", true, false),
      stream(3, "Audio", false, false),
      stream(4, "Subtitle", false, false),
      stream(5, "Subtitle", false, false),
    ]
  }

  #[test]
  fn with_an_external_subtitle_file_only_subtitles_off_is_sent() {
    let mut streams = remux();
    streams.push(stream(6, "Subtitle", false, true));
    assert_eq!(jriver_track(&streams, TrackKind::Subtitle, 4), None);
    assert_eq!(jriver_track(&streams, TrackKind::Subtitle, 6), None);
    assert_eq!(
      jriver_track(&streams, TrackKind::Subtitle, NO_SUBTITLES),
      Some(0)
    );
    // Audio isn't affected.
    assert_eq!(jriver_track(&streams, TrackKind::Audio, 3), Some(2));
    assert_eq!(
      start_pick(&streams, TrackKind::Subtitle, Some(5)),
      StartPick::Unmapped(5)
    );
    assert_eq!(
      start_pick(&streams, TrackKind::Subtitle, Some(NO_SUBTITLES)),
      StartPick::Select(0)
    );
  }

  #[test]
  fn without_a_track_list_nothing_is_sent() {
    for kind in [TrackKind::Audio, TrackKind::Subtitle] {
      assert_eq!(jriver_track(&[], kind, NO_SUBTITLES), None);
      assert_eq!(jriver_track(&[], kind, 1), None);
      assert_eq!(start_pick(&[], kind, None), StartPick::Keep);
    }
    assert_eq!(
      start_pick(&[], TrackKind::Subtitle, Some(NO_SUBTITLES)),
      StartPick::Keep
    );
    assert_eq!(start_pick(&[], TrackKind::Audio, Some(2)), StartPick::Keep);
  }

  #[test]
  fn audio_counts_from_0_and_subtitles_from_1_with_0_off() {
    let streams = remux();
    assert_eq!(jriver_track(&streams, TrackKind::Audio, 1), Some(0));
    assert_eq!(jriver_track(&streams, TrackKind::Audio, 3), Some(2));
    assert_eq!(jriver_track(&streams, TrackKind::Subtitle, 4), Some(1));
    assert_eq!(jriver_track(&streams, TrackKind::Subtitle, 5), Some(2));
    assert_eq!(
      jriver_track(&streams, TrackKind::Subtitle, NO_SUBTITLES),
      Some(0)
    );
    // Not a track of that kind, or unknown: nothing to send.
    assert_eq!(jriver_track(&streams, TrackKind::Audio, 4), None);
    assert_eq!(jriver_track(&streams, TrackKind::Audio, NO_SUBTITLES), None);
    assert_eq!(jriver_track(&streams, TrackKind::Audio, 99), None);
  }

  /// Avatar as Jellyfin lists it, against what MC showed.
  #[test]
  fn avatar_matches_the_measurement_in_mc() {
    let mut streams = vec![stream(0, "Video", true, false)];
    streams.extend((1..=8).map(|i| stream(i, "Audio", i == 1, false)));
    streams.extend((9..=16).map(|i| stream(i, "Subtitle", false, false)));
    // 2nd audio (English DD 5.1) is 1; 3rd (Spanish DD+ 7.1) is 2.
    assert_eq!(jriver_track(&streams, TrackKind::Audio, 2), Some(1));
    assert_eq!(jriver_track(&streams, TrackKind::Audio, 3), Some(2));
    assert_eq!(jriver_track(&streams, TrackKind::Audio, 1), Some(0));
    // 1st subtitle (English) is 1; the 8th (Italian) is 8.
    assert_eq!(jriver_track(&streams, TrackKind::Subtitle, 9), Some(1));
    assert_eq!(jriver_track(&streams, TrackKind::Subtitle, 16), Some(8));
  }

  #[test]
  fn the_default_is_the_marked_track_else_the_first_audio_and_no_subtitles() {
    let mut streams = remux();
    assert_eq!(default_index(&streams, TrackKind::Audio), Some(2));
    assert_eq!(
      default_index(&streams, TrackKind::Subtitle),
      Some(NO_SUBTITLES)
    );
    streams[5].is_default = true;
    assert_eq!(default_index(&streams, TrackKind::Subtitle), Some(5));
    streams[2].is_default = false;
    assert_eq!(default_index(&streams, TrackKind::Audio), Some(1));
    assert_eq!(default_index(&[], TrackKind::Audio), None);
  }

  #[test]
  fn a_start_sends_whatever_jellyfin_asks_for_even_the_default() {
    let streams = remux();
    let audio = |requested| start_pick(&streams, TrackKind::Audio, requested);
    let subtitles = |requested| start_pick(&streams, TrackKind::Subtitle, requested);
    assert_eq!(audio(None), StartPick::Keep);
    // MC may remember another track for this file.
    assert_eq!(audio(Some(2)), StartPick::Select(1));
    assert_eq!(audio(Some(3)), StartPick::Select(2));
    assert_eq!(subtitles(None), StartPick::Keep);
    assert_eq!(subtitles(Some(NO_SUBTITLES)), StartPick::Select(0));
    assert_eq!(subtitles(Some(5)), StartPick::Select(2));
    assert_eq!(subtitles(Some(6)), StartPick::Unmapped(6));
  }
}
