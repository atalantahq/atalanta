// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! Whether a screenshot changed where the accessibility tree can't see it.
//!
//! Explore decides whether an action did anything from the tree's hash. A
//! canvas, a game or a video view changes pixels the tree never shows (a
//! stroke drawn, a maze piece moved), so drawing tasks were stopped as
//! "stuck" while they progressed (AndroidWorld rerun, 2026-11-07). When the
//! turn already took a screenshot, a coarse grid of the image before and
//! after the action settles it: the status and navigation bars are left out
//! (a clock, a notification icon), and a few cells must change, so a
//! blinking cursor doesn't count.

const COLS: usize = 48;
const ROWS: usize = 96;
/// Rows of the image left out at the top and bottom, as a fraction.
const EDGE: f32 = 0.05;
/// A cell's mean in some colour channel must move by more than this (0–255).
const CELL_DELTA: i16 = 10;
/// And at least this many cells must move.
const MIN_CELLS: usize = 3;

/// A grid of each cell's mean colour (3 bytes a cell), or `None` if the PNG
/// can't be decoded. Colour, not luminance, and a grid twice as fine, read
/// from every second pixel: a 1 px cyan stroke on a white canvas moved no
/// 45 px cell's luminance by more than 5, and a stroke that was drawn read
/// as no change (AndroidWorld BrowserDraw, 2026-11-08).
pub fn signature(png_bytes: &[u8]) -> Option<Vec<u8>> {
    decode(png_bytes)?.signature()
}

impl Image {
    /// [`signature`] of an image already decoded: explore decodes each
    /// screenshot once a turn and reuses it for the model's copy, the
    /// swatch colours and this (review 04 #3).
    pub fn signature(&self) -> Option<Vec<u8>> {
        let img = self;
        let (w, h) = (img.width, img.height);
        if w < COLS || h < ROWS {
            return None;
        }
        let top = (h as f32 * EDGE) as usize;
        let bottom = h - top;
        let span = bottom - top;
        let mut sums = vec![0u64; COLS * ROWS * 3];
        let mut counts = vec![0u64; COLS * ROWS];
        for y in (top..bottom).step_by(2) {
            let row = ((y - top) * ROWS / span).min(ROWS - 1);
            for x in (0..w).step_by(2) {
                let col = (x * COLS / w).min(COLS - 1);
                let cell = row * COLS + col;
                let p = (y * w + x) * 3;
                for c in 0..3 {
                    sums[cell * 3 + c] += u64::from(img.rgb[p + c]);
                }
                counts[cell] += 1;
            }
        }
        Some(
            sums.iter()
                .enumerate()
                .map(|(i, s)| match counts[i / 3] {
                    0 => 0,
                    n => (s / n) as u8,
                })
                .collect(),
        )
    }
}

/// Whether two signatures show a visible change: enough cells whose mean
/// moved in some channel, and not all in one column a few rows tall (a
/// blinking text cursor's shape at this grid).
pub fn differs(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let moved: Vec<usize> = a
        .chunks(3)
        .zip(b.chunks(3))
        .enumerate()
        .filter(|(_, (x, y))| {
            x.iter()
                .zip(y.iter())
                .any(|(p, q)| (i16::from(*p) - i16::from(*q)).abs() > CELL_DELTA)
        })
        .map(|(i, _)| i)
        .collect();
    if moved.len() < MIN_CELLS {
        return false;
    }
    let cols: std::collections::HashSet<usize> = moved.iter().map(|i| i % COLS).collect();
    let rows = moved.iter().map(|i| i / COLS);
    let tall = rows.clone().max().unwrap_or(0) - rows.min().unwrap_or(0) + 1;
    cols.len() > 1 || tall > CURSOR_ROWS
}

/// A change in one grid column at most this many rows tall is a cursor.
const CURSOR_ROWS: usize = 4;

/// Whether the screenshot `after` changed from `before` (PNGs).
pub fn changed(before: &[u8], after: &[u8]) -> bool {
    match (signature(before), signature(after)) {
        (Some(a), Some(b)) => differs(&a, &b),
        _ => false,
    }
}

/// A decoded screenshot: RGB rows.
pub struct Image {
    pub width: usize,
    pub height: usize,
    rgb: Vec<u8>,
}

/// Decode a PNG to RGB, or `None`.
pub fn decode(png_bytes: &[u8]) -> Option<Image> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    // A header claiming a huge size would abort on allocation (audit
    // 2026-11 CORE-2).
    let (w, h) = (reader.info().width, reader.info().height);
    if u64::from(w) * u64::from(h) > atalanta_core::visual::MAX_PNG_PIXELS {
        return None;
    }
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width as usize, info.height as usize);
    let channels = match info.color_type {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => return None,
    };
    let mut rgb = Vec::with_capacity(w * h * 3);
    for px in buf[..w * h * channels].chunks(channels) {
        if channels >= 3 {
            rgb.extend_from_slice(&px[..3]);
        } else {
            rgb.extend_from_slice(&[px[0], px[0], px[0]]);
        }
    }
    Some(Image {
        width: w,
        height: h,
        rgb,
    })
}

impl Image {
    /// The colour filling the middle of a rectangle (image pixels): the
    /// mean of its inner half, when that is one colour (every sampled pixel
    /// within 24 of the mean in each channel); `None` for an icon, a
    /// picture or text, or a rectangle off the image.
    pub fn solid_color(&self, x: i32, y: i32, w: i32, h: i32) -> Option<[u8; 3]> {
        let (x0, y0) = ((x + w / 4).max(0) as usize, (y + h / 4).max(0) as usize);
        let (x1, y1) = (
            ((x + w - w / 4).max(0) as usize).min(self.width),
            ((y + h - h / 4).max(0) as usize).min(self.height),
        );
        if x1 <= x0 + 1 || y1 <= y0 + 1 {
            return None;
        }
        // An odd step: an even one fell in phase with a striped icon and
        // saw only one of its colours.
        let step = ((x1 - x0).max(y1 - y0) / 24) | 1;
        let mut px: Vec<[u8; 3]> = Vec::new();
        for yy in (y0..y1).step_by(step) {
            for xx in (x0..x1).step_by(step) {
                let i = (yy * self.width + xx) * 3;
                px.push([self.rgb[i], self.rgb[i + 1], self.rgb[i + 2]]);
            }
        }
        let n = px.len() as u32;
        let mean = [0, 1, 2].map(|c| (px.iter().map(|p| u32::from(p[c])).sum::<u32>() / n) as u8);
        px.iter()
            .all(|p| (0..3).all(|c| (i16::from(p[c]) - i16::from(mean[c])).abs() <= 24))
            .then_some(mean)
    }
}

/// The nearest CSS named colour (CSS Color Module Level 4, the 140
/// distinct names with "aqua"/"fuchsia" read as "cyan"/"magenta"), for the
/// model to match a swatch to a colour it was asked for. The whole standard
/// list, not a palette picked for one app.
pub fn color_name(c: [u8; 3]) -> &'static str {
    const NAMES: &[(&str, [u8; 3])] = &[
        ("alice blue", [240, 248, 255]),
        ("antique white", [250, 235, 215]),
        ("cyan", [0, 255, 255]),
        ("aquamarine", [127, 255, 212]),
        ("azure", [240, 255, 255]),
        ("beige", [245, 245, 220]),
        ("bisque", [255, 228, 196]),
        ("black", [0, 0, 0]),
        ("blanched almond", [255, 235, 205]),
        ("blue", [0, 0, 255]),
        ("blue violet", [138, 43, 226]),
        ("brown", [165, 42, 42]),
        ("burlywood", [222, 184, 135]),
        ("cadet blue", [95, 158, 160]),
        ("chartreuse", [127, 255, 0]),
        ("chocolate", [210, 105, 30]),
        ("coral", [255, 127, 80]),
        ("cornflower blue", [100, 149, 237]),
        ("cornsilk", [255, 248, 220]),
        ("crimson", [220, 20, 60]),
        ("dark blue", [0, 0, 139]),
        ("dark cyan", [0, 139, 139]),
        ("dark goldenrod", [184, 134, 11]),
        ("dark gray", [169, 169, 169]),
        ("dark green", [0, 100, 0]),
        ("dark khaki", [189, 183, 107]),
        ("dark magenta", [139, 0, 139]),
        ("dark olive green", [85, 107, 47]),
        ("dark orange", [255, 140, 0]),
        ("dark orchid", [153, 50, 204]),
        ("dark red", [139, 0, 0]),
        ("dark salmon", [233, 150, 122]),
        ("dark sea green", [143, 188, 143]),
        ("dark slate blue", [72, 61, 139]),
        ("dark slate gray", [47, 79, 79]),
        ("dark turquoise", [0, 206, 209]),
        ("dark violet", [148, 0, 211]),
        ("deep pink", [255, 20, 147]),
        ("deep sky blue", [0, 191, 255]),
        ("dim gray", [105, 105, 105]),
        ("dodger blue", [30, 144, 255]),
        ("firebrick", [178, 34, 34]),
        ("floral white", [255, 250, 240]),
        ("forest green", [34, 139, 34]),
        ("magenta", [255, 0, 255]),
        ("gainsboro", [220, 220, 220]),
        ("ghost white", [248, 248, 255]),
        ("gold", [255, 215, 0]),
        ("goldenrod", [218, 165, 32]),
        ("gray", [128, 128, 128]),
        ("green", [0, 128, 0]),
        ("green yellow", [173, 255, 47]),
        ("honeydew", [240, 255, 240]),
        ("hot pink", [255, 105, 180]),
        ("indian red", [205, 92, 92]),
        ("indigo", [75, 0, 130]),
        ("ivory", [255, 255, 240]),
        ("khaki", [240, 230, 140]),
        ("lavender", [230, 230, 250]),
        ("lavender blush", [255, 240, 245]),
        ("lawn green", [124, 252, 0]),
        ("lemon chiffon", [255, 250, 205]),
        ("light blue", [173, 216, 230]),
        ("light coral", [240, 128, 128]),
        ("light cyan", [224, 255, 255]),
        ("light goldenrod yellow", [250, 250, 210]),
        ("light gray", [211, 211, 211]),
        ("light green", [144, 238, 144]),
        ("light pink", [255, 182, 193]),
        ("light salmon", [255, 160, 122]),
        ("light sea green", [32, 178, 170]),
        ("light sky blue", [135, 206, 250]),
        ("light slate gray", [119, 136, 153]),
        ("light steel blue", [176, 196, 222]),
        ("light yellow", [255, 255, 224]),
        ("lime", [0, 255, 0]),
        ("lime green", [50, 205, 50]),
        ("linen", [250, 240, 230]),
        ("maroon", [128, 0, 0]),
        ("medium aquamarine", [102, 205, 170]),
        ("medium blue", [0, 0, 205]),
        ("medium orchid", [186, 85, 211]),
        ("medium purple", [147, 112, 219]),
        ("medium sea green", [60, 179, 113]),
        ("medium slate blue", [123, 104, 238]),
        ("medium spring green", [0, 250, 154]),
        ("medium turquoise", [72, 209, 204]),
        ("medium violet red", [199, 21, 133]),
        ("midnight blue", [25, 25, 112]),
        ("mint cream", [245, 255, 250]),
        ("misty rose", [255, 228, 225]),
        ("moccasin", [255, 228, 181]),
        ("navajo white", [255, 222, 173]),
        ("navy", [0, 0, 128]),
        ("old lace", [253, 245, 230]),
        ("olive", [128, 128, 0]),
        ("olive drab", [107, 142, 35]),
        ("orange", [255, 165, 0]),
        ("orange red", [255, 69, 0]),
        ("orchid", [218, 112, 214]),
        ("pale goldenrod", [238, 232, 170]),
        ("pale green", [152, 251, 152]),
        ("pale turquoise", [175, 238, 238]),
        ("pale violet red", [219, 112, 147]),
        ("papaya whip", [255, 239, 213]),
        ("peach puff", [255, 218, 185]),
        ("peru", [205, 133, 63]),
        ("pink", [255, 192, 203]),
        ("plum", [221, 160, 221]),
        ("powder blue", [176, 224, 230]),
        ("purple", [128, 0, 128]),
        ("rebecca purple", [102, 51, 153]),
        ("red", [255, 0, 0]),
        ("rosy brown", [188, 143, 143]),
        ("royal blue", [65, 105, 225]),
        ("saddle brown", [139, 69, 19]),
        ("salmon", [250, 128, 114]),
        ("sandy brown", [244, 164, 96]),
        ("sea green", [46, 139, 87]),
        ("seashell", [255, 245, 238]),
        ("sienna", [160, 82, 45]),
        ("silver", [192, 192, 192]),
        ("sky blue", [135, 206, 235]),
        ("slate blue", [106, 90, 205]),
        ("slate gray", [112, 128, 144]),
        ("snow", [255, 250, 250]),
        ("spring green", [0, 255, 127]),
        ("steel blue", [70, 130, 180]),
        ("tan", [210, 180, 140]),
        ("teal", [0, 128, 128]),
        ("thistle", [216, 191, 216]),
        ("tomato", [255, 99, 71]),
        ("turquoise", [64, 224, 208]),
        ("violet", [238, 130, 238]),
        ("wheat", [245, 222, 179]),
        ("white", [255, 255, 255]),
        ("white smoke", [245, 245, 245]),
        ("yellow", [255, 255, 0]),
        ("yellow green", [154, 205, 50]),
    ];
    let d = |a: [u8; 3]| {
        (0..3)
            .map(|i| (i32::from(a[i]) - i32::from(c[i])).pow(2))
            .sum::<i32>()
    };
    NAMES
        .iter()
        .min_by_key(|(_, rgb)| d(*rgb))
        .map(|(n, _)| *n)
        .unwrap_or("")
}

/// `png` shrunk so its long side is at most `max_side` pixels (an area
/// average), re-encoded as an RGB PNG; `None` when it is that small already
/// or isn't a PNG this can decode (the caller sends the original).
pub fn downscale(png_bytes: &[u8], max_side: usize) -> Option<Vec<u8>> {
    decode(png_bytes)?.downscaled_png(max_side)
}

impl Image {
    /// [`downscale`] of an image already decoded.
    pub fn downscaled_png(&self, max_side: usize) -> Option<Vec<u8>> {
        let img = self;
        let (w, h) = (img.width, img.height);
        let long = w.max(h);
        if long <= max_side || max_side == 0 {
            return None;
        }
        let nw = (w * max_side / long).max(1);
        let nh = (h * max_side / long).max(1);
        let mut out = Vec::with_capacity(nw * nh * 3);
        for y in 0..nh {
            let (y0, y1) = (y * h / nh, ((y + 1) * h / nh).max(y * h / nh + 1));
            for x in 0..nw {
                let (x0, x1) = (x * w / nw, ((x + 1) * w / nw).max(x * w / nw + 1));
                let mut sum = [0u32; 3];
                for yy in y0..y1.min(h) {
                    for xx in x0..x1.min(w) {
                        let p = (yy * w + xx) * 3;
                        for (acc, v) in sum.iter_mut().zip(&img.rgb[p..p + 3]) {
                            *acc += u32::from(*v);
                        }
                    }
                }
                let n = ((y1.min(h) - y0) * (x1.min(w) - x0)).max(1) as u32;
                out.extend(sum.map(|v| (v / n) as u8));
            }
        }
        let mut bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut bytes, nw as u32, nh as u32);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            let mut wr = enc.write_header().ok()?;
            wr.write_image_data(&out).ok()?;
        }
        Some(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_of(w: u32, h: u32, paint: impl Fn(u32, u32) -> u8) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, w, h);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            let mut wr = enc.write_header().unwrap();
            let mut data = Vec::with_capacity((w * h * 3) as usize);
            for y in 0..h {
                for x in 0..w {
                    let v = paint(x, y);
                    data.extend([v, v, v]);
                }
            }
            wr.write_image_data(&data).unwrap();
        }
        out
    }

    #[test]
    fn a_swatch_has_a_colour_and_an_icon_has_none() {
        // Left half cyan, right half black and white stripes.
        let png = {
            let mut out = Vec::new();
            {
                let mut enc = png::Encoder::new(&mut out, 200, 100);
                enc.set_color(png::ColorType::Rgb);
                enc.set_depth(png::BitDepth::Eight);
                let mut wr = enc.write_header().unwrap();
                let mut data = Vec::new();
                for _y in 0..100 {
                    for x in 0..200u32 {
                        let p = if x < 100 {
                            [0, 255, 255]
                        } else if x % 4 < 2 {
                            [0, 0, 0]
                        } else {
                            [255, 255, 255]
                        };
                        data.extend(p);
                    }
                }
                wr.write_image_data(&data).unwrap();
            }
            out
        };
        let img = decode(&png).unwrap();
        let c = img.solid_color(0, 0, 100, 100).unwrap();
        assert_eq!(c, [0, 255, 255]);
        assert_eq!(color_name(c), "cyan");
        assert_eq!(img.solid_color(100, 0, 100, 100), None);
        assert_eq!(img.solid_color(500, 500, 10, 10), None);
        assert_eq!(color_name([0, 130, 126]), "teal");
        // Two close purples get two names.
        assert_ne!(color_name([153, 50, 204]), color_name([148, 0, 211]));
    }

    #[test]
    fn a_large_screenshot_is_shrunk_and_a_small_one_is_left_alone() {
        let big = png_of(1080, 2400, |x, _| if x < 540 { 0 } else { 255 });
        let small = downscale(&big, 1000).expect("shrunk");
        let img = decode(&small).unwrap();
        assert_eq!((img.width, img.height), (450, 1000));
        // Area averages keep what was where.
        assert_eq!(img.solid_color(0, 0, 200, 1000), Some([0, 0, 0]));
        assert_eq!(img.solid_color(250, 0, 200, 1000), Some([255, 255, 255]));
        assert!(small.len() < big.len());
        assert!(downscale(&png_of(400, 800, |_, _| 9), 1000).is_none());
        assert!(downscale(b"not a png", 1000).is_none());
    }

    #[test]
    fn a_stroke_counts_and_the_status_bar_and_a_cursor_do_not() {
        let blank = png_of(240, 480, |_, _| 255);
        // A diagonal stroke across the middle: many cells.
        let stroke = png_of(240, 480, |x, y| {
            if (y as i32 - 2 * x as i32).abs() < 6 && y > 100 {
                0
            } else {
                255
            }
        });
        assert!(changed(&blank, &stroke));
        // The clock in the status bar (top 5%) changed: ignored.
        let clock = png_of(240, 480, |x, y| if y < 20 && x < 60 { 0 } else { 255 });
        assert!(!changed(&blank, &clock));
        // A text cursor at phone size (3 x 55 px): a few cells of one column.
        let page = png_of(1080, 2340, |_, _| 255);
        let cursor = png_of(1080, 2340, |x, y| {
            if (540..543).contains(&x) && (1200..1255).contains(&y) {
                0
            } else {
                255
            }
        });
        assert!(!changed(&page, &cursor));
        assert!(!changed(&blank, &blank));
        // A thin cyan stroke (2 px) across a white canvas: drawn, so a change.
        let cyan_line = {
            let mut out = Vec::new();
            {
                let mut enc = png::Encoder::new(&mut out, 1080, 2340);
                enc.set_color(png::ColorType::Rgb);
                enc.set_depth(png::BitDepth::Eight);
                let mut wr = enc.write_header().unwrap();
                let mut data = Vec::with_capacity(1080 * 2340 * 3);
                for y in 0..2340u32 {
                    for x in 0..1080u32 {
                        let p: [u8; 3] = if (1000..1002).contains(&y) && (300..800).contains(&x) {
                            [0, 255, 255]
                        } else {
                            [255, 255, 255]
                        };
                        data.extend(p);
                    }
                }
                wr.write_image_data(&data).unwrap();
            }
            out
        };
        let white = png_of(1080, 2340, |_, _| 255);
        assert!(changed(&white, &cyan_line));
        assert!(!changed(b"not a png", &blank));
    }
}
