//! Decoded, downscaled, animation-aware emote/emoji image cache.
//!
//! egui's built-in image loaders decode animated GIF/WebP at *source* resolution
//! (a 7TV 4× emote ≈ 384 px, ×N frames) and never evict — a wall of animated
//! emotes can balloon RAM into the hundreds of MB. This module decodes frames off
//! the UI thread and **downscales each to render size** before GPU upload (the
//! single biggest win — ~16–45× fewer bytes/frame), drives animation off a global
//! wall-clock, and lets the UI enforce an LRU memory budget. Static images are just
//! a one-frame animation, so this one path serves PNG/JPEG, animated GIF, and
//! animated WebP — and the animate/static toggle is "cycle frames" vs "frame 0".

use eframe::egui;
use image::AnimationDecoder;

/// Frames are downscaled so their longest side is ≤ this (≈2× the 28 px render
/// height, kept for crispness on hi-DPI). The dominant memory saving.
const TARGET_PX: u32 = 56;
/// Per-frame delay floor (20 fps) — clamps absurdly fast GIFs and the repaint rate.
const MIN_DELAY: f32 = 0.05;

/// One emote's state in the cache.
pub enum EmoteLoad {
    /// A background decode has been spawned.
    Loading,
    /// Decoded + downscaled off-thread; awaiting GPU upload on the UI thread.
    Decoded(Vec<egui::ColorImage>, Vec<f32>),
    /// Uploaded and ready to draw.
    Ready(EmoteAnim),
    /// Unreadable / undecodable — the renderer falls back to text.
    Failed,
}

/// A decoded emote: 1 frame (static) or N frames (animated), uploaded to textures.
pub struct EmoteAnim {
    frames: Vec<egui::TextureHandle>,
    /// Cumulative frame end-times in seconds; `cum.last()` is the loop duration.
    cum: Vec<f32>,
    /// Approximate GPU bytes (for the LRU budget).
    pub bytes: usize,
    /// Wall-clock time this emote was last drawn (for LRU eviction).
    pub last_drawn: f64,
}

impl EmoteAnim {
    pub fn is_animated(&self) -> bool {
        self.frames.len() > 1 && self.cum.last().is_some_and(|&t| t > 0.0)
    }

    pub fn size(&self) -> egui::Vec2 {
        self.frames[0].size_vec2()
    }

    /// `(texture, seconds-until-next-frame)` for global clock time `t`. For a static
    /// emote (or `t` infinite-loop edge) returns frame 0 with no scheduled repaint.
    pub fn frame_at(&self, t: f64) -> (&egui::TextureHandle, f32) {
        let total = self.cum.last().copied().unwrap_or(0.0);
        if !self.is_animated() {
            return (&self.frames[0], f32::INFINITY);
        }
        let pos = (t.rem_euclid(total as f64)) as f32;
        let idx = self.cum.iter().position(|&c| c > pos).unwrap_or(0);
        let remaining = (self.cum[idx] - pos).max(MIN_DELAY);
        (&self.frames[idx], remaining)
    }
}

/// Decode (and downscale) image bytes into frames + per-frame delays, off the UI
/// thread. `None` on an unreadable/undecodable input.
pub fn decode(bytes: &[u8]) -> Option<(Vec<egui::ColorImage>, Vec<f32>)> {
    let (rgba, delays) = match image::guess_format(bytes).ok()? {
        image::ImageFormat::Gif => {
            let dec = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes)).ok()?;
            collect_frames(dec)?
        }
        image::ImageFormat::WebP => {
            let dec = image::codecs::webp::WebPDecoder::new(std::io::Cursor::new(bytes)).ok()?;
            if dec.has_animation() {
                collect_frames(dec)?
            } else {
                (vec![image::load_from_memory(bytes).ok()?.to_rgba8()], vec![0.0])
            }
        }
        _ => (vec![image::load_from_memory(bytes).ok()?.to_rgba8()], vec![0.0]),
    };
    let images = rgba
        .into_iter()
        .map(|img| {
            let img = downscale(img);
            let size = [img.width() as usize, img.height() as usize];
            egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw())
        })
        .collect();
    Some((images, delays))
}

fn collect_frames<'a>(
    dec: impl AnimationDecoder<'a>,
) -> Option<(Vec<image::RgbaImage>, Vec<f32>)> {
    let frames = dec.into_frames().collect_frames().ok()?;
    if frames.is_empty() {
        return None;
    }
    let mut imgs = Vec::with_capacity(frames.len());
    let mut delays = Vec::with_capacity(frames.len());
    for f in frames {
        let (num, den) = f.delay().numer_denom_ms();
        let secs = if den == 0 {
            0.1
        } else {
            (num as f32 / den as f32) / 1000.0
        };
        delays.push(secs.max(MIN_DELAY));
        imgs.push(f.into_buffer());
    }
    Some((imgs, delays))
}

fn downscale(img: image::RgbaImage) -> image::RgbaImage {
    let (w, h) = img.dimensions();
    let longest = w.max(h);
    if longest <= TARGET_PX {
        return img;
    }
    let scale = TARGET_PX as f32 / longest as f32;
    let nw = ((w as f32 * scale).round() as u32).max(1);
    let nh = ((h as f32 * scale).round() as u32).max(1);
    image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Triangle)
}

/// Upload decoded frames to GPU textures on the UI thread, producing a drawable
/// [`EmoteAnim`]. `key` makes the texture names unique + stable per source path.
pub fn upload(
    images: Vec<egui::ColorImage>,
    delays: Vec<f32>,
    ctx: &egui::Context,
    key: &str,
) -> EmoteAnim {
    let mut frames = Vec::with_capacity(images.len());
    let mut cum = Vec::with_capacity(images.len());
    let mut bytes = 0usize;
    let mut acc = 0.0f32;
    for (i, ci) in images.into_iter().enumerate() {
        bytes += ci.width() * ci.height() * 4;
        acc += delays.get(i).copied().unwrap_or(0.0);
        cum.push(acc);
        let tex = ctx.load_texture(format!("emote_{key}_{i}"), ci, egui::TextureOptions::LINEAR);
        frames.push(tex);
    }
    EmoteAnim {
        frames,
        cum,
        bytes,
        last_drawn: 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anim(cum: Vec<f32>) -> EmoteAnim {
        EmoteAnim {
            frames: Vec::new(), // not dereferenced by frame_at when computing index logic below
            cum,
            bytes: 0,
            last_drawn: 0.0,
        }
    }

    #[test]
    fn frame_index_math_wraps_on_loop() {
        // 3 frames, 0.1s each → cumulative [0.1, 0.2, 0.3], loop 0.3s.
        let a = anim(vec![0.1, 0.2, 0.3]);
        // helper replicating frame_at's index pick (without touching textures).
        let pick = |t: f64| {
            let total = *a.cum.last().unwrap();
            let pos = (t.rem_euclid(total as f64)) as f32;
            a.cum.iter().position(|&c| c > pos).unwrap_or(0)
        };
        assert_eq!(pick(0.0), 0);
        assert_eq!(pick(0.15), 1);
        assert_eq!(pick(0.25), 2);
        assert_eq!(pick(0.30), 0); // wraps
        assert_eq!(pick(0.45), 1); // 0.45 % 0.3 = 0.15 → frame 1
    }
}
