//! The bridge's icon, drawn in code so the repo needs no binary image: a
//! round blue badge with a white play triangle. Used for the tray icon and,
//! from `build.rs`, for the exe's own icon (Start menu, Add or Remove
//! Programs). Plain std only, as `build.rs` includes this file.

/// The badge at `size` x `size` pixels, as RGBA bytes. It's designed on a
/// 32-pixel grid and scaled.
pub fn badge_rgba(size: u32) -> Vec<u8> {
  let n = size as usize;
  let scale = size as f32 / 32.0;
  let mut rgba = vec![0u8; n * n * 4];
  let center = (size as f32 - 1.0) / 2.0;
  let radius = size as f32 / 2.0 - 0.5;
  for y in 0..n {
    for x in 0..n {
      let (fx, fy) = (x as f32, y as f32);
      let distance = ((fx - center).powi(2) + (fy - center).powi(2)).sqrt();
      // Soft edge over one pixel.
      let coverage = (radius - distance + 0.5).clamp(0.0, 1.0);
      if coverage == 0.0 {
        continue;
      }
      // The pixel's centre on the 32-pixel grid. The play triangle there
      // points left (11, 8), (11, 23) and right (24, 15.5).
      let (gx, gy) = ((fx + 0.5) / scale - 0.5, (fy + 0.5) / scale - 0.5);
      let in_triangle = gx >= 11.0 && {
        let half_height = (24.0 - gx) * 8.0 / 13.0;
        (gy - 15.5).abs() <= half_height
      };
      let (r, g, b) = if in_triangle {
        (255, 255, 255)
      } else {
        (0x1f, 0x6f, 0xd6)
      };
      let i = (y * n + x) * 4;
      rgba[i] = r;
      rgba[i + 1] = g;
      rgba[i + 2] = b;
      rgba[i + 3] = (coverage * 255.0).round() as u8;
    }
  }
  rgba
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn every_size_is_the_same_badge() {
    for size in [16u32, 32, 48, 256] {
      let rgba = badge_rgba(size);
      let n = size as usize;
      assert_eq!(rgba.len(), n * n * 4);
      let pixel = |x: usize, y: usize| &rgba[(y * n + x) * 4..(y * n + x) * 4 + 4];
      assert_eq!(pixel(0, 0)[3], 0, "{size}: corners are transparent");
      assert_eq!(
        pixel(n / 2, n / 2),
        [255, 255, 255, 255],
        "{size}: triangle"
      );
      assert_eq!(
        pixel(n * 5 / 32, n / 2),
        [0x1f, 0x6f, 0xd6, 255],
        "{size}: badge colour"
      );
    }
  }
}
