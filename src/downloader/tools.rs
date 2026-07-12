//! Tool binary resolution (yt-dlp/SABR/custom tools), SABR config, and
//! auth source resolution.

use super::*;

/// Default SABR format selector when the setting is unset/empty.
pub const SABR_DEFAULT_FORMAT: &str = "ba[protocol=sabr]+bv[protocol=sabr]";
/// Default SABR `--extractor-args` when the setting is unset/empty.
pub const SABR_DEFAULT_EXTRACTOR_ARGS: &str =
    "youtube:formats=duplicate,missing_pot;player-client=web;webpage-client=web";
/// Default PO-token-provider `--extractor-args` (bgutil HTTP server on its default
/// port). Passed as a *separate* `--extractor-args` entry because it targets a
/// different extractor key (`youtubepot-bgutilhttp`) than the `youtube:` args.
/// Used when the setting key has never been written; an explicit empty value
/// disables it (rely on the plugin's own auto-detection instead).
pub const SABR_DEFAULT_POT_ARGS: &str = "youtubepot-bgutilhttp:base_url=http://127.0.0.1:4416";
/// Consecutive from-start SABR stalls ("not near live head") tolerated with
/// deep-rewind enabled before giving up and falling back to live-edge capture.
/// Deep-rewind extends the DVR window, so the *first* stall may be transient;
/// but a persistent stall repeats every attempt (each re-downloading the opening
/// — observed ~190 MiB — then dying), so we tolerate one retry then fall back.
/// With deep-rewind off a stall is a true window expiry and we fall back at once.
pub(super) const SABR_STALL_FALLBACK_TRIES: u32 = 2;
/// Default DASH-companion format selector when the setting is unset/empty.
pub const DASH_DEFAULT_FORMAT: &str = "bestvideo+bestaudio/best";

/// SABR (Server Adaptive Bit Rate) capture configuration for YouTube. SABR is the
/// only protocol that reliably supports `--live-from-start` today, but it lives in
/// a yt-dlp dev fork (a separate binary). See the YouTube SABR settings section.
#[derive(Clone, Debug, Default)]
pub struct SabrConfig {
    /// Master toggle (Settings). When false, YouTube capture-from-start uses the
    /// system binary's normal path.
    pub enabled: bool,
    /// Path to the SABR dev-build binary; empty ⇒ SABR unavailable.
    pub binary: String,
    /// Format selector injected by the preset (e.g. `ba[protocol=sabr]+bv[protocol=sabr]`).
    pub format: String,
    /// `--extractor-args` value injected by the preset.
    pub extractor_args: String,
    /// Manual raw args; when non-empty, replaces the format + extractor-args preset.
    pub raw_args: String,
    /// PO-token-provider `--extractor-args`, passed as its own `--extractor-args`
    /// entry (different extractor key than `extractor_args`). Empty ⇒ not passed.
    /// Applied regardless of the preset/`raw_args` choice (it's orthogonal to
    /// format selection).
    pub pot_args: String,
    /// GLOBAL default video codec/quality preference (a `-S` sort layered on the
    /// selector). A monitor's own pref overrides this unless it's `Inherit`.
    pub codec_pref: SabrCodecPref,
    /// GLOBAL raw `-S` string when `codec_pref == Custom`.
    pub codec_custom: String,
}

impl SabrConfig {
    /// True when SABR capture is configured and usable.
    pub(crate) fn usable(&self) -> bool {
        self.enabled && !self.binary.is_empty()
    }
}

/// A user-defined alternate yt-dlp-compatible binary (e.g. a personal fork or
/// a different dev build), selectable per-video download alongside the system
/// yt-dlp and the built-in SABR dev build. Uses the same yt-dlp argument
/// template as `Tool::YtDlp` — only the invoked program differs.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CustomTool {
    pub alias: String,
    pub path: String,
}

/// Settings key for the persisted custom-tools list (JSON-encoded
/// `Vec<CustomTool>`).
pub(super) const K_CUSTOM_TOOLS: &str = "custom_tools";

/// Reserved [`Video::tool_binary`] value selecting the built-in SABR dev build.
pub const TOOL_BINARY_SABR: &str = "sabr";

/// Load the user-defined custom tools list from settings.
pub fn load_custom_tools(store: &Store) -> Vec<CustomTool> {
    store
        .get_setting(K_CUSTOM_TOOLS)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Persist the user-defined custom tools list to settings.
pub fn save_custom_tools(store: &Store, tools: &[CustomTool]) -> anyhow::Result<()> {
    store.set_setting(K_CUSTOM_TOOLS, &serde_json::to_string(tools)?)?;
    Ok(())
}

/// The yt-dlp-family binaries available to the supervisor: the system build
/// (PATH or an explicit path), the optional SABR dev build, and any
/// user-defined custom tools.
#[derive(Clone, Debug, Default)]
pub struct YtDlpBins {
    /// Explicit system yt-dlp path; empty ⇒ `yt-dlp` on PATH.
    pub system: String,
    pub sabr: SabrConfig,
    pub custom: Vec<CustomTool>,
}

impl YtDlpBins {
    /// The program name/path for the system yt-dlp.
    pub fn system_program(&self) -> String {
        if self.system.is_empty() {
            "yt-dlp".to_string()
        } else {
            self.system.clone()
        }
    }

    /// Resolve a [`Video::tool_binary`] value to the program to invoke: empty
    /// ⇒ the system yt-dlp, [`TOOL_BINARY_SABR`] ⇒ the SABR dev build, else a
    /// custom tool's path by alias. Falls back to the system yt-dlp if the
    /// SABR build isn't configured or the alias no longer exists.
    pub fn resolve_program(&self, tool_binary: &str) -> String {
        match tool_binary {
            "" => self.system_program(),
            TOOL_BINARY_SABR => {
                if self.sabr.binary.is_empty() {
                    self.system_program()
                } else {
                    self.sabr.binary.clone()
                }
            }
            alias => self
                .custom
                .iter()
                .find(|t| t.alias == alias)
                .map(|t| t.path.clone())
                .unwrap_or_else(|| self.system_program()),
        }
    }
}

/// Read a setting as a string, defaulting to empty when absent.
pub(super) fn setting_str(store: &Store, key: &str) -> String {
    store.get_setting(key).ok().flatten().unwrap_or_default()
}

/// Load the configured yt-dlp binaries + SABR preset from settings, applying the
/// built-in fallbacks for any empty preset fields.
pub(crate) fn load_ytdlp_bins(store: &Store) -> YtDlpBins {
    let enabled = store
        .get_setting("ytdlp_sabr_enabled")
        .ok()
        .flatten()
        .map(|v| v != "0")
        .unwrap_or(true);
    let fmt = setting_str(store, "ytdlp_sabr_format");
    let xargs = setting_str(store, "ytdlp_sabr_extractor_args");
    // Experimental deep-rewind: when on, append `enable_live_deep_rewind=true` to
    // the youtube extractor-args so SABR can rewind past YouTube's normal ~4h DVR
    // window (lets capture-from-start reach the start of a long stream instead of
    // stalling with "not near live head"). Dev-build-only feature; the upstream
    // code reads only the literal lowercase `true`. Off by default — a stock
    // yt-dlp would silently ignore it, and the upstream author marks it unstable.
    let deep_rewind = store
        .get_setting("ytdlp_sabr_deep_rewind")
        .ok()
        .flatten()
        .map(|v| v == "1")
        .unwrap_or(false);
    // PO-token args: absent (never written) ⇒ the bgutil default; present (even
    // empty) ⇒ honor it verbatim, so the user can deliberately disable it.
    let pot_args = match store.get_setting("ytdlp_sabr_pot_args") {
        Ok(Some(v)) => v,
        _ => SABR_DEFAULT_POT_ARGS.to_string(),
    };
    // Global codec/quality preference. Absent/unknown ⇒ Auto (yt-dlp default),
    // preserving prior behavior. (Only the per-monitor field uses `Inherit`.)
    let codec_pref = match SabrCodecPref::parse(&setting_str(store, "ytdlp_sabr_codec_pref")) {
        SabrCodecPref::Inherit => SabrCodecPref::Auto,
        other => other,
    };
    YtDlpBins {
        system: setting_str(store, "ytdlp_binary_path"),
        sabr: SabrConfig {
            enabled,
            binary: setting_str(store, "ytdlp_sabr_binary_path"),
            format: if fmt.is_empty() { SABR_DEFAULT_FORMAT.to_string() } else { fmt },
            extractor_args: {
                let base = if xargs.is_empty() {
                    SABR_DEFAULT_EXTRACTOR_ARGS.to_string()
                } else {
                    xargs
                };
                // Append under the same `youtube:` namespace (`;`-separated).
                // Guard against a double-append if the user already added it to
                // the extractor-args field by hand.
                if deep_rewind && !base.contains("enable_live_deep_rewind") {
                    format!("{base};enable_live_deep_rewind=true")
                } else {
                    base
                }
            },
            raw_args: setting_str(store, "ytdlp_sabr_raw_args"),
            pot_args,
            codec_pref,
            codec_custom: setting_str(store, "ytdlp_sabr_codec_custom"),
        },
        custom: load_custom_tools(store),
    }
}

/// Resolve a monitor's effective SABR format-sort (`-S` value): the monitor's own
/// codec preference, or the global default when the monitor is set to `Inherit`.
/// `""` = add no `-S` (yt-dlp's default codec preference).
pub(super) fn resolve_sabr_sort(m: &Monitor, sabr: &SabrConfig) -> String {
    let (pref, custom) = if m.sabr_codec_pref == SabrCodecPref::Inherit {
        (sabr.codec_pref, sabr.codec_custom.as_str())
    } else {
        (m.sabr_codec_pref, m.sabr_codec_custom.as_str())
    };
    pref.sort_arg(custom)
}

/// Load the DASH-companion format selector (dual capture), with fallback.
pub(super) fn load_dash_format(store: &Store) -> String {
    let f = setting_str(store, "ytdlp_dash_format");
    if f.is_empty() { DASH_DEFAULT_FORMAT.to_string() } else { f }
}

/// Resolved download authentication for a monitor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthSource {
    None,
    /// yt-dlp `--cookies-from-browser <browser>`.
    CookiesBrowser(String),
    /// yt-dlp `--cookies <path>`.
    CookiesFile(String),
    /// Twitch `--twitch-api-header=Authorization=OAuth <token>` (streamlink).
    Token(String),
}

/// Resolve the effective auth for a monitor from its override + the global default.
pub fn resolve_auth(
    m: &MonitorWithChannel,
    global_method: &str,
    global_browser: &str,
) -> AuthSource {
    resolve_auth_for(
        m.monitor.auth_kind,
        &m.monitor.auth_value,
        global_method,
        global_browser,
    )
}

/// Resolve an auth source from an `(auth_kind, auth_value)` pair plus the global
/// default — shared by monitors and on-demand videos.
pub fn resolve_auth_for(
    auth_kind: AuthKind,
    auth_value: &str,
    global_method: &str,
    global_browser: &str,
) -> AuthSource {
    let val = auth_value.trim();
    let browser = global_browser.trim();
    match auth_kind {
        AuthKind::Inherit => match global_method {
            "cookies" if !browser.is_empty() => AuthSource::CookiesBrowser(browser.to_string()),
            _ => AuthSource::None,
        },
        AuthKind::Disabled => AuthSource::None,
        AuthKind::CookiesBrowser => {
            let b = if val.is_empty() { browser } else { val };
            if b.is_empty() {
                AuthSource::None
            } else {
                AuthSource::CookiesBrowser(b.to_string())
            }
        }
        AuthKind::CookiesFile if !val.is_empty() => AuthSource::CookiesFile(val.to_string()),
        AuthKind::Token if !val.is_empty() => AuthSource::Token(val.to_string()),
        _ => AuthSource::None,
    }
}


#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]

    use super::*;
    #[allow(unused_imports)]
    use crate::models::{Channel, Container, DetectionMethod, Monitor, Tool};
    #[allow(unused_imports)]
    use crate::downloader::test_util::*;

    #[test]
    fn resolve_auth_precedence() {
        // Inherit + global cookies -> browser cookies.
        let mut r = row(Tool::YtDlp, Container::Mkv, Platform::YouTube);
        assert_eq!(
            resolve_auth(&r, "cookies", "chrome"),
            AuthSource::CookiesBrowser("chrome".into())
        );
        // Per-channel override wins over global.
        r.monitor.auth_kind = AuthKind::Token;
        r.monitor.auth_value = "tok".into();
        assert_eq!(
            resolve_auth(&r, "cookies", "chrome"),
            AuthSource::Token("tok".into())
        );
        // Disabled forces none even if a global default exists.
        r.monitor.auth_kind = AuthKind::Disabled;
        assert_eq!(resolve_auth(&r, "cookies", "chrome"), AuthSource::None);
    }
    #[test]
    fn explicit_system_binary_path_is_used() {
        let bins = YtDlpBins {
            system: "C:/tools/yt-dlp.exe".into(),
            ..Default::default()
        };
        let plan = build_plan(
            &row(Tool::YtDlp, Container::Mkv, Platform::YouTube),
            1_700_000_000,
            &AuthSource::None,
            &[],
            None,
            "",
            None,
            0,
            &bins,
        );
        assert_eq!(plan.program, "C:/tools/yt-dlp.exe");
    }
    #[test]
    fn resolve_program_falls_back_to_system_for_unknown_binary() {
        let bins = YtDlpBins::default();
        // No SABR build configured and no matching custom tool -> system yt-dlp.
        assert_eq!(bins.resolve_program(TOOL_BINARY_SABR), "yt-dlp");
        assert_eq!(bins.resolve_program("no-such-alias"), "yt-dlp");
        assert_eq!(bins.resolve_program(""), "yt-dlp");
    }
}
