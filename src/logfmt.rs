//! Log-formatting helpers: brand-colored `[Platform]` tags for log messages,
//! and an ANSI-stripping writer so those colors never pollute the file log.
//!
//! Color is decided once at startup ([`set_color_enabled`], from whether
//! stderr is a real terminal): [`PlatTag`] then embeds truecolor ANSI escapes
//! in the message text itself. The stderr layer passes them through; the
//! rolling-file layer wraps its writer in [`StripAnsi`] so the same event is
//! written clean. In release builds (no console) color is simply off.

use std::fmt;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::models::Platform;

static COLOR: AtomicBool = AtomicBool::new(false);

/// Enable/disable ANSI colors in log-message text (set once at startup when
/// stderr is a terminal that renders escapes).
pub fn set_color_enabled(on: bool) {
    COLOR.store(on, Ordering::Relaxed);
}

pub fn color_enabled() -> bool {
    COLOR.load(Ordering::Relaxed)
}

/// `[Twitch]`-style tag in the platform's brand color (when color is on).
/// Obtain via [`Platform::tag`]; embed with `{}` in any log message.
pub struct PlatTag(pub Platform);

impl PlatTag {
    /// Brand color as RGB (Twitch purple, YouTube red, Kick green, NRK blue,
    /// Nebula indigo).
    fn rgb(&self) -> (u8, u8, u8) {
        match self.0 {
            Platform::Twitch => (145, 70, 255), // #9146FF
            Platform::YouTube => (255, 68, 68), // #FF4444 (pure #FF0000 reads as an error color)
            Platform::Kick => (83, 252, 24),    // #53FC18
            Platform::Nrk => (0, 137, 224),     // #0089E0 (nrk.no interface blue)
            Platform::Nebula => (94, 92, 230),  // #5E5CE6 (nebula.tv accent indigo)
            Platform::Generic => (150, 150, 150),
        }
    }
}

impl fmt::Display for PlatTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if color_enabled() {
            let (r, g, b) = self.rgb();
            write!(f, "\x1b[38;2;{r};{g};{b}m[{}]\x1b[0m", self.0.label())
        } else {
            write!(f, "[{}]", self.0.label())
        }
    }
}

impl Platform {
    /// Colored log tag for this platform, e.g. `[YouTube]` in YouTube red.
    pub fn tag(self) -> PlatTag {
        PlatTag(self)
    }
}

/// `MakeWriter` wrapper that strips ANSI escape sequences, for log sinks that
/// must stay plain text (the rolling file). CSI sequences (`ESC [ ... final`)
/// are removed; a bare `ESC` plus its follow byte likewise.
pub struct StripAnsiMake<M>(pub M);

impl<'a, M: tracing_subscriber::fmt::MakeWriter<'a>> tracing_subscriber::fmt::MakeWriter<'a>
    for StripAnsiMake<M>
{
    type Writer = StripAnsi<M::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        StripAnsi { inner: self.0.make_writer(), state: AnsiState::Text }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum AnsiState {
    Text,
    /// Saw `ESC`, deciding sequence type from the next byte.
    Esc,
    /// Inside `ESC [ ...` — skip until a final byte (0x40–0x7E).
    Csi,
}

/// Writer that filters ANSI escapes out of the byte stream. Stateful across
/// `write` calls so a sequence split over two writes is still removed.
pub struct StripAnsi<W> {
    inner: W,
    state: AnsiState,
}

impl<W: io::Write> io::Write for StripAnsi<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Filter into a scratch buffer, then write it out whole. Report the
        // full input length as consumed — the escapes were "written" (dropped).
        let mut clean = Vec::with_capacity(buf.len());
        for &b in buf {
            match self.state {
                AnsiState::Text => {
                    if b == 0x1b {
                        self.state = AnsiState::Esc;
                    } else {
                        clean.push(b);
                    }
                }
                AnsiState::Esc => {
                    self.state = if b == b'[' { AnsiState::Csi } else { AnsiState::Text };
                }
                AnsiState::Csi => {
                    if (0x40..=0x7e).contains(&b) {
                        self.state = AnsiState::Text;
                    }
                }
            }
        }
        self.inner.write_all(&clean)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn strip(input: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut w = StripAnsi { inner: &mut out, state: AnsiState::Text };
            w.write_all(input).unwrap();
        }
        out
    }

    #[test]
    fn strips_csi_sequences_and_keeps_text() {
        let colored = b"a \x1b[38;2;145;70;255m[Twitch]\x1b[0m b";
        assert_eq!(strip(colored), b"a [Twitch] b");
    }

    #[test]
    fn strips_sequence_split_across_writes() {
        let mut out = Vec::new();
        {
            let mut w = StripAnsi { inner: &mut out, state: AnsiState::Text };
            w.write_all(b"x\x1b[3").unwrap();
            w.write_all(b"1my\x1b[0m").unwrap();
        }
        assert_eq!(out, b"xy");
    }

    /// Capture writer for driving a real fmt subscriber in tests.
    #[derive(Clone, Default)]
    struct Cap(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl io::Write for Cap {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Cap {
        type Writer = Cap;
        fn make_writer(&'a self) -> Cap {
            self.clone()
        }
    }

    /// The regression from 2026-07-13: tracing-subscriber's default ANSI
    /// sanitization rewrote our embedded colour escapes to literal "\x1b[38;…"
    /// text in the terminal. With sanitization off, the raw ESC must survive
    /// the fmt pipeline — and the file side must still strip it cleanly.
    #[test]
    fn embedded_ansi_survives_fmt_and_strips_for_file() {
        const MSG: &str = "tag \x1b[38;2;145;70;255m[Twitch]\x1b[0m end";
        // Terminal path: raw ESC preserved.
        let cap = Cap::default();
        let sub = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_ansi_sanitization(false)
            .with_writer(cap.clone())
            .finish();
        tracing::subscriber::with_default(sub, || tracing::info!("{MSG}"));
        let out = cap.0.lock().unwrap().clone();
        assert!(out.contains(&0x1b), "raw ESC must reach the writer: {}", String::from_utf8_lossy(&out));
        assert!(!String::from_utf8_lossy(&out).contains("\\x1b"), "must not be escaped to text");
        // File path: same event through StripAnsiMake → no ESC, text intact.
        let cap = Cap::default();
        let sub = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_ansi_sanitization(false)
            .with_writer(StripAnsiMake(cap.clone()))
            .finish();
        tracing::subscriber::with_default(sub, || tracing::info!("{MSG}"));
        let out = cap.0.lock().unwrap().clone();
        let text = String::from_utf8_lossy(&out);
        assert!(!out.contains(&0x1b), "file log must carry no ESC: {text}");
        assert!(text.contains("tag [Twitch] end"), "tag text survives: {text}");
    }

    #[test]
    fn plat_tag_plain_when_color_off() {
        set_color_enabled(false);
        assert_eq!(Platform::YouTube.tag().to_string(), "[YouTube]");
        set_color_enabled(true);
        let s = Platform::Twitch.tag().to_string();
        assert!(s.starts_with("\x1b[38;2;145;70;255m[Twitch]"));
        assert!(s.ends_with("\x1b[0m"));
        set_color_enabled(false); // restore for other tests
    }
}
