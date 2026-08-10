//! Throwaway probe: decode a specific animated WebP emote frame-by-frame,
//! mirroring `src/emote_anim.rs::decode`'s WebP path exactly, and dump each
//! frame to disk as a PNG so a reported "ghosting artifact" can be inspected
//! frame by frame instead of guessed at.
//!
//! `cargo run --example webp_emote_probe -- <path-to.webp> <out-dir>`

use image::AnimationDecoder;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: webp_emote_probe <path.webp> <out-dir>");
    let out_dir = args.next().expect("usage: webp_emote_probe <path.webp> <out-dir>");
    std::fs::create_dir_all(&out_dir).unwrap();

    let bytes = std::fs::read(&path).unwrap();
    println!("file: {path} ({} bytes)", bytes.len());
    println!("guessed format: {:?}", image::guess_format(&bytes));

    let dec = image::codecs::webp::WebPDecoder::new(std::io::Cursor::new(bytes)).unwrap();
    println!("has_animation: {}", dec.has_animation());

    let frames = dec.into_frames().collect_frames().unwrap();
    println!("frame count: {}", frames.len());

    let mut prev_size: Option<(u32, u32)> = None;
    for (i, f) in frames.iter().enumerate() {
        let (num, den) = f.delay().numer_denom_ms();
        let buf = f.buffer();
        let (w, h) = (buf.width(), buf.height());
        let size_changed = prev_size.is_some_and(|(pw, ph)| (pw, ph) != (w, h));
        println!(
            "frame {i:>2}: {w}x{h} left={} top={} delay={num}/{den}ms{}",
            f.left(),
            f.top(),
            if size_changed { "  <-- SIZE CHANGED FROM PREVIOUS FRAME" } else { "" }
        );
        prev_size = Some((w, h));
        let out = image::DynamicImage::ImageRgba8(buf.clone());
        out.save(format!("{out_dir}/frame_{i:02}.png")).unwrap();
    }
}
