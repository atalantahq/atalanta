// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

use std::sync::PoisonError;

use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, SwashCache, Weight};

/// Larger sizes would make swash rasterise enormous glyph bitmaps.
const MAX_FONT_PX: f32 = 2048.0;

/// A font size cosmic-text can lay out, or `None` for zero, negative or
/// non-finite sizes (which draw nothing, as on Android for `textSize="0"`).
fn usable_font_size(font_size: f32) -> Option<f32> {
    (font_size.is_finite() && font_size > 0.0).then(|| font_size.min(MAX_FONT_PX))
}

fn line_height(font_size: f32) -> f32 {
    (font_size * 1.2).max(1.0)
}

/// A wrap width, or `None` (unbounded) for missing, non-positive or
/// non-finite widths.
fn width_limit(max_width: Option<f32>) -> Option<f32> {
    max_width.filter(|w| w.is_finite() && *w > 0.0)
}
use std::sync::Mutex;

/// Bundled Noto Sans Regular (OFL licensed) for deterministic cross-platform rendering.
static NOTO_SANS_REGULAR: &[u8] = include_bytes!("../fonts/NotoSans-Regular.ttf");
/// Bundled Noto Sans Bold (OFL licensed).
static NOTO_SANS_BOLD: &[u8] = include_bytes!("../fonts/NotoSans-Bold.ttf");

/// Text measurement and rendering engine using cosmic-text + swash.
///
/// Provides font-aware text sizing for layout leaf nodes and
/// glyph-level rasterization for pixel-accurate rendering.
pub struct TextMeasurer {
    inner: Mutex<TextMeasurerInner>,
}

struct TextMeasurerInner {
    font_system: FontSystem,
    swash_cache: SwashCache,
}

/// A rendered glyph with its position and alpha coverage bitmap.
pub struct RenderedGlyph {
    /// X position in the target surface (can be negative for glyph overhang).
    pub x: i32,
    /// Y position in the target surface.
    pub y: i32,
    /// Width of the glyph bitmap.
    pub width: u32,
    /// Height of the glyph bitmap.
    pub height: u32,
    /// Alpha coverage data (one byte per pixel, row-major).
    pub alpha: Vec<u8>,
}

impl TextMeasurer {
    /// Create a new text measurer with bundled fonts only.
    ///
    /// Uses Noto Sans (Regular + Bold) embedded at compile time to ensure
    /// identical rendering across macOS, Linux, and CI environments.
    pub fn new() -> Self {
        let mut db = fontdb::Database::new();
        db.load_font_data(NOTO_SANS_REGULAR.to_vec());
        db.load_font_data(NOTO_SANS_BOLD.to_vec());
        let font_system = FontSystem::new_with_locale_and_db("en-US".to_string(), db);

        Self {
            inner: Mutex::new(TextMeasurerInner {
                font_system,
                swash_cache: SwashCache::new(),
            }),
        }
    }

    /// Measure text dimensions at the given font size.
    ///
    /// Returns (width, height) in pixels.
    /// If `max_width` is Some, text wraps at that width.
    pub fn measure(&self, text: &str, font_size: f32, max_width: Option<f32>) -> (f32, f32) {
        self.measure_weighted(text, font_size, 400, max_width)
    }

    /// [`Self::measure`] in the given weight (700 is the bundled bold face).
    pub fn measure_weighted(
        &self,
        text: &str,
        font_size: f32,
        font_weight: u16,
        max_width: Option<f32>,
    ) -> (f32, f32) {
        // cosmic-text asserts on a 0 line height and spins on negative
        // sizes (VEL-098/099): such text takes no space.
        let Some(font_size) = usable_font_size(font_size) else {
            return (0.0, 0.0);
        };
        let line_height = line_height(font_size);
        if text.is_empty() {
            return (0.0, line_height);
        }

        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let metrics = Metrics::new(font_size, line_height);

        let mut buffer = Buffer::new(&mut inner.font_system, metrics);

        buffer.set_size(width_limit(max_width), None);

        let attrs = Attrs::new()
            .family(Family::SansSerif)
            .weight(Weight(font_weight));
        buffer.set_text(text, &attrs, Shaping::Advanced, None);

        buffer.shape_until_scroll(&mut inner.font_system, false);

        // Calculate bounds from layout runs
        let mut total_width: f32 = 0.0;
        let mut total_height: f32 = 0.0;

        for run in buffer.layout_runs() {
            let run_width = run.line_w;
            total_width = total_width.max(run_width);
            total_height += line_height;
        }

        // Ensure minimum height of one line
        if total_height == 0.0 {
            total_height = line_height;
        }

        (total_width.ceil(), total_height.ceil())
    }

    /// Render text into a list of positioned glyph bitmaps.
    ///
    /// `origin_x` / `origin_y` are the top-left position in the target surface.
    /// Returns glyph bitmaps with absolute positions ready for compositing.
    pub fn render_glyphs(
        &self,
        text: &str,
        font_size: f32,
        max_width: Option<f32>,
        origin_x: f32,
        origin_y: f32,
    ) -> Vec<RenderedGlyph> {
        self.render_glyphs_weighted(text, font_size, 400, max_width, (origin_x, origin_y))
    }

    /// [`Self::render_glyphs`] in the given weight.
    pub fn render_glyphs_weighted(
        &self,
        text: &str,
        font_size: f32,
        font_weight: u16,
        max_width: Option<f32>,
        (origin_x, origin_y): (f32, f32),
    ) -> Vec<RenderedGlyph> {
        let Some(font_size) = usable_font_size(font_size) else {
            return Vec::new();
        };
        if text.is_empty() {
            return Vec::new();
        }
        // No room to draw in (`layout_width="0dp"` before weights, or more
        // padding than width): Android clips the text away. Drawing it
        // unbounded spilled one long line across the neighbours. Only NaN
        // and infinity mean "no limit".
        if max_width.is_some_and(|w| w <= 0.0) {
            return Vec::new();
        }

        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let TextMeasurerInner {
            font_system,
            swash_cache,
        } = &mut *inner;
        let line_height = line_height(font_size);
        let metrics = Metrics::new(font_size, line_height);

        let mut buffer = Buffer::new(font_system, metrics);
        buffer.set_size(width_limit(max_width), None);

        let attrs = Attrs::new()
            .family(Family::SansSerif)
            .weight(Weight(font_weight));
        buffer.set_text(text, &attrs, Shaping::Advanced, None);
        buffer.shape_until_scroll(font_system, false);

        let mut glyphs = Vec::new();

        for run in buffer.layout_runs() {
            for glyph in run.glyphs.iter() {
                let physical = glyph.physical((0.0, 0.0), 1.0);
                let cache_key = physical.cache_key;

                // Get rasterized glyph image from swash
                let image = swash_cache.get_image(font_system, cache_key);

                let image = match image {
                    Some(img) if !img.data.is_empty() && img.placement.width > 0 => img,
                    _ => continue,
                };

                // Rounded, not truncated: a 0.6 px move used to shift the whole
                // text 0 or 1 px depending on the integer part (VT-9 B8).
                let gx = origin_x.round() as i32 + physical.x + image.placement.left;
                let gy = origin_y.round() as i32 + physical.y + run.line_y.round() as i32
                    - image.placement.top;

                // Convert to alpha-only coverage
                let w = image.placement.width as usize;
                let h = image.placement.height as usize;
                let alpha = match image.content {
                    cosmic_text::SwashContent::Mask => image.data.clone(),
                    cosmic_text::SwashContent::Color => {
                        // RGBA data — extract alpha channel
                        image
                            .data
                            .chunks(4)
                            .map(|c| c.get(3).copied().unwrap_or(0))
                            .collect()
                    }
                    cosmic_text::SwashContent::SubpixelMask => {
                        // RGB subpixel — average to grayscale alpha
                        image
                            .data
                            .chunks(3)
                            .map(|c| {
                                let r = c.first().copied().unwrap_or(0) as u16;
                                let g = c.get(1).copied().unwrap_or(0) as u16;
                                let b = c.get(2).copied().unwrap_or(0) as u16;
                                ((r + g + b) / 3) as u8
                            })
                            .collect()
                    }
                };

                if alpha.len() == w * h {
                    glyphs.push(RenderedGlyph {
                        x: gx,
                        y: gy,
                        width: w as u32,
                        height: h as u32,
                        alpha,
                    });
                }
            }
        }

        glyphs
    }
}

impl Default for TextMeasurer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_measure_returns_nonzero() {
        let measurer = TextMeasurer::new();
        let (w, h) = measurer.measure("Hello World", 16.0, None);
        assert!(w > 0.0, "width should be positive, got {w}");
        assert!(h > 0.0, "height should be positive, got {h}");
    }

    #[test]
    fn test_empty_text() {
        let measurer = TextMeasurer::new();
        let (w, h) = measurer.measure("", 16.0, None);
        assert_eq!(w, 0.0);
        assert!(h > 0.0, "empty text still has line height");
    }

    #[test]
    fn test_longer_text_is_wider() {
        let measurer = TextMeasurer::new();
        let (w1, _) = measurer.measure("Hi", 16.0, None);
        let (w2, _) = measurer.measure("Hello World this is longer", 16.0, None);
        assert!(w2 > w1, "longer text should be wider");
    }

    #[test]
    fn test_larger_font_is_taller() {
        let measurer = TextMeasurer::new();
        let (_, h1) = measurer.measure("Hello", 12.0, None);
        let (_, h2) = measurer.measure("Hello", 24.0, None);
        assert!(h2 > h1, "larger font should produce taller text");
    }

    #[test]
    fn test_wrap_with_max_width() {
        let measurer = TextMeasurer::new();
        let (w_no_wrap, h_no_wrap) = measurer.measure("Hello World Test", 16.0, None);
        let (w_wrap, h_wrap) = measurer.measure("Hello World Test", 16.0, Some(50.0));
        // Wrapped text should be narrower but taller
        assert!(
            w_wrap <= w_no_wrap + 1.0,
            "wrapped should be narrower or equal"
        );
        assert!(h_wrap >= h_no_wrap, "wrapped should be taller or equal");
    }

    #[test]
    fn unusable_font_sizes_take_no_space_and_return_quickly() {
        // VEL-098/099: 0 panicked inside cosmic-text, negative sizes spun.
        let measurer = TextMeasurer::new();
        for size in [0.0, -1.0, -16.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let start = std::time::Instant::now();
            assert_eq!(measurer.measure("Hello", size, None), (0.0, 0.0), "{size}");
            assert!(measurer
                .render_glyphs("Hello", size, Some(100.0), 0.0, 0.0)
                .is_empty());
            assert!(
                start.elapsed() < std::time::Duration::from_secs(2),
                "{size}"
            );
        }
        // The measurer still works afterwards.
        assert!(measurer.measure("Hello", 16.0, None).0 > 0.0);
    }

    #[test]
    fn bad_wrap_widths_mean_unbounded() {
        let measurer = TextMeasurer::new();
        let free = measurer.measure("Hello World Test", 16.0, None);
        for w in [f32::NAN, 0.0, -10.0, f32::INFINITY] {
            assert_eq!(
                measurer.measure("Hello World Test", 16.0, Some(w)),
                free,
                "{w}"
            );
        }
        let (_, h) = measurer.measure("Hi", 1e9, None);
        assert!(h <= MAX_FONT_PX * 1.2 + 1.0, "{h}");
    }

    /// Review MINOR-4: a zero or negative wrap width has no room to draw
    /// in, so nothing is drawn (Android clips it); it drew one unbounded
    /// line over the neighbours. NaN and infinity still mean unbounded.
    #[test]
    fn no_room_draws_nothing() {
        let measurer = TextMeasurer::new();
        for w in [0.0, -10.0, -0.5] {
            assert!(
                measurer
                    .render_glyphs("Hello World Test", 16.0, Some(w), 0.0, 0.0)
                    .is_empty(),
                "{w}"
            );
        }
        for w in [f32::NAN, f32::INFINITY] {
            assert!(!measurer
                .render_glyphs("Hello", 16.0, Some(w), 0.0, 0.0)
                .is_empty());
        }
    }
}
