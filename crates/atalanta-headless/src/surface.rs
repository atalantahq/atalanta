// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

use std::sync::Arc;

use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Rect, Transform};

use crate::render_tree::{self, ComputedLayout, RenderNode};
use crate::scroll::{scroll_axes, Clip};
use crate::text::TextMeasurer;

/// `a` faded by `alpha` (0..=1).
fn scale_alpha(a: u8, alpha: f32) -> u8 {
    (a as f32 * alpha.clamp(0.0, 1.0)).round() as u8
}

/// A rectangle with circular corners of `r`, as four cubic arcs.
fn rounded_rect(l: ComputedLayout, r: f32) -> Option<tiny_skia::Path> {
    // Control-point distance for a quarter circle drawn as a cubic.
    const K: f32 = 0.552_284_8;
    let (x0, y0, x1, y1) = (l.x, l.y, l.x + l.width, l.y + l.height);
    let k = r * K;
    let mut pb = PathBuilder::new();
    pb.move_to(x0 + r, y0);
    pb.line_to(x1 - r, y0);
    pb.cubic_to(x1 - r + k, y0, x1, y0 + r - k, x1, y0 + r);
    pb.line_to(x1, y1 - r);
    pb.cubic_to(x1, y1 - r + k, x1 - r + k, y1, x1 - r, y1);
    pb.line_to(x0 + r, y1);
    pb.cubic_to(x0 + r - k, y1, x0, y1 - r + k, x0, y1 - r);
    pb.line_to(x0, y0 + r);
    pb.cubic_to(x0, y0 + r - k, x0 + r - k, y0, x0 + r, y0);
    pb.close();
    pb.finish()
}

/// CPU-only software rendering surface using tiny-skia.
///
/// Renders a RenderTree to an RGBA pixel buffer and encodes to PNG.
/// Deterministic: no GPU, no threading, no system-dependent state.
pub struct SoftwareSurface {
    pixmap: Pixmap,
}

impl SoftwareSurface {
    /// Create a new surface with the given dimensions.
    pub fn new(width: u32, height: u32) -> Result<Self, SurfaceError> {
        let pixmap =
            Pixmap::new(width, height).ok_or(SurfaceError::CreationFailed { width, height })?;
        Ok(Self { pixmap })
    }

    /// Get surface dimensions.
    pub fn size(&self) -> (u32, u32) {
        (self.pixmap.width(), self.pixmap.height())
    }

    /// Clear the surface to a solid color.
    pub fn clear(&mut self, color: render_tree::Color) {
        self.pixmap
            .fill(Color::from_rgba8(color.r, color.g, color.b, color.a));
    }

    /// Render a complete render tree to the surface.
    pub fn render_tree(&mut self, root: &RenderNode, text_measurer: Option<&Arc<TextMeasurer>>) {
        // Clear to white background
        self.pixmap.fill(Color::from_rgba8(255, 255, 255, 255));
        let screen = Clip::screen(self.pixmap.width() as f32, self.pixmap.height() as f32);
        self.render_node(root, text_measurer, screen, 1.0);
    }

    /// Render a single node and its children recursively, inside `clip`: a scroll
    /// container clips its content to itself. `alpha` is the opacity inherited
    /// from the ancestors: a view's alpha fades it and everything inside it.
    fn render_node(
        &mut self,
        node: &RenderNode,
        text_measurer: Option<&Arc<TextMeasurer>>,
        clip: Clip,
        alpha: f32,
    ) {
        let opacity = if node.style.opacity.is_nan() {
            1.0
        } else {
            node.style.opacity.clamp(0.0, 1.0)
        };
        let alpha = alpha * opacity;
        // Fully transparent: nothing to paint (it keeps its place in the layout).
        if !node.style.visible || alpha <= 0.0 {
            return;
        }

        let layout = match node.layout {
            Some(l) => l,
            None => return,
        };

        // Draw background if non-transparent
        let bg = &node.style.background_color;
        if bg.a > 0 {
            self.draw_rect(layout, bg, node.style.border_radius, alpha, clip);
        }

        // Draw text if present
        if let Some(text) = &node.text {
            if let Some(measurer) = text_measurer {
                self.draw_text_glyphs(text, layout, &node.style, measurer, clip, alpha);
            } else {
                self.draw_text_fallback(text, layout, &node.style, clip, alpha);
            }
        }

        // Draw children
        let inner = if scroll_axes(&node.node_type).is_some() {
            clip.intersect(Clip::of(&layout))
        } else {
            clip
        };
        if inner.is_empty() {
            return;
        }
        for child in &node.children {
            self.render_node(child, text_measurer, inner, alpha);
        }
    }

    /// Draw a filled rectangle, with rounded corners when `radius > 0`, at
    /// `alpha` times the colour's own alpha.
    fn draw_rect(
        &mut self,
        layout: ComputedLayout,
        color: &render_tree::Color,
        radius: f32,
        alpha: f32,
        clip: Clip,
    ) {
        let c = Clip::of(&layout).intersect(clip);
        let Some(clip_rect) = Rect::from_ltrb(c.x0, c.y0, c.x1, c.y1) else {
            return;
        };

        let mut paint = Paint::default();
        paint.set_color_rgba8(color.r, color.g, color.b, scale_alpha(color.a, alpha));
        paint.anti_alias = true;

        let radius = if radius.is_finite() {
            radius.min(layout.width / 2.0).min(layout.height / 2.0)
        } else {
            0.0
        };
        let path = if radius > 0.0 {
            // The whole rounded shape, masked to the clip below.
            match rounded_rect(layout, radius) {
                Some(p) => p,
                None => return,
            }
        } else {
            let mut pb = PathBuilder::new();
            pb.push_rect(clip_rect);
            match pb.finish() {
                Some(p) => p,
                None => return,
            }
        };
        let mask = if radius > 0.0 {
            let mut mask = match tiny_skia::Mask::new(self.pixmap.width(), self.pixmap.height()) {
                Some(m) => m,
                None => return,
            };
            let mut pb = PathBuilder::new();
            pb.push_rect(clip_rect);
            if let Some(clip_path) = pb.finish() {
                mask.fill_path(&clip_path, FillRule::Winding, false, Transform::identity());
            }
            Some(mask)
        } else {
            None
        };

        self.pixmap.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            mask.as_ref(),
        );
    }

    /// Draw text using real glyph rasterization via cosmic-text + swash.
    fn draw_text_glyphs(
        &mut self,
        text: &str,
        layout: ComputedLayout,
        style: &render_tree::NodeStyle,
        text_measurer: &TextMeasurer,
        clip: Clip,
        alpha: f32,
    ) {
        if text.is_empty() {
            return;
        }

        let max_width = Some(layout.width - style.padding.left - style.padding.right);
        let origin_x = layout.x + style.padding.left;
        let origin_y = layout.y + style.padding.top;

        let glyphs = text_measurer.render_glyphs_weighted(
            text,
            style.font_size,
            style.font_weight,
            max_width,
            (origin_x, origin_y),
        );

        let text_color = &style.text_color;
        // The text colour's own alpha and the inherited opacity scale coverage.
        let paint_alpha = text_color.a as f32 / 255.0 * alpha;
        let surface_w = self.pixmap.width() as i32;
        let surface_h = self.pixmap.height() as i32;
        let pixels = self.pixmap.data_mut();

        for glyph in &glyphs {
            for gy in 0..glyph.height as i32 {
                for gx in 0..glyph.width as i32 {
                    let px = glyph.x + gx;
                    let py = glyph.y + gy;

                    // Clip to surface bounds and the scroll container
                    if px < 0
                        || py < 0
                        || px >= surface_w
                        || py >= surface_h
                        || !clip.contains((px as f32, py as f32))
                    {
                        continue;
                    }

                    let alpha_idx = (gy as usize) * (glyph.width as usize) + (gx as usize);
                    let alpha = glyph.alpha[alpha_idx];
                    if alpha == 0 {
                        continue;
                    }

                    let pixel_idx = ((py * surface_w + px) * 4) as usize;
                    if pixel_idx + 3 >= pixels.len() {
                        continue;
                    }

                    // Alpha-blend text color onto existing pixel
                    let a = alpha as f32 / 255.0 * paint_alpha;
                    let inv_a = 1.0 - a;

                    pixels[pixel_idx] =
                        (text_color.r as f32 * a + pixels[pixel_idx] as f32 * inv_a) as u8;
                    pixels[pixel_idx + 1] =
                        (text_color.g as f32 * a + pixels[pixel_idx + 1] as f32 * inv_a) as u8;
                    pixels[pixel_idx + 2] =
                        (text_color.b as f32 * a + pixels[pixel_idx + 2] as f32 * inv_a) as u8;
                    pixels[pixel_idx + 3] = ((a * 255.0).round() as u8).max(pixels[pixel_idx + 3]);
                }
            }
        }
    }

    /// Fallback: draw text as colored block (used when no TextMeasurer available).
    fn draw_text_fallback(
        &mut self,
        text: &str,
        layout: ComputedLayout,
        style: &render_tree::NodeStyle,
        clip: Clip,
        alpha: f32,
    ) {
        if text.is_empty() {
            return;
        }

        let text_color = &style.text_color;
        let mut paint = Paint::default();
        paint.set_color_rgba8(
            text_color.r,
            text_color.g,
            text_color.b,
            scale_alpha(text_color.a, alpha),
        );
        paint.anti_alias = true;

        // Render text as a thin horizontal bar to indicate text presence
        let text_height = style.font_size.min(layout.height);
        let y_offset = (layout.height - text_height) / 2.0;

        // Use character count to determine text block width
        let char_width = style.font_size * 0.6;
        let char_count = text.chars().count() as f32;
        let text_width = (char_count * char_width).min(layout.width);

        let bar = Clip::of(&ComputedLayout {
            x: layout.x + style.padding.left,
            y: layout.y + y_offset,
            width: text_width,
            height: text_height,
        })
        .intersect(clip);
        let rect = match Rect::from_ltrb(bar.x0, bar.y0, bar.x1, bar.y1) {
            Some(r) => r,
            None => return,
        };

        let path = {
            let mut pb = PathBuilder::new();
            pb.push_rect(rect);
            match pb.finish() {
                Some(p) => p,
                None => return,
            }
        };

        self.pixmap.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }

    /// Encode the current surface to PNG bytes.
    pub fn encode_png(&self) -> Result<Vec<u8>, SurfaceError> {
        self.pixmap
            .encode_png()
            .map_err(|e| SurfaceError::EncodingFailed(e.to_string()))
    }

    /// Get raw RGBA pixel data.
    pub fn pixels(&self) -> &[u8] {
        self.pixmap.data()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SurfaceError {
    #[error("Failed to create surface with dimensions {width}x{height}")]
    CreationFailed { width: u32, height: u32 },
    #[error("PNG encoding failed: {0}")]
    EncodingFailed(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render_tree::Color as RColor;

    fn px(surface: &SoftwareSurface, x: usize, y: usize) -> (u8, u8, u8) {
        let w = surface.size().0 as usize;
        let p = &surface.pixels()[(y * w + x) * 4..][..3];
        (p[0], p[1], p[2])
    }

    fn boxed(x: f32, y: f32, w: f32, h: f32) -> Option<ComputedLayout> {
        Some(ComputedLayout {
            x,
            y,
            width: w,
            height: h,
        })
    }

    /// VT-9 B7: `alpha` used to be parsed and ignored, so 50 % red painted pure red.
    #[test]
    fn opacity_fades_the_background_and_is_inherited() {
        let mut surface = SoftwareSurface::new(100, 100).unwrap();
        let mut child = RenderNode::container("View");
        child.style.background_color = RColor::from_rgb(255, 0, 0);
        child.layout = boxed(0.0, 0.0, 50.0, 50.0);
        let mut parent = RenderNode::container("View").with_child(child);
        parent.style.opacity = 0.5;
        parent.layout = boxed(0.0, 0.0, 100.0, 100.0);
        surface.render_tree(&parent, None);
        let (r, g, b) = px(&surface, 25, 25);
        assert_eq!(r, 255);
        assert!((126..=129).contains(&g) && g == b, "{:?}", (r, g, b));

        // alpha 0 paints nothing.
        parent.style.opacity = 0.0;
        surface.render_tree(&parent, None);
        assert_eq!(px(&surface, 25, 25), (255, 255, 255));
    }

    /// VT-9 B7: `border_radius` was ignored, so corners were square.
    #[test]
    fn border_radius_rounds_the_corners() {
        let mut surface = SoftwareSurface::new(100, 100).unwrap();
        let mut node = RenderNode::container("View");
        node.style.background_color = RColor::from_rgb(255, 0, 0);
        node.style.border_radius = 20.0;
        node.layout = boxed(10.0, 10.0, 80.0, 80.0);
        surface.render_tree(&node, None);
        assert_eq!(px(&surface, 11, 11), (255, 255, 255), "corner");
        assert_eq!(px(&surface, 50, 50), (255, 0, 0), "centre");
        assert_eq!(px(&surface, 50, 11), (255, 0, 0), "top edge");
    }

    fn darkest(surface: &SoftwareSurface) -> u8 {
        surface.pixels().chunks(4).map(|c| c[0]).min().unwrap()
    }

    fn text_node(text: &str) -> RenderNode {
        let mut node = RenderNode::container("TextView");
        node.text = Some(text.to_string());
        node.style.font_size = 32.0;
        node.layout = boxed(0.0, 0.0, 300.0, 60.0);
        node
    }

    /// VT-9 B7: `#80000000` text drew fully black.
    #[test]
    fn text_alpha_scales_glyph_coverage() {
        let measurer = Arc::new(TextMeasurer::new());
        let mut surface = SoftwareSurface::new(300, 60).unwrap();
        let mut node = text_node("Hello");
        node.style.text_color = RColor::from_argb(0xFF00_0000);
        surface.render_tree(&node, Some(&measurer));
        assert!(darkest(&surface) < 10, "opaque text is black");
        node.style.text_color = RColor::from_argb(0x8000_0000);
        surface.render_tree(&node, Some(&measurer));
        let d = darkest(&surface);
        assert!((120..=135).contains(&d), "half-transparent text: {d}");
    }

    /// VT-9 B7: bold rendered and measured like regular.
    #[test]
    fn bold_text_renders_and_measures_bolder() {
        let measurer = Arc::new(TextMeasurer::new());
        let regular = measurer.measure_weighted("Hello", 32.0, 400, None).0;
        let bold = measurer.measure_weighted("Hello", 32.0, 700, None).0;
        assert!(bold > regular, "bold {bold} vs regular {regular}");

        let ink = |weight| {
            let mut surface = SoftwareSurface::new(300, 60).unwrap();
            let mut node = text_node("Hello");
            node.style.font_weight = weight;
            surface.render_tree(&node, Some(&measurer));
            surface
                .pixels()
                .chunks(4)
                .map(|c| 255 - c[0] as u32)
                .sum::<u32>()
        };
        assert!(ink(700) > ink(400));
    }

    /// VT-9 B8: glyph origins were truncated, so moving text by 0.6 px either
    /// didn't move it or moved it a whole pixel depending on the integer part.
    #[test]
    fn glyph_origins_round_rather_than_truncate() {
        let measurer = Arc::new(TextMeasurer::new());
        let render = |x: f32| {
            let mut surface = SoftwareSurface::new(300, 60).unwrap();
            let mut node = text_node("Hi");
            node.layout = boxed(x, 0.0, 200.0, 60.0);
            surface.render_tree(&node, Some(&measurer));
            surface.pixels().to_vec()
        };
        // 10.6 is nearer 11 than 10.
        assert_eq!(render(10.6), render(11.0));
        assert_ne!(render(10.6), render(10.0));
    }

    #[test]
    fn test_create_surface() {
        let surface = SoftwareSurface::new(1080, 1920).unwrap();
        assert_eq!(surface.size(), (1080, 1920));
    }

    #[test]
    fn test_encode_png() {
        let mut surface = SoftwareSurface::new(100, 100).unwrap();
        surface.clear(RColor::WHITE);
        let png = surface.encode_png().unwrap();
        // PNG magic bytes
        assert_eq!(&png[0..4], &[0x89, b'P', b'N', b'G']);
        assert!(png.len() > 8);
    }

    #[test]
    fn test_render_colored_rect() {
        let mut surface = SoftwareSurface::new(100, 100).unwrap();

        let mut node = RenderNode::container("View");
        node.style.background_color = RColor::from_rgb(255, 0, 0);
        node.layout = Some(ComputedLayout {
            x: 10.0,
            y: 10.0,
            width: 50.0,
            height: 50.0,
        });

        surface.render_tree(&node, None);
        let png = surface.encode_png().unwrap();
        assert!(!png.is_empty());

        // Verify the red pixel is present in the buffer
        let pixels = surface.pixels();
        // Check pixel at (35, 35) — center of the 50x50 rect at (10,10)
        let idx = ((35 * 100 + 35) * 4) as usize;
        assert_eq!(pixels[idx], 255, "red channel");
        assert_eq!(pixels[idx + 1], 0, "green channel");
        assert_eq!(pixels[idx + 2], 0, "blue channel");
    }

    #[test]
    fn test_glyph_rendering_produces_varied_pixels() {
        let text_measurer = Arc::new(TextMeasurer::new());
        let mut surface = SoftwareSurface::new(200, 50).unwrap();

        let mut node = RenderNode::container("TextView");
        node.text = Some("Hello World".to_string());
        node.style.text_color = RColor::from_rgb(0, 0, 0);
        node.style.font_size = 16.0;
        node.layout = Some(ComputedLayout {
            x: 0.0,
            y: 0.0,
            width: 200.0,
            height: 50.0,
        });

        surface.render_tree(&node, Some(&text_measurer));
        let pixels = surface.pixels();

        // Count non-white pixels — glyph rendering should produce anti-aliased edges
        // with varied alpha values, not just solid blocks
        let mut non_white = 0u32;
        let mut unique_values = std::collections::HashSet::new();
        for chunk in pixels.chunks(4) {
            let r = chunk[0];
            let g = chunk[1];
            let b = chunk[2];
            if r != 255 || g != 255 || b != 255 {
                non_white += 1;
                unique_values.insert((r, g, b));
            }
        }

        assert!(
            non_white > 10,
            "should have rendered text pixels, got {non_white}"
        );
        // Real glyph rendering produces many unique gray values from anti-aliasing
        assert!(
            unique_values.len() > 3,
            "expected varied pixel values from anti-aliasing, got {} unique",
            unique_values.len()
        );
    }
}
