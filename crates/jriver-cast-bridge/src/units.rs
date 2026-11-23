//! Time units. Jellyfin counts in ticks of 100 ns (`i64`), MCWS in
//! milliseconds. They are distinct types so they can't be mixed by accident.

use std::fmt;

use serde::Serialize;

/// Jellyfin ticks per millisecond (1 tick = 100 ns).
pub const TICKS_PER_MS: i64 = 10_000;

/// A Jellyfin position or duration in ticks. Serializes as the bare number.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct Ticks(pub i64);

/// An MCWS position or duration in milliseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Millis(pub u64);

impl Ticks {
  /// Milliseconds, truncated. Negative ticks (never valid for a position)
  /// become 0.
  pub fn to_millis(self) -> Millis {
    Millis(u64::try_from(self.0 / TICKS_PER_MS).unwrap_or(0))
  }
}

impl Millis {
  /// Ticks = ms × 10,000, saturating at `i64::MAX` (about 29,000 years).
  pub fn to_ticks(self) -> Ticks {
    let ms = i64::try_from(self.0).unwrap_or(i64::MAX);
    Ticks(ms.saturating_mul(TICKS_PER_MS))
  }
}

impl From<Millis> for Ticks {
  fn from(ms: Millis) -> Self {
    ms.to_ticks()
  }
}

impl From<Ticks> for Millis {
  fn from(ticks: Ticks) -> Self {
    ticks.to_millis()
  }
}

impl fmt::Display for Millis {
  /// `h:mm:ss.mmm`, or `m:ss.mmm` under an hour.
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let total_ms = self.0;
    let ms = total_ms % 1000;
    let total_s = total_ms / 1000;
    let (h, m, s) = (total_s / 3600, (total_s / 60) % 60, total_s % 60);
    if h > 0 {
      write!(f, "{h}:{m:02}:{s:02}.{ms:03}")
    } else {
      write!(f, "{m}:{s:02}.{ms:03}")
    }
  }
}

/// Parse a position typed by a person: plain milliseconds (`1200000`),
/// `m:ss`, or `h:mm:ss`, each optionally with `.fff` seconds.
pub fn parse_position(input: &str) -> Result<Millis, String> {
  let input = input.trim();
  if input.is_empty() {
    return Err("empty position".to_owned());
  }
  if !input.contains(':') {
    return input
      .parse::<u64>()
      .map(Millis)
      .map_err(|_| format!("not a position: {input:?} (use milliseconds, m:ss or h:mm:ss)"));
  }
  let parts: Vec<&str> = input.split(':').collect();
  if parts.len() > 3 {
    return Err(format!("too many ':' in {input:?}"));
  }
  let (seconds_part, minute_hour) = parts.split_last().expect("split yields at least one part");
  let (whole_s, frac_ms) = match seconds_part.split_once('.') {
    Some((whole, frac)) => {
      if frac.is_empty() || frac.len() > 3 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("bad fraction in {input:?}"));
      }
      let frac_ms = format!("{frac:0<3}")
        .parse::<u64>()
        .map_err(|e| e.to_string())?;
      (whole, frac_ms)
    }
    None => (*seconds_part, 0),
  };
  let number = |s: &str| -> Result<u64, String> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
      return Err(format!("bad number {s:?} in {input:?}"));
    }
    s.parse::<u64>().map_err(|e| e.to_string())
  };
  let seconds = number(whole_s)?;
  if seconds >= 60 {
    return Err(format!("seconds must be under 60 in {input:?}"));
  }
  let (hours, minutes) = match minute_hour {
    [m] => (0, number(m)?),
    [h, m] => {
      let minutes = number(m)?;
      if minutes >= 60 {
        return Err(format!("minutes must be under 60 in {input:?}"));
      }
      (number(h)?, minutes)
    }
    _ => unreachable!("1 to 3 parts"),
  };
  let total_s = hours
    .checked_mul(3600)
    .and_then(|h| h.checked_add(minutes.checked_mul(60)?))
    .and_then(|hm| hm.checked_add(seconds))
    .ok_or_else(|| format!("position too large: {input:?}"))?;
  total_s
    .checked_mul(1000)
    .and_then(|ms| ms.checked_add(frac_ms))
    .map(Millis)
    .ok_or_else(|| format!("position too large: {input:?}"))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn ticks_and_millis_convert_by_ten_thousand() {
    assert_eq!(Millis(1).to_ticks(), Ticks(10_000));
    assert_eq!(Millis(1_200_000).to_ticks(), Ticks(12_000_000_000));
    assert_eq!(Ticks(12_000_000_000).to_millis(), Millis(1_200_000));
    assert_eq!(Ticks::from(Millis(11_840_703)), Ticks(118_407_030_000));
  }

  #[test]
  fn sub_millisecond_ticks_truncate() {
    assert_eq!(Ticks(9_999).to_millis(), Millis(0));
    assert_eq!(Ticks(19_999).to_millis(), Millis(1));
  }

  #[test]
  fn negative_ticks_become_zero_and_huge_values_saturate() {
    assert_eq!(Ticks(-10_000).to_millis(), Millis(0));
    assert_eq!(Millis(u64::MAX).to_ticks(), Ticks(i64::MAX));
    assert_eq!(
      Millis(i64::MAX as u64 / 10_000 + 1).to_ticks(),
      Ticks(i64::MAX)
    );
    assert_eq!(Ticks(i64::MAX).to_millis(), Millis(922_337_203_685_477));
  }

  #[test]
  fn round_trip_is_exact_for_whole_milliseconds() {
    for ms in [0, 1, 999, 1_000, 3_599_999, 11_840_703, 86_400_000] {
      assert_eq!(Millis(ms).to_ticks().to_millis(), Millis(ms));
    }
  }

  #[test]
  fn display_formats_positions() {
    assert_eq!(Millis(0).to_string(), "0:00.000");
    assert_eq!(Millis(1_200_300).to_string(), "20:00.300");
    assert_eq!(Millis(11_840_703).to_string(), "3:17:20.703");
  }

  #[test]
  fn parses_positions() {
    assert_eq!(parse_position("1200000"), Ok(Millis(1_200_000)));
    assert_eq!(parse_position("20:00"), Ok(Millis(1_200_000)));
    assert_eq!(parse_position("1:05:30"), Ok(Millis(3_930_000)));
    assert_eq!(parse_position("0:00.5"), Ok(Millis(500)));
    assert_eq!(parse_position("3:17:20.703"), Ok(Millis(11_840_703)));
    assert_eq!(parse_position(" 90:00 "), Ok(Millis(5_400_000)));
  }

  #[test]
  fn rejects_bad_positions() {
    for bad in [
      "",
      "abc",
      "-5",
      "1:60",
      "1:60:00",
      "1:2:3:4",
      "1:00.",
      "1:00.1234",
      "1:x",
      ":30",
      "1.5",
    ] {
      assert!(parse_position(bad).is_err(), "{bad:?} should be rejected");
    }
  }
}
