//! Throwaway probe: decode a specific animated WebP or GIF emote
//! frame-by-frame, mirroring `src/emote_anim.rs::decode`'s codec calls
//! exactly, and dump each frame to disk as a PNG so a reported "ghosting
//! artifact" can be inspected (and compared across formats/CDN variants of
//! the same emote) instead of guessed at.
//!
//! `cargo run --example webp_emote_probe -- <path-to.webp-or-gif> <out-dir>`

use image::AnimationDecoder;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: webp_emote_probe <path> <out-dir>");
    let out_dir = args.next().expect("usage: webp_emote_probe <path> <out-dir>");
    std::fs::create_dir_all(&out_dir).unwrap();

    let bytes = std::fs::read(&path).unwrap();
    println!("file: {path} ({} bytes)", bytes.len());
    let format = image::guess_format(&bytes).unwrap();
    println!("guessed format: {format:?}");

    let frames = match format {
        image::ImageFormat::Gif => {
            let dec = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes)).unwrap();
            dec.into_frames().collect_frames().unwrap()
        }
        image::ImageFormat::WebP => {
            let dec = image::codecs::webp::WebPDecoder::new(std::io::Cursor::new(bytes)).unwrap();
            println!("has_animation: {}", dec.has_animation());
            dec.into_frames().collect_frames().unwrap()
        }
        other => panic!("unsupported format for this probe: {other:?}"),
    };
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
