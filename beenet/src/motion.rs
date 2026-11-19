//! Motion detection for `camera watch`: is something moving in front of
//! the camera?
//!
//! Each picture is shrunk to a grid of cells (the average brightness of the
//! pixels in each). The detector keeps a background, what the scene usually
//! looks like, that slowly follows the pictures. A picture moves when enough
//! cells differ clearly from the background. Before comparing, the overall
//! brightness change is taken out, so a camera adjusting its exposure (or a
//! cloud before the sun) is not movement; a light switched on is, once.

use alloc::vec::Vec;

/// The grid: 64 x 48 cells (10 x 10 pixels each at 640 x 480).
pub const GRID_W: usize = 64;
pub const GRID_H: usize = 48;

/// A cell differs when it is this much brighter or darker (of 255).
const THRESHOLD: i32 = 24;
/// Pictures to learn the background from before anything counts.
const LEARN: u32 = 8;
/// The background moves 1/8 of the way to each picture.
const FOLLOW_SHIFT: i32 = 3;

/// The average brightness of each cell of a `width` x `height` picture
/// (0x00RRGGBB pixels), row by row.
pub fn cells(pixels: &[u32], width: usize, height: usize) -> Vec<u8> {
    let mut sums = alloc::vec![0u32; GRID_W * GRID_H];
    let mut counts = alloc::vec![0u32; GRID_W * GRID_H];
    if width == 0 || height == 0 {
        return alloc::vec![0; GRID_W * GRID_H];
    }
    for y in 0..height {
        let cy = y * GRID_H / height;
        let row = match pixels.get(y * width..(y + 1) * width) {
            Some(row) => row,
            None => break,
        };
        for (x, p) in row.iter().enumerate() {
            let c = cy * GRID_W + x * GRID_W / width;
            let (r, g, b) = (p >> 16 & 0xFF, p >> 8 & 0xFF, p & 0xFF);
            sums[c] += (r * 77 + g * 150 + b * 29) >> 8;
            counts[c] += 1;
        }
    }
    sums.iter().zip(counts.iter()).map(|(s, n)| if *n == 0 { 0 } else { (s / n) as u8 }).collect()
}

pub struct Detector {
    /// Brightness of each cell times 16 (room for slow following).
    background: Vec<i32>,
    seen: u32,
}

impl Default for Detector {
    fn default() -> Self {
        Self::new()
    }
}

impl Detector {
    pub fn new() -> Detector {
        Detector { background: Vec::new(), seen: 0 }
    }

    /// Feed the cells of the next picture. Returns how much of it moved, in
    /// thousandths of the cells (0 while still learning the background).
    pub fn feed(&mut self, cells: &[u8]) -> u32 {
        if cells.len() != GRID_W * GRID_H {
            return 0;
        }
        if self.background.len() != cells.len() {
            self.background = cells.iter().map(|c| *c as i32 * 16).collect();
            self.seen = 1;
            return 0;
        }
        // Take out the change of the whole picture's brightness.
        let total: i64 = cells.iter().zip(self.background.iter()).map(|(c, b)| *c as i64 * 16 - *b as i64).sum();
        let shift = (total / cells.len() as i64) as i32;
        let mut moved = 0u32;
        for (c, b) in cells.iter().zip(self.background.iter_mut()) {
            let now = *c as i32 * 16;
            if ((now - shift - *b) / 16).abs() > THRESHOLD {
                moved += 1;
            }
            *b += (now - *b) >> FOLLOW_SHIFT;
        }
        self.seen = self.seen.saturating_add(1);
        if self.seen <= LEARN {
            return 0;
        }
        moved * 1000 / cells.len() as u32
    }
}

/// Is this much change movement? (2% of the picture.)
pub fn is_movement(per_mille: u32) -> bool {
    per_mille >= 20
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(w: usize, h: usize, f: impl Fn(usize, usize) -> u32) -> Vec<u32> {
        (0..w * h).map(|i| f(i % w, i / w)).collect()
    }

    #[test]
    fn cells_average_the_pixels() {
        let px = picture(640, 480, |x, _| if x < 320 { 0xFFFFFF } else { 0 });
        let c = cells(&px, 640, 480);
        assert_eq!(c.len(), GRID_W * GRID_H);
        assert!(c[0] >= 254 && c[GRID_W - 1] == 0);
        assert_eq!(cells(&[], 0, 0).len(), GRID_W * GRID_H);
        assert_eq!(cells(&px[..1000], 640, 480).len(), GRID_W * GRID_H); // cut short: no panic
        // A picture smaller than the grid fills only the cells its pixels fall in.
        let tiny = cells(&[0x808080; 4], 2, 2);
        assert_eq!(tiny.iter().filter(|c| **c == 128).count(), 4);
        assert_eq!(tiny.iter().filter(|c| **c == 0).count(), GRID_W * GRID_H - 4);
    }

    #[test]
    fn a_still_scene_does_not_move() {
        let mut d = Detector::new();
        let scene = picture(640, 480, |x, y| ((x / 7 + y / 5) % 200) as u32 * 0x010101);
        let c = cells(&scene, 640, 480);
        for _ in 0..30 {
            assert_eq!(d.feed(&c), 0);
        }
        // Brighter all over (the camera's exposure): not movement either.
        let brighter: Vec<u8> = c.iter().map(|v| v.saturating_add(40)).collect();
        assert!(!is_movement(d.feed(&brighter)));
    }

    #[test]
    fn something_moving_is_seen() {
        let mut d = Detector::new();
        let scene = |bx: usize| picture(640, 480, move |x, y| if (bx..bx + 120).contains(&x) && (100..300).contains(&y) { 0xF0F0F0 } else { 0x303030 });
        for _ in 0..10 {
            d.feed(&cells(&scene(0), 640, 480));
        }
        let moved = d.feed(&cells(&scene(300), 640, 480));
        assert!(is_movement(moved), "{moved}");
        // It stays there: the background takes it in and the movement fades.
        let mut last = moved;
        for _ in 0..40 {
            last = d.feed(&cells(&scene(300), 640, 480));
        }
        assert!(!is_movement(last), "{last}");
    }

    #[test]
    fn learning_first_and_wrong_sizes() {
        let mut d = Detector::new();
        let a = vec![10u8; GRID_W * GRID_H];
        let b = vec![200u8; GRID_W * GRID_H];
        // A big change while still learning counts as nothing.
        assert_eq!(d.feed(&a), 0);
        assert_eq!(d.feed(&b), 0);
        assert_eq!(d.feed(&[1, 2, 3]), 0);
    }
}
