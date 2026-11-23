// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

use std::path::PathBuf;

use atalanta_common::{AtalantaError, Result};
use tracing::{debug, info, warn};

/// Configuration for visual assertion comparison.
#[derive(Debug, Clone)]
pub struct VisualConfig {
    /// Minimum share of compared pixels that must match (0.0–1.0), default
    /// 0.98. A share of pixels, not a colour tolerance: that is `rule`.
    pub threshold: f64,
    /// When one pixel counts as changed (D-V2).
    pub rule: PixelRule,
    /// Directory where baseline screenshots are stored.
    pub baselines_dir: PathBuf,
    /// Directory where diff images are written on failure.
    pub diffs_dir: PathBuf,
    /// If true, update baselines instead of comparing.
    pub update_baselines: bool,
    /// A cluster of changed pixels at least this big fails the comparison
    /// whatever the overall similarity (Argos ignores changes ≤ 20 px):
    /// one wrong price is a real change even on a 99.9% identical screen.
    pub cluster_px: usize,
}

impl Default for VisualConfig {
    fn default() -> Self {
        Self {
            threshold: 0.98,
            rule: PixelRule::default(),
            baselines_dir: PathBuf::from("atalanta-baselines"),
            diffs_dir: PathBuf::from("atalanta-diffs"),
            update_baselines: false,
            cluster_px: DEFAULT_CLUSTER_PX,
        }
    }
}

/// Result of a visual comparison.
#[derive(Debug, Clone)]
pub struct VisualComparisonResult {
    /// Overall similarity score (0.0–1.0).
    pub similarity: f64,
    /// Number of pixels that differed.
    pub diff_pixel_count: usize,
    /// Total pixels compared.
    pub total_pixels: usize,
    /// Whether the comparison passed the threshold.
    pub passed: bool,
    /// Path to the diff image (if generated).
    pub diff_image_path: Option<PathBuf>,
    /// Size of the biggest cluster of changed pixels.
    pub largest_cluster: usize,
    /// Where that cluster is, in screenshot pixels.
    pub largest_cluster_bounds: Option<MaskRegion>,
    /// Update mode wrote the baseline (it was missing or had changed).
    pub baseline_written: bool,
}

/// Default for [`VisualConfig::cluster_px`].
pub const DEFAULT_CLUSTER_PX: usize = 20;

/// When one pixel counts as changed (D-V2): `assertScreenshot`'s
/// `comparator` and `tolerance`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelRule {
    pub comparator: atalanta_common::Comparator,
    /// `strict`: the largest per-channel difference (alpha included) that
    /// still counts as the same, 0..255. Default 16.
    pub tolerance: u8,
}

impl Default for PixelRule {
    fn default() -> Self {
        Self {
            comparator: atalanta_common::Comparator::Strict,
            tolerance: CHANNEL_TOLERANCE,
        }
    }
}

/// `perceptual`'s threshold: pixelmatch's and Playwright's default (0.1 of
/// the largest YIQ distance).
pub const PERCEPTUAL_THRESHOLD: f64 = 0.1;

/// The largest squared YIQ distance between two colours, from pixelmatch.
const MAX_YIQ_DELTA: f64 = 35215.0;

impl PixelRule {
    /// Whether two pixels differ under this rule (before the
    /// anti-aliasing check).
    fn differs(&self, a: &[u8; 4], b: &[u8; 4]) -> bool {
        match self.comparator {
            atalanta_common::Comparator::Strict => a
                .iter()
                .zip(b)
                .any(|(ca, cb)| ca.abs_diff(*cb) > self.tolerance),
            atalanta_common::Comparator::Perceptual => {
                yiq_delta(a, b) > MAX_YIQ_DELTA * PERCEPTUAL_THRESHOLD * PERCEPTUAL_THRESHOLD
            }
        }
    }
}

/// pixelmatch's `colorDelta` (ISC licence, Mapbox), after Kotsarenko and
/// Ramos (2010): squared YIQ distance, each pixel blended over white by
/// its alpha first.
fn yiq_delta(a: &[u8; 4], b: &[u8; 4]) -> f64 {
    if a == b {
        return 0.0;
    }
    let blend = |p: &[u8; 4]| {
        let alpha = f64::from(p[3]) / 255.0;
        let c = |v: u8| 255.0 + (f64::from(v) - 255.0) * alpha;
        (c(p[0]), c(p[1]), c(p[2]))
    };
    let ((r1, g1, b1), (r2, g2, b2)) = (blend(a), blend(b));
    let y = |r: f64, g: f64, b: f64| r * 0.29889531 + g * 0.58662247 + b * 0.11448223;
    let i = |r: f64, g: f64, b: f64| r * 0.59597799 - g * 0.2741761 - b * 0.32180189;
    let q = |r: f64, g: f64, b: f64| r * 0.21147017 - g * 0.52261711 + b * 0.31114694;
    let dy = y(r1, g1, b1) - y(r2, g2, b2);
    let di = i(r1, g1, b1) - i(r2, g2, b2);
    let dq = q(r1, g1, b1) - q(r2, g2, b2);
    0.5053 * dy * dy + 0.299 * di * di + 0.1957 * dq * dq
}

/// A region to mask (ignore) during visual comparison.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MaskRegion {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Named mask presets for common dynamic regions.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MaskPreset {
    /// Mask the top status bar (time, battery, signal).
    StatusBar,
    /// Mask the keyboard area (bottom of screen).
    Keyboard,
    /// Custom rectangular region.
    Custom(MaskRegion),
}

impl MaskPreset {
    /// Convert a preset to concrete pixel regions given screen dimensions.
    pub fn to_region(&self, screen_width: u32, screen_height: u32) -> MaskRegion {
        match self {
            MaskPreset::StatusBar => MaskRegion {
                x: 0,
                y: 0,
                width: screen_width,
                height: (screen_height as f64 * 0.04) as u32, // ~4% of screen
            },
            MaskPreset::Keyboard => MaskRegion {
                x: 0,
                y: (screen_height as f64 * 0.6) as u32,
                width: screen_width,
                height: (screen_height as f64 * 0.4) as u32, // bottom 40%
            },
            MaskPreset::Custom(r) => r.clone(),
        }
    }
}

/// Engine for performing visual screenshot assertions.
pub struct VisualEngine {
    config: VisualConfig,
}

impl VisualEngine {
    pub fn new(config: VisualConfig) -> Self {
        Self { config }
    }

    /// Assert that a screenshot matches its baseline.
    ///
    /// - If `update_baselines` is true, saves the screenshot as the new
    ///   baseline, unless the stored one already matches it (VT-7: a
    ///   re-encoded but equal frame leaves the file alone).
    /// - Otherwise, compares against the stored baseline using pixel
    ///   similarity; a failure leaves `<name>-expected.png`,
    ///   `<name>-actual.png` and `<name>-diff.png` in `diffs_dir` (VT-6).
    pub fn assert_screenshot(
        &self,
        baseline_name: &str,
        screenshot_png: &[u8],
        masks: &[MaskPreset],
    ) -> Result<VisualComparisonResult> {
        let baseline_path = self.config.baselines_dir.join(baseline_name);
        let unchanged = VisualComparisonResult {
            similarity: 1.0,
            diff_pixel_count: 0,
            total_pixels: 0,
            passed: true,
            diff_image_path: None,
            largest_cluster: 0,
            largest_cluster_bounds: None,
            baseline_written: false,
        };

        // Update mode: save as new baseline
        if self.config.update_baselines {
            let same = std::fs::read(&baseline_path).ok().is_some_and(|old| {
                compare_images_rule(
                    &old,
                    screenshot_png,
                    masks,
                    self.config.threshold,
                    self.config.cluster_px,
                    self.config.rule,
                )
                .is_ok_and(|r| r.passed)
            });
            if same {
                debug!(baseline = baseline_name, "baseline unchanged");
                return Ok(unchanged);
            }
            std::fs::create_dir_all(&self.config.baselines_dir)
                .map_err(|e| AtalantaError::io("Failed to create baselines dir", e))?;
            write_atomic(&baseline_path, screenshot_png).map_err(|e| {
                AtalantaError::io(format!("Failed to write baseline {baseline_name}"), e)
            })?;
            info!(baseline = baseline_name, "baseline updated");
            return Ok(VisualComparisonResult {
                baseline_written: true,
                ..unchanged
            });
        }

        // Load baseline
        if !baseline_path.exists() {
            return Err(AtalantaError::Config(format!(
                "Baseline not found: {}. Run with --update-baselines to create it.",
                baseline_path.display()
            )));
        }

        let baseline_bytes = std::fs::read(&baseline_path).map_err(|e| {
            AtalantaError::io(format!("Failed to read baseline {baseline_name}"), e)
        })?;

        // Compare the raw PNG bytes
        let result = compare_images_rule(
            &baseline_bytes,
            screenshot_png,
            masks,
            self.config.threshold,
            self.config.cluster_px,
            self.config.rule,
        )?;

        if !result.passed {
            let diff = generate_diff_overlay_rule(
                &baseline_bytes,
                screenshot_png,
                masks,
                self.config.rule,
            );
            let diff_path = self.save_frames(
                baseline_name,
                &[("expected", &baseline_bytes), ("actual", screenshot_png)],
                diff.as_deref(),
            )?;
            warn!(
                baseline = baseline_name,
                similarity = result.similarity,
                threshold = self.config.threshold,
                diff_pixels = result.diff_pixel_count,
                diff_path = ?diff_path,
                "visual assertion failed"
            );
            return Ok(VisualComparisonResult {
                diff_image_path: diff_path,
                ..result
            });
        } else {
            debug!(
                baseline = baseline_name,
                similarity = result.similarity,
                "visual assertion passed"
            );
        }

        Ok(result)
    }

    /// Write `<stem>-<kind>.png` for each frame and `<stem>-diff.png` into
    /// `diffs_dir`; returns the diff's path (the step's screenshot).
    pub fn save_frames(
        &self,
        baseline_name: &str,
        frames: &[(&str, &[u8])],
        diff: Option<&[u8]>,
    ) -> Result<Option<PathBuf>> {
        std::fs::create_dir_all(&self.config.diffs_dir)
            .map_err(|e| AtalantaError::io("Failed to create diffs dir", e))?;
        let stem = baseline_name.strip_suffix(".png").unwrap_or(baseline_name);
        let write = |kind: &str, data: &[u8]| -> Result<PathBuf> {
            let path = self.config.diffs_dir.join(format!("{stem}-{kind}.png"));
            std::fs::write(&path, data)
                .map_err(|e| AtalantaError::io(format!("Failed to write {}", path.display()), e))?;
            Ok(path)
        };
        for (kind, data) in frames {
            write(kind, data)?;
        }
        diff.map(|d| write("diff", d)).transpose()
    }

    /// Check if a baseline exists for the given name.
    pub fn has_baseline(&self, baseline_name: &str) -> bool {
        self.config.baselines_dir.join(baseline_name).exists()
    }

    /// List all baselines in the baselines directory.
    pub fn list_baselines(&self) -> Result<Vec<String>> {
        if !self.config.baselines_dir.exists() {
            return Ok(Vec::new());
        }

        let entries = std::fs::read_dir(&self.config.baselines_dir)
            .map_err(|e| AtalantaError::io("Failed to read baselines dir", e))?;

        let mut baselines = Vec::new();
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if name.ends_with(".png") {
                    baselines.push(name.to_string());
                }
            }
        }

        baselines.sort();
        Ok(baselines)
    }
}

/// The most pixels a PNG Atalanta decodes may have (64 Mpx; an 8K screen
/// is 33 Mpx).
pub const MAX_PNG_PIXELS: u64 = 64 * 1024 * 1024;

/// A decoded image as 8-bit RGBA.
struct Rgba {
    width: u32,
    height: u32,
    data: Vec<u8>,
}

fn decode(png_bytes: &[u8]) -> Result<Rgba> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    // Palette / 16-bit / grayscale all normalize to 8-bit with an alpha channel.
    decoder.set_transformations(
        png::Transformations::normalize_to_color8() | png::Transformations::ALPHA,
    );
    let bad = |e: png::DecodingError| AtalantaError::Config(format!("invalid PNG: {e}"));
    let mut reader = decoder.read_info().map_err(bad)?;
    // The header names the size; a 100-byte baseline claiming 2^30 x 2^28
    // pixels made the buffer allocation abort the run (audit 2026-11
    // CORE-2: baselines come with pull requests).
    let (w, h) = (reader.info().width, reader.info().height);
    if u64::from(w) * u64::from(h) > MAX_PNG_PIXELS {
        return Err(AtalantaError::Config(format!(
            "invalid PNG: {w}x{h} is larger than any screen ({MAX_PNG_PIXELS} pixels at most)"
        )));
    }
    let mut buf = vec![0; reader.output_buffer_size().unwrap_or(0)];
    let info = reader.next_frame(&mut buf).map_err(bad)?;
    buf.truncate(info.buffer_size());
    let data = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::GrayscaleAlpha => buf
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|ga| [ga[0], ga[0], ga[0], ga[1]])
            .collect(),
        other => {
            return Err(AtalantaError::Config(format!(
                "unsupported PNG color type {other:?}"
            )))
        }
    };
    Ok(Rgba {
        width: info.width,
        height: info.height,
        data,
    })
}

fn encode(img: &Rgba) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, img.width, img.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().ok()?;
    writer.write_image_data(&img.data).ok()?;
    writer.finish().ok()?;
    Some(out)
}

/// Per-channel difference a pixel may have and still count as equal — absorbs
/// anti-aliasing and compression noise without hiding real changes.
const CHANNEL_TOLERANCE: u8 = 16;

/// Per-pixel diff mask: `Some(true)` = differs, `Some(false)` = same,
/// `None` = masked out. Pixels that differ only by anti-aliasing (an edge
/// rendered a sub-pixel differently) count as the same.
fn diff_mask(a: &Rgba, b: &Rgba, masks: &[MaskPreset], rule: PixelRule) -> Vec<Option<bool>> {
    let regions: Vec<MaskRegion> = masks
        .iter()
        .map(|m| m.to_region(a.width, a.height))
        .collect();
    let masked = |x: u32, y: u32| {
        regions
            .iter()
            // Without `r.x + r.width`, which overflowed for a huge region.
            .any(|r| x >= r.x && x - r.x < r.width && y >= r.y && y - r.y < r.height)
    };
    a.data
        .as_chunks::<4>()
        .0
        .iter()
        .zip(b.data.as_chunks::<4>().0.iter())
        .enumerate()
        .map(|(i, (pa, pb))| {
            let (x, y) = (i as u32 % a.width, i as u32 / a.width);
            if masked(x, y) {
                return None;
            }
            let differs = rule.differs(pa, pb);
            Some(differs && !antialiased(a, x, y, b) && !antialiased(b, x, y, a))
        })
        .collect()
}

fn px(img: &Rgba, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * img.width + x) * 4) as usize;
    [
        img.data[i],
        img.data[i + 1],
        img.data[i + 2],
        img.data[i + 3],
    ]
}

fn brightness(p: [u8; 4]) -> f64 {
    0.29889531 * f64::from(p[0]) + 0.58662247 * f64::from(p[1]) + 0.11448223 * f64::from(p[2])
}

/// Whether (x, y) in `img` looks like an anti-aliased edge pixel: it sits
/// between a darker and a brighter neighbour, and that neighbour is part
/// of a flat area in both images. Adapted from pixelmatch (ISC licence,
/// Mapbox), after Vysniauskas, "Anti-aliased Pixel and Intensity Slope
/// Detector" (2009).
fn antialiased(img: &Rgba, x: u32, y: u32, other: &Rgba) -> bool {
    let (x0, y0) = (x.saturating_sub(1), y.saturating_sub(1));
    let (x2, y2) = ((x + 1).min(img.width - 1), (y + 1).min(img.height - 1));
    let mut zeroes = u32::from(x == x0 || x == x2 || y == y0 || y == y2);
    let center = brightness(px(img, x, y));
    let (mut min, mut max) = (0.0f64, 0.0f64);
    let (mut min_at, mut max_at) = ((0, 0), (0, 0));
    for ny in y0..=y2 {
        for nx in x0..=x2 {
            if nx == x && ny == y {
                continue;
            }
            let delta = center - brightness(px(img, nx, ny));
            if delta == 0.0 {
                zeroes += 1;
                if zeroes > 2 {
                    return false;
                }
            } else if delta < min {
                min = delta;
                min_at = (nx, ny);
            } else if delta > max {
                max = delta;
                max_at = (nx, ny);
            }
        }
    }
    if min == 0.0 || max == 0.0 {
        return false;
    }
    (many_siblings(img, min_at) && many_siblings(other, min_at))
        || (many_siblings(img, max_at) && many_siblings(other, max_at))
}

/// More than two neighbours of exactly the same colour.
fn many_siblings(img: &Rgba, (x, y): (u32, u32)) -> bool {
    let (x0, y0) = (x.saturating_sub(1), y.saturating_sub(1));
    let (x2, y2) = ((x + 1).min(img.width - 1), (y + 1).min(img.height - 1));
    let mut zeroes = u32::from(x == x0 || x == x2 || y == y0 || y == y2);
    let c = px(img, x, y);
    for ny in y0..=y2 {
        for nx in x0..=x2 {
            if (nx, ny) != (x, y) && px(img, nx, ny) == c {
                zeroes += 1;
                if zeroes > 2 {
                    return true;
                }
            }
        }
    }
    false
}

/// The biggest 8-connected cluster of changed pixels: (size, bounds).
fn largest_cluster(mask: &[Option<bool>], width: u32, height: u32) -> (usize, Option<MaskRegion>) {
    let mut seen = vec![false; mask.len()];
    let mut best = (0, None);
    let mut stack = Vec::new();
    for start in 0..mask.len() {
        if seen[start] || mask[start] != Some(true) {
            continue;
        }
        let (mut size, mut lo, mut hi) = (0usize, (u32::MAX, u32::MAX), (0u32, 0u32));
        seen[start] = true;
        stack.push(start);
        while let Some(i) = stack.pop() {
            size += 1;
            let (x, y) = (i as u32 % width, i as u32 / width);
            lo = (lo.0.min(x), lo.1.min(y));
            hi = (hi.0.max(x), hi.1.max(y));
            for ny in y.saturating_sub(1)..=(y + 1).min(height - 1) {
                for nx in x.saturating_sub(1)..=(x + 1).min(width - 1) {
                    let j = (ny * width + nx) as usize;
                    if !seen[j] && mask[j] == Some(true) {
                        seen[j] = true;
                        stack.push(j);
                    }
                }
            }
        }
        if size > best.0 {
            best = (
                size,
                Some(MaskRegion {
                    x: lo.0,
                    y: lo.1,
                    width: hi.0 - lo.0 + 1,
                    height: hi.1 - lo.1 + 1,
                }),
            );
        }
    }
    best
}

/// Compare two PNG screenshots pixel by pixel, ignoring masked regions,
/// with the default cluster limit (tests use this form).
///
/// Both images are decoded first: comparing compressed bytes is meaningless
/// (one changed pixel shifts every byte after it in the deflate stream). A
/// size mismatch is a failure with similarity 0.
#[cfg(test)]
fn compare_images(
    baseline: &[u8],
    actual: &[u8],
    masks: &[MaskPreset],
    threshold: f64,
) -> Result<VisualComparisonResult> {
    compare_images_with(baseline, actual, masks, threshold, DEFAULT_CLUSTER_PX)
}

/// Fails when overall similarity drops below `threshold`, or when changed
/// pixels concentrate in one cluster of `cluster_px` or more. The default
/// pixel rule (strict, tolerance 16).
pub fn compare_images_with(
    baseline: &[u8],
    actual: &[u8],
    masks: &[MaskPreset],
    threshold: f64,
    cluster_px: usize,
) -> Result<VisualComparisonResult> {
    compare_images_rule(
        baseline,
        actual,
        masks,
        threshold,
        cluster_px,
        PixelRule::default(),
    )
}

/// [`compare_images_with`] under `rule`. `threshold` is the share of
/// compared (unmasked) pixels that must match, not a colour distance.
pub fn compare_images_rule(
    baseline: &[u8],
    actual: &[u8],
    masks: &[MaskPreset],
    threshold: f64,
    cluster_px: usize,
    rule: PixelRule,
) -> Result<VisualComparisonResult> {
    let (a, b) = (decode(baseline)?, decode(actual)?);
    // Different sizes can't be compared pixel by pixel: say so, instead of
    // "0.00% similar" with no diff (VEL-202).
    if (a.width, a.height) != (b.width, b.height) {
        return Err(AtalantaError::AssertionFailed {
            expected: format!("a {}x{} screen (the baseline's size)", a.width, a.height),
            actual: format!(
                "a {}x{} screen; re-record the baseline with --update-baselines \
                 if the device or window size changed on purpose",
                b.width, b.height
            ),
            selector: "screenshot".into(),
            screenshot: None,
        });
    }
    let mask = diff_mask(&a, &b, masks, rule);
    let total = mask.iter().filter(|m| m.is_some()).count();
    // Masks over most of the screen leave nothing to compare: a mask on a
    // root container or a full-screen list passed with similarity 1.0.
    if total * 2 < mask.len() {
        return Err(AtalantaError::AssertionFailed {
            expected: "at least half of the screen compared".into(),
            actual: format!(
                "masks cover {}% of it; mask smaller elements",
                100 - total * 100 / mask.len().max(1)
            ),
            selector: String::new(),
            screenshot: None,
        });
    }
    let diff = mask.iter().filter(|m| **m == Some(true)).count();
    let similarity = if total == 0 {
        1.0
    } else {
        1.0 - diff as f64 / total as f64
    };
    let (cluster, bounds) = largest_cluster(&mask, a.width, a.height);
    Ok(VisualComparisonResult {
        similarity,
        diff_pixel_count: diff,
        total_pixels: total,
        passed: similarity >= threshold && cluster < cluster_px.max(1),
        diff_image_path: None,
        largest_cluster: cluster,
        largest_cluster_bounds: bounds,
        baseline_written: false,
    })
}

/// Write `data` to `path` through a temporary file in the same directory
/// and a rename, so a parallel worker reading the baseline never sees half
/// a file (B3).
pub fn write_atomic(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_file_name(format!(".{name}.{}-{seq}.tmp", std::process::id()));
    let result = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Whether two screenshots show the same thing (for stable-screenshot
/// gating): no changed pixel outside anti-aliasing.
pub fn images_equal(a: &[u8], b: &[u8]) -> bool {
    match (decode(a), decode(b)) {
        (Ok(a), Ok(b)) if (a.width, a.height) == (b.width, b.height) => {
            !diff_mask(&a, &b, &[], PixelRule::default()).contains(&Some(true))
        }
        _ => false,
    }
}

/// Width and height of a PNG.
pub fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    decode(png).ok().map(|i| (i.width, i.height))
}

/// The actual screenshot, dimmed, with differing pixels in red and masked
/// regions in blue. `None` if the images can't be decoded or differ in size.
pub fn generate_diff_overlay(
    baseline: &[u8],
    actual: &[u8],
    masks: &[MaskPreset],
) -> Option<Vec<u8>> {
    generate_diff_overlay_rule(baseline, actual, masks, PixelRule::default())
}

/// [`generate_diff_overlay`] under `rule`.
pub fn generate_diff_overlay_rule(
    baseline: &[u8],
    actual: &[u8],
    masks: &[MaskPreset],
    rule: PixelRule,
) -> Option<Vec<u8>> {
    let (a, b) = (decode(baseline).ok()?, decode(actual).ok()?);
    if (a.width, a.height) != (b.width, b.height) {
        return None;
    }
    let mask = diff_mask(&a, &b, masks, rule);
    let data = b
        .data
        .as_chunks::<4>()
        .0
        .iter()
        .zip(&mask)
        .flat_map(|(px, m)| match m {
            Some(true) => [255, 0, 0, 255],
            None => [0, 0, 160, 255],
            Some(false) => [px[0] / 3, px[1] / 3, px[2] / 3, 255],
        })
        .collect();
    encode(&Rgba { data, ..b })
}

/// The `region` of `png` as a PNG of its own (`crop:`, VT-12); the region
/// is cut to the image first. `None` if the PNG doesn't decode or nothing
/// of the region is on it.
pub fn crop_png(png: &[u8], region: &MaskRegion) -> Option<Vec<u8>> {
    let img = decode(png).ok()?;
    let r = clip_region(region, img.width, img.height)?;
    let mut data = Vec::with_capacity((r.width * r.height * 4) as usize);
    for y in r.y..r.y + r.height {
        let start = ((y * img.width + r.x) * 4) as usize;
        data.extend_from_slice(&img.data[start..start + (r.width * 4) as usize]);
    }
    encode(&Rgba {
        width: r.width,
        height: r.height,
        data,
    })
}

/// `region` cut to a `w`×`h` image; `None` when nothing is left.
pub fn clip_region(region: &MaskRegion, w: u32, h: u32) -> Option<MaskRegion> {
    let x0 = region.x.min(w);
    let y0 = region.y.min(h);
    let x1 = region.x.saturating_add(region.width).min(w);
    let y1 = region.y.saturating_add(region.height).min(h);
    (x1 > x0 && y1 > y0).then(|| MaskRegion {
        x: x0,
        y: y0,
        width: x1 - x0,
        height: y1 - y0,
    })
}

/// Masks of a `w`×`h` frame moved into the `crop` of it: presets become
/// rectangles of the full frame first, then each is cut to the crop and
/// shifted to its origin; masks outside the crop are dropped.
pub fn crop_masks(masks: &[MaskPreset], w: u32, h: u32, crop: &MaskRegion) -> Vec<MaskPreset> {
    masks
        .iter()
        .filter_map(|m| {
            let r = m.to_region(w, h);
            let x0 = r.x.max(crop.x);
            let y0 = r.y.max(crop.y);
            let x1 = r.x.saturating_add(r.width).min(crop.x + crop.width);
            let y1 = r.y.saturating_add(r.height).min(crop.y + crop.height);
            (x1 > x0 && y1 > y0).then(|| {
                MaskPreset::Custom(MaskRegion {
                    x: x0 - crop.x,
                    y: y0 - crop.y,
                    width: x1 - x0,
                    height: y1 - y0,
                })
            })
        })
        .collect()
}

/// A `w`×`h` black PNG with the given rectangles painted white (tests).
#[cfg(test)]
pub(crate) fn painted(w: u32, h: u32, rects: &[(u32, u32, u32, u32)]) -> Vec<u8> {
    let data = (0..w * h)
        .flat_map(|i| {
            let (x, y) = (i % w, i / w);
            if rects
                .iter()
                .any(|&(rx, ry, rw, rh)| x >= rx && x < rx + rw && y >= ry && y < ry + rh)
            {
                [255, 255, 255, 255]
            } else {
                [0, 0, 0, 255]
            }
        })
        .collect();
    encode(&Rgba {
        width: w,
        height: h,
        data,
    })
    .unwrap()
}

#[cfg(test)]
mod tests {

    /// Audit 2026-11 CORE-2: a tiny PNG whose header claims a huge size is
    /// refused before anything is allocated.
    #[test]
    fn a_png_claiming_a_huge_size_is_refused_not_an_abort() {
        fn crc32(bytes: &[u8]) -> u32 {
            let mut c = 0xffff_ffffu32;
            for &b in bytes {
                c ^= u32::from(b);
                for _ in 0..8 {
                    c = if c & 1 != 0 {
                        0xedb8_8320 ^ (c >> 1)
                    } else {
                        c >> 1
                    };
                }
            }
            !c
        }
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        let mut ihdr = b"IHDR".to_vec();
        // One pixel wide (png's own limits watch its row buffers, not the
        // caller's frame buffer), two billion high: 8 GB to allocate.
        ihdr.extend_from_slice(&1u32.to_be_bytes());
        ihdr.extend_from_slice(&0x7fff_ffffu32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(&ihdr);
        png.extend_from_slice(&crc32(&ihdr).to_be_bytes());
        // A few bytes of image data, so the header is read through.
        let mut idat = b"IDAT".to_vec();
        idat.extend_from_slice(&[0x78, 0x9c, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01]);
        png.extend_from_slice(&8u32.to_be_bytes());
        png.extend_from_slice(&idat);
        png.extend_from_slice(&crc32(&idat).to_be_bytes());
        let e = super::decode(&png).err().expect("refused");
        assert!(e.to_string().contains("larger than any screen"), "{e}");
    }
    use super::*;

    /// A `w`×`h` PNG filled with `color`, with `changed` pixels (top-left
    /// first) set to white.
    fn png(w: u32, h: u32, color: [u8; 4], changed: usize) -> Vec<u8> {
        let data = (0..w * h)
            .flat_map(|i| {
                if (i as usize) < changed {
                    [255, 255, 255, 255]
                } else {
                    color
                }
            })
            .collect();
        encode(&Rgba {
            width: w,
            height: h,
            data,
        })
        .unwrap()
    }

    #[test]
    fn test_identical_images_pass() {
        let data = png(10, 10, [10, 20, 30, 255], 0);
        let result = compare_images(&data, &data, &[], 0.98).unwrap();
        assert!(result.passed);
        assert_eq!(result.similarity, 1.0);
        assert_eq!(result.diff_pixel_count, 0);
    }

    #[test]
    fn test_slightly_different_images() {
        let result = compare_images(
            &png(10, 100, [0, 0, 0, 255], 0),
            &png(10, 100, [0, 0, 0, 255], 10),
            &[],
            0.98,
        )
        .unwrap();
        assert_eq!(result.diff_pixel_count, 10);
        assert_eq!(result.similarity, 0.99);
        assert!(result.passed); // 99% > 98% threshold
    }

    #[test]
    fn test_small_noise_is_tolerated() {
        let result = compare_images(
            &png(10, 10, [100, 100, 100, 255], 0),
            &png(10, 10, [108, 95, 100, 255], 0),
            &[],
            1.0,
        )
        .unwrap();
        assert!(result.passed, "sub-tolerance channel noise must not count");
    }

    #[test]
    fn test_very_different_images_fail() {
        let result = compare_images(
            &png(10, 10, [0, 0, 0, 255], 0),
            &png(10, 10, [255, 255, 255, 255], 0),
            &[],
            0.98,
        )
        .unwrap();
        assert!(!result.passed);
        assert!(result.similarity < 0.1);
    }

    #[test]
    fn test_different_sizes_fail() {
        let result = compare_images(
            &png(10, 10, [0, 0, 0, 255], 0),
            &png(10, 20, [0, 0, 0, 255], 0),
            &[],
            0.98,
        );
        // A failure that names both sizes, not "0.00% similar" (VEL-202).
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("10x10") && msg.contains("10x20"), "{msg}");
    }

    #[test]
    fn test_masks_exclude_dynamic_regions() {
        // The changed pixels are all in row 0 — inside a custom mask.
        let mask = MaskPreset::Custom(MaskRegion {
            x: 0,
            y: 0,
            width: 10,
            height: 1,
        });
        let result = compare_images(
            &png(10, 10, [0, 0, 0, 255], 0),
            &png(10, 10, [0, 0, 0, 255], 10),
            &[mask],
            1.0,
        )
        .unwrap();
        assert!(result.passed);
        assert_eq!(result.total_pixels, 90);
    }

    #[test]
    fn test_diff_overlay_is_a_valid_png() {
        let overlay = generate_diff_overlay(
            &png(4, 4, [0, 0, 0, 255], 0),
            &png(4, 4, [0, 0, 0, 255], 3),
            &[],
        )
        .unwrap();
        let img = decode(&overlay).unwrap();
        assert_eq!(&img.data[..4], &[255, 0, 0, 255]);
    }

    #[test]
    fn test_garbage_input_is_an_error() {
        assert!(compare_images(&[1, 2, 3], &[1, 2, 3], &[], 0.98).is_err());
    }

    #[test]
    fn test_mask_preset_status_bar() {
        let region = MaskPreset::StatusBar.to_region(1080, 2400);
        assert_eq!(region.x, 0);
        assert_eq!(region.y, 0);
        assert_eq!(region.width, 1080);
        assert!(region.height > 0 && region.height < 200);
    }

    #[test]
    fn test_mask_preset_keyboard() {
        let region = MaskPreset::Keyboard.to_region(1080, 2400);
        assert_eq!(region.x, 0);
        assert!(region.y > 1000);
        assert_eq!(region.width, 1080);
    }

    #[test]
    fn test_visual_engine_update_baselines() {
        let dir = tempfile::tempdir().unwrap();
        let config = VisualConfig {
            threshold: 0.98,
            rule: PixelRule::default(),
            cluster_px: DEFAULT_CLUSTER_PX,
            baselines_dir: dir.path().join("baselines"),
            diffs_dir: dir.path().join("diffs"),
            update_baselines: true,
        };
        let engine = VisualEngine::new(config);
        let screenshot = png(2, 2, [0, 0, 0, 255], 0);
        let result = engine
            .assert_screenshot("test.png", &screenshot, &[])
            .unwrap();
        assert!(result.passed);
        assert!(engine.has_baseline("test.png"));
    }

    #[test]
    fn test_visual_engine_missing_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let config = VisualConfig {
            threshold: 0.98,
            rule: PixelRule::default(),
            cluster_px: DEFAULT_CLUSTER_PX,
            baselines_dir: dir.path().join("baselines"),
            diffs_dir: dir.path().join("diffs"),
            update_baselines: false,
        };
        let engine = VisualEngine::new(config);
        let result = engine.assert_screenshot("nonexistent.png", &[1, 2, 3], &[]);
        assert!(result.is_err());
    }

    #[test]
    fn a_small_concentrated_change_fails_despite_high_similarity() {
        // 200×200 = 40,000 px; a 6×6 block is 0.09% of the screen.
        let base = painted(200, 200, &[]);
        let changed = painted(200, 200, &[(50, 50, 6, 6)]);
        let r = compare_images(&base, &changed, &[], 0.98).unwrap();
        assert!(r.similarity > 0.99);
        assert!(!r.passed, "a 36 px cluster is a real change");
        assert_eq!(r.largest_cluster, 36);
        let b = r.largest_cluster_bounds.unwrap();
        assert_eq!((b.x, b.y, b.width, b.height), (50, 50, 6, 6));
    }

    #[test]
    fn scattered_specks_pass() {
        let base = painted(200, 200, &[]);
        let specks: Vec<(u32, u32, u32, u32)> =
            (0..10).map(|i| (i * 18 + 2, i * 18 + 2, 1, 1)).collect();
        let changed = painted(200, 200, &specks);
        let r = compare_images(&base, &changed, &[], 0.98).unwrap();
        assert!(r.passed, "{:?}", r.largest_cluster);
        assert_eq!(r.largest_cluster, 1);
    }

    #[test]
    fn an_edge_shifted_by_anti_aliasing_is_not_a_change() {
        // Baseline: a white square on black. Actual: the same square with a
        // grey column on its right edge, as a sub-pixel shift renders it.
        let w = 40;
        let mut data = Vec::new();
        for y in 0..w {
            for x in 0..w {
                let inside = (10..20).contains(&x) && (10..30).contains(&y);
                data.extend_from_slice(&if inside { [255; 4] } else { [0, 0, 0, 255] });
            }
        }
        let base = encode(&Rgba {
            width: w,
            height: w,
            data: data.clone(),
        })
        .unwrap();
        for y in 10..30u32 {
            let i = ((y * w + 20) * 4) as usize;
            data[i..i + 3].copy_from_slice(&[128, 128, 128]);
        }
        let actual = encode(&Rgba {
            width: w,
            height: w,
            data,
        })
        .unwrap();
        let r = compare_images(&base, &actual, &[], 0.98).unwrap();
        assert!(
            r.passed,
            "anti-aliased edge counted as {} changed px",
            r.diff_pixel_count
        );
    }

    #[test]
    fn equal_screenshots_are_equal() {
        let a = painted(20, 20, &[(1, 1, 3, 3)]);
        assert!(images_equal(&a, &a));
        assert!(!images_equal(&a, &painted(20, 20, &[(10, 10, 5, 5)])));
        assert_eq!(png_size(&a), Some((20, 20)));
    }

    #[test]
    fn masks_over_most_of_the_screen_fail_instead_of_passing() {
        // VEL-215: a mask on the root left nothing compared, similarity 1.0.
        let whole = MaskPreset::Custom(MaskRegion {
            x: 0,
            y: 0,
            width: 10,
            height: 8,
        });
        let r = compare_images(
            &png(10, 10, [0, 0, 0, 255], 0),
            &png(10, 10, [0, 0, 0, 255], 50),
            &[whole],
            0.98,
        );
        let e = r.expect_err("80% masked is not a comparison");
        assert!(e.to_string().contains("80%"), "{e}");
    }

    #[test]
    fn a_huge_mask_region_does_not_overflow() {
        // VEL-204: `r.x + r.width` overflowed (a panic in debug builds).
        let huge = MaskPreset::Custom(MaskRegion {
            x: 5,
            y: 9,
            width: u32::MAX,
            height: u32::MAX,
        });
        let r = compare_images(
            &png(10, 10, [0, 0, 0, 255], 0),
            &png(10, 10, [0, 0, 0, 255], 0),
            &[huge],
            0.98,
        )
        .unwrap();
        assert_eq!(r.total_pixels, 95, "only the last row's right half masked");
    }

    fn engine(dir: &std::path::Path, update: bool) -> VisualEngine {
        VisualEngine::new(VisualConfig {
            baselines_dir: dir.join("baselines"),
            diffs_dir: dir.join("diffs"),
            update_baselines: update,
            ..VisualConfig::default()
        })
    }

    #[test]
    fn a_failure_leaves_expected_actual_and_diff() {
        // VT-6: only diff_<name> was kept.
        let dir = tempfile::tempdir().unwrap();
        let base = painted(40, 40, &[]);
        std::fs::create_dir_all(dir.path().join("baselines")).unwrap();
        std::fs::write(dir.path().join("baselines/home.png"), &base).unwrap();
        let actual = painted(40, 40, &[(5, 5, 8, 8)]);
        let r = engine(dir.path(), false)
            .assert_screenshot("home.png", &actual, &[])
            .unwrap();
        assert!(!r.passed);
        let d = dir.path().join("diffs");
        assert_eq!(std::fs::read(d.join("home-expected.png")).unwrap(), base);
        assert_eq!(std::fs::read(d.join("home-actual.png")).unwrap(), actual);
        assert_eq!(r.diff_image_path, Some(d.join("home-diff.png")));
        assert!(d.join("home-diff.png").is_file());
    }

    #[test]
    fn update_mode_rewrites_only_changed_baselines() {
        // VT-7: every baseline was rewritten, equal or not.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("baselines/home.png");
        let base = painted(40, 40, &[]);
        let e = engine(dir.path(), true);
        assert!(
            e.assert_screenshot("home.png", &base, &[])
                .unwrap()
                .baseline_written
        );
        // The same picture, encoded differently (a speck under the gate).
        let equal = painted(40, 40, &[(3, 3, 1, 1)]);
        assert_ne!(equal, base);
        let r = e.assert_screenshot("home.png", &equal, &[]).unwrap();
        assert!(!r.baseline_written);
        assert_eq!(std::fs::read(&path).unwrap(), base);
        // A real change is written.
        let changed = painted(40, 40, &[(5, 5, 10, 10)]);
        assert!(
            e.assert_screenshot("home.png", &changed, &[])
                .unwrap()
                .baseline_written
        );
        assert_eq!(std::fs::read(&path).unwrap(), changed);
        // No temporary files are left behind.
        let left: Vec<_> = std::fs::read_dir(dir.path().join("baselines"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(left, ["home.png"]);
    }

    #[test]
    fn a_reader_never_sees_half_a_baseline() {
        // B3: a plain fs::write let a parallel worker read a truncated file.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.png");
        let (a, b) = (
            painted(400, 400, &[(0, 0, 400, 200)]),
            painted(400, 400, &[(0, 200, 400, 200)]),
        );
        write_atomic(&path, &a).unwrap();
        let writer = {
            let (path, a, b) = (path.clone(), a.clone(), b.clone());
            std::thread::spawn(move || {
                for i in 0..40 {
                    write_atomic(&path, if i % 2 == 0 { &b } else { &a }).unwrap();
                }
            })
        };
        while !writer.is_finished() {
            let got = std::fs::read(&path).unwrap();
            assert!(got == a || got == b, "read {} bytes", got.len());
        }
        writer.join().unwrap();
    }

    /// A `w`×`h` PNG of one colour.
    fn flat(w: u32, h: u32, color: [u8; 4]) -> Vec<u8> {
        png(w, h, color, 0)
    }

    #[test]
    fn tolerance_sets_how_far_a_channel_may_move() {
        // D-V2: a uniform +20 shift is a change at the default 16 and
        // none at tolerance 24.
        let (a, b) = (
            flat(10, 10, [100, 100, 100, 255]),
            flat(10, 10, [120, 120, 120, 255]),
        );
        assert!(!compare_images_with(&a, &b, &[], 0.98, 20).unwrap().passed);
        let loose = PixelRule {
            tolerance: 24,
            ..PixelRule::default()
        };
        let r = compare_images_rule(&a, &b, &[], 0.98, 20, loose).unwrap();
        assert!(r.passed && r.diff_pixel_count == 0, "{r:?}");
        let exact = PixelRule {
            tolerance: 0,
            ..PixelRule::default()
        };
        let c = flat(10, 10, [101, 100, 100, 255]);
        assert!(
            !compare_images_rule(&a, &c, &[], 0.98, 20, exact)
                .unwrap()
                .passed
        );
    }

    #[test]
    fn perceptual_is_yiq_at_0_1_like_pixelmatch() {
        // D-V2: grey shifts below about 26.4 pass at t=0.1 (0.5053 * d^2 <=
        // 35215 * 0.01), larger ones fail; strict fails the 20 shift.
        let perceptual = PixelRule {
            comparator: atalanta_common::Comparator::Perceptual,
            ..PixelRule::default()
        };
        let base = flat(10, 10, [100, 100, 100, 255]);
        for (d, passes) in [(20u8, true), (26, true), (27, false), (40, false)] {
            let other = flat(10, 10, [100 + d, 100 + d, 100 + d, 255]);
            let r = compare_images_rule(&base, &other, &[], 0.98, 20, perceptual).unwrap();
            assert_eq!(r.passed, passes, "grey +{d}: {r:?}");
        }
        // A blue shift barely moves luma: perceptual forgives it, strict
        // doesn't.
        let bluer = flat(10, 10, [100, 100, 140, 255]);
        assert!(
            compare_images_rule(&base, &bluer, &[], 0.98, 20, perceptual)
                .unwrap()
                .passed
        );
        assert!(
            !compare_images_with(&base, &bluer, &[], 0.98, 20)
                .unwrap()
                .passed
        );
        // Alpha blends over white first.
        assert_eq!(yiq_delta(&[0, 0, 0, 0], &[255, 255, 255, 255]), 0.0);
        let bw = yiq_delta(&[0, 0, 0, 255], &[255, 255, 255, 255]);
        assert!(bw > 0.9 * MAX_YIQ_DELTA && bw <= MAX_YIQ_DELTA, "{bw}");
    }

    #[test]
    fn crop_cuts_the_image_and_moves_masks_into_it() {
        // VT-12.
        let img = painted(20, 20, &[(10, 10, 5, 5)]);
        let crop = MaskRegion {
            x: 8,
            y: 8,
            width: 10,
            height: 40,
        };
        let c = crop_png(&img, &crop).unwrap();
        assert_eq!(png_size(&c), Some((10, 12)));
        let d = decode(&c).unwrap();
        assert_eq!(px(&d, 2, 2), [255, 255, 255, 255]);
        assert_eq!(px(&d, 0, 0), [0, 0, 0, 255]);
        let masks = crop_masks(
            &[
                MaskPreset::StatusBar,
                MaskPreset::Custom(MaskRegion {
                    x: 0,
                    y: 9,
                    width: 10,
                    height: 2,
                }),
            ],
            20,
            20,
            &clip_region(&crop, 20, 20).unwrap(),
        );
        // The 4% status bar (0..0) is outside; the other is cut to x 8..10.
        assert_eq!(masks.len(), 1);
        let r = masks[0].to_region(10, 12);
        assert_eq!((r.x, r.y, r.width, r.height), (0, 1, 2, 2));
        assert!(crop_png(
            &img,
            &MaskRegion {
                x: 30,
                y: 0,
                width: 5,
                height: 5
            }
        )
        .is_none());
    }
}
