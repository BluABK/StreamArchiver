//! Browser fingerprint helpers — dynamic UA + client-hint headers derived from
//! the user's configured cookies browser (`cookies_browser` setting).
//!
//! Builds once at startup; all HTTP scrapes use it so requests look like the
//! same browser that supplies the cookies, keeping fingerprints consistent.

/// UA string + optional client-hint headers for one browser.
pub(crate) struct BrowserFingerprint {
    /// Full `User-Agent` header value.
    pub ua: String,
    /// `Sec-CH-UA` value; `None` for Firefox (doesn't send it).
    pub sec_ch_ua: Option<String>,
}

impl BrowserFingerprint {
    /// Apply YouTube page-navigation headers (Sec-Fetch-Dest: document, Sec-CH-UA,
    /// Accept, Cache-Control, etc.) to a reqwest `RequestBuilder`.
    pub fn apply_yt_nav_headers(
        &self,
        rb: reqwest::RequestBuilder,
    ) -> reqwest::RequestBuilder {
        let rb = rb
            .header(
                "Accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8",
            )
            .header("Cache-Control", "no-cache")
            .header("Pragma", "no-cache")
            .header("Upgrade-Insecure-Requests", "1")
            .header("Sec-Fetch-Dest", "document")
            .header("Sec-Fetch-Mode", "navigate")
            .header("Sec-Fetch-Site", "none")
            .header("Sec-Fetch-User", "?1");
        if let Some(ch_ua) = &self.sec_ch_ua {
            rb.header("Sec-CH-UA", ch_ua.clone())
                .header("Sec-CH-UA-Mobile", "?0")
                .header("Sec-CH-UA-Platform", "\"Windows\"")
        } else {
            rb
        }
    }

    /// Apply Kick XHR/CORS request headers (Sec-Fetch-Mode: cors, Origin, Referer).
    pub fn apply_kick_xhr_headers(
        &self,
        rb: reqwest::RequestBuilder,
    ) -> reqwest::RequestBuilder {
        let rb = rb
            .header("Cache-Control", "no-cache")
            .header("Origin", "https://kick.com")
            .header("Referer", "https://kick.com/")
            .header("Sec-Fetch-Dest", "empty")
            .header("Sec-Fetch-Mode", "cors")
            .header("Sec-Fetch-Site", "same-origin");
        if let Some(ch_ua) = &self.sec_ch_ua {
            rb.header("Sec-CH-UA", ch_ua.clone())
                .header("Sec-CH-UA-Mobile", "?0")
                .header("Sec-CH-UA-Platform", "\"Windows\"")
        } else {
            rb
        }
    }
}

const FALLBACK_CHROME_VERSION: u32 = 136;
const FALLBACK_FIREFOX_VERSION: u32 = 128;

/// Try to read the major version of the given browser from the Windows registry
/// by shelling out to `reg.exe`. Returns `None` if the key is absent (browser
/// not installed) or the query fails.
#[cfg(windows)]
fn detect_browser_version(browser: &str) -> Option<u32> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let (key, value_name) = match browser {
        "chrome" | "chromium" => (
            r"HKLM\SOFTWARE\Google\Chrome\BLBeacon",
            "version",
        ),
        "firefox" => (
            r"HKLM\SOFTWARE\Mozilla\Mozilla Firefox",
            "CurrentVersion",
        ),
        "edge" => (
            r"HKLM\SOFTWARE\Microsoft\EdgeUpdate\Clients\{56EB18F8-B008-4CBD-B6D2-8C97FE7E9062}",
            "pv",
        ),
        "brave" => (
            r"HKLM\SOFTWARE\BraveSoftware\Update\Clients\{AFE6A462-C574-4B8A-AF43-4CC60DF4563B}",
            "pv",
        ),
        "vivaldi" => (
            r"HKCU\SOFTWARE\Vivaldi",
            "Version",
        ),
        "opera" => (
            r"HKCU\SOFTWARE\Opera Software",
            "Last Version",
        ),
        _ => return None,
    };

    let output = std::process::Command::new("reg")
        .args(["query", key, "/v", value_name])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;

    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        if line.contains("REG_SZ") {
            if let Some(part) = line.split("REG_SZ").nth(1) {
                let version_str = part.trim();
                if let Some(major_str) = version_str.split('.').next() {
                    if let Ok(v) = major_str.parse::<u32>() {
                        return Some(v);
                    }
                }
            }
        }
    }
    None
}

#[cfg(not(windows))]
fn detect_browser_version(_browser: &str) -> Option<u32> {
    None
}

/// Build a `BrowserFingerprint` for the given browser name (e.g. `"chrome"`,
/// `"firefox"`, `"edge"`). Detects the installed version from the registry;
/// falls back to a baked-in recent version if detection fails or the browser
/// is not recognized.
pub(crate) fn build_browser_fingerprint(browser: &str) -> BrowserFingerprint {
    let version = detect_browser_version(browser).unwrap_or(match browser {
        "firefox" => FALLBACK_FIREFOX_VERSION,
        _ => FALLBACK_CHROME_VERSION,
    });

    match browser {
        "firefox" => BrowserFingerprint {
            ua: format!(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:{version}.0) \
                 Gecko/20100101 Firefox/{version}.0"
            ),
            sec_ch_ua: None,
        },
        "edge" => BrowserFingerprint {
            ua: format!(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                 (KHTML, like Gecko) Chrome/{version}.0.0.0 Safari/537.36 \
                 Edg/{version}.0.0.0"
            ),
            sec_ch_ua: Some(format!(
                r#""Microsoft Edge";v="{version}", "Chromium";v="{version}", "Not-A.Brand";v="99""#
            )),
        },
        "brave" => BrowserFingerprint {
            ua: format!(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                 (KHTML, like Gecko) Chrome/{version}.0.0.0 Safari/537.36"
            ),
            sec_ch_ua: Some(format!(
                r#""Brave";v="{version}", "Chromium";v="{version}", "Not-A.Brand";v="99""#
            )),
        },
        // chrome | chromium | opera | vivaldi | safari | (default Chrome-family)
        _ => BrowserFingerprint {
            ua: format!(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                 (KHTML, like Gecko) Chrome/{version}.0.0.0 Safari/537.36"
            ),
            sec_ch_ua: Some(format!(
                r#""Chromium";v="{version}", "Google Chrome";v="{version}", "Not-A.Brand";v="99""#
            )),
        },
    }
}
