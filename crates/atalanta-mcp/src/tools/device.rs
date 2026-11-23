// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

use std::sync::Arc;

use atalanta_common::{AtalantaError, PlatformDriver, Result};
use base64::Engine;
use serde_json::Value;

pub async fn list_devices(driver: &Arc<dyn PlatformDriver>, _args: &Value) -> Result<Value> {
    let devices = driver.list_devices().await?;
    serde_json::to_value(&devices)
        .map_err(|e| AtalantaError::Internal(anyhow::anyhow!("Serialization error: {e}")))
}

/// The longest edge an inline screenshot may have: clients reject or
/// re-encode bigger images, and every pixel is context (row 2.3b).
pub const INLINE_MAX_PX: u32 = 1999;

/// `screenshot` (row 2.3b): written to a file whose path is returned, so
/// the image stays out of the model's context unless asked for. With
/// `inline: true` the image comes back too, its longest edge at most
/// [`INLINE_MAX_PX`].
pub async fn screenshot(
    driver: &Arc<dyn PlatformDriver>,
    device_id: &str,
    args: &Value,
) -> Result<Value> {
    let png_bytes = driver.screenshot(device_id).await?;
    let (w, h) = png_size(&png_bytes).unwrap_or((0, 0));
    let dir = std::env::temp_dir().join("atalanta-mcp-screenshots");
    std::fs::create_dir_all(&dir)
        .map_err(|e| atalanta_common::AtalantaError::io(dir.display(), e))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let clean: String = device_id
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    let path = dir.join(format!("{clean}-{stamp}.png"));
    std::fs::write(&path, &png_bytes)
        .map_err(|e| atalanta_common::AtalantaError::io(path.display(), e))?;
    let mut out = serde_json::json!({
        "path": path,
        "width": w,
        "height": h,
        "size_bytes": png_bytes.len(),
    });
    if args.get("inline").and_then(Value::as_bool).unwrap_or(false) {
        let small = downscale_png(&png_bytes, INLINE_MAX_PX).unwrap_or_else(|| png_bytes.clone());
        let (sw, sh) = png_size(&small).unwrap_or((w, h));
        out["inline_width"] = sw.into();
        out["inline_height"] = sh.into();
        out["format"] = "png".into();
        out["data"] = base64::engine::general_purpose::STANDARD
            .encode(&small)
            .into();
    } else {
        out["note"] = "the image is in `path`; pass inline: true to get it in the reply".into();
    }
    Ok(out)
}

fn png_size(bytes: &[u8]) -> Option<(u32, u32)> {
    let d = png::Decoder::new(std::io::Cursor::new(bytes));
    let r = d.read_info().ok()?;
    let i = r.info();
    Some((i.width, i.height))
}

/// `bytes` with its longest edge at most `max` px (a box filter), or
/// `None` when it already fits or can't be read.
pub fn downscale_png(bytes: &[u8], max: u32) -> Option<Vec<u8>> {
    let mut d = png::Decoder::new(std::io::Cursor::new(bytes));
    d.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut r = d.read_info().ok()?;
    let (w, h) = (r.info().width, r.info().height);
    if w.max(h) <= max {
        return None;
    }
    let mut buf = vec![0; r.output_buffer_size()?];
    let frame = r.next_frame(&mut buf).ok()?;
    let ch = frame.color_type.samples();
    let k = w.max(h).div_ceil(max);
    let (nw, nh) = (w / k, h / k);
    let mut out = vec![0u8; (nw * nh * 3) as usize];
    for y in 0..nh {
        for x in 0..nw {
            let mut acc = [0u32; 3];
            for dy in 0..k {
                for dx in 0..k {
                    let i = (((y * k + dy) * w + (x * k + dx)) as usize) * ch;
                    for c in 0..3 {
                        // Grey images: the one channel for all three.
                        acc[c] += u32::from(buf[i + if ch >= 3 { c } else { 0 }]);
                    }
                }
            }
            let o = ((y * nw + x) * 3) as usize;
            for c in 0..3 {
                out[o + c] = (acc[c] / (k * k)) as u8;
            }
        }
    }
    let mut png_out = Vec::new();
    {
        let mut e = png::Encoder::new(&mut png_out, nw, nh);
        e.set_color(png::ColorType::Rgb);
        e.set_depth(png::BitDepth::Eight);
        let mut w = e.write_header().ok()?;
        w.write_image_data(&out).ok()?;
    }
    Some(png_out)
}

#[cfg(test)]
mod shot_tests {
    use super::*;

    fn png_of(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut e = png::Encoder::new(&mut out, w, h);
            e.set_color(png::ColorType::Rgba);
            e.set_depth(png::BitDepth::Eight);
            let mut wr = e.write_header().unwrap();
            wr.write_image_data(&vec![200u8; (w * h * 4) as usize])
                .unwrap();
        }
        out
    }

    #[test]
    fn a_big_screenshot_is_downscaled_below_the_limit() {
        let big = png_of(1206, 2622);
        let small = downscale_png(&big, INLINE_MAX_PX).unwrap();
        let (w, h) = png_size(&small).unwrap();
        assert!(w.max(h) <= INLINE_MAX_PX, "{w}x{h}");
        assert_eq!((w, h), (603, 1311));
        assert!(downscale_png(&png_of(800, 600), INLINE_MAX_PX).is_none());
    }
}
