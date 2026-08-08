//! Sending a chat message, via Helix `POST /helix/chat/messages`.
//!
//! **Helix, not IRC, deliberately.** This app's archival chat capture
//! (`crate::chat`) logs in anonymously as `justinfan*` and never sends a byte;
//! authenticating it would mean a second connection with its own reconnect,
//! PING/PONG and JOIN lifecycle, torn down per window — a whole subsystem for
//! one text box. Helix is a stateless request that returns a structured
//! `is_sent` plus a `drop_reason`, where over IRC an AutoMod hold or a ban
//! arrives as an out-of-band NOTICE you'd have to keep a reader alive for.
//!
//! Nothing in this module has ever been run against a real channel — see the
//! note on [`send_message`].

use std::collections::VecDeque;

use serde::Deserialize;

/// Twitch's own cap for a single chat message.
pub const MAX_MESSAGE_CHARS: usize = 500;

/// Minimum gap between two sends. Twitch's own non-moderator budget is 20 per
/// 30 s; this is the "don't hammer" half of staying inside it.
pub const MIN_SEND_GAP_MS: i64 = 1_500;
/// Rolling window for [`MAX_PER_WINDOW`].
pub const SEND_WINDOW_MS: i64 = 30_000;
/// Sends allowed per [`SEND_WINDOW_MS`] — Twitch's non-moderator budget.
pub const MAX_PER_WINDOW: usize = 20;

/// Why a send was refused before it left the app.
#[derive(Clone, Debug, PartialEq)]
pub enum SendBlock {
    Empty,
    TooLong(usize),
    /// Wait this many more milliseconds.
    Cooldown(i64),
    /// Twitch silently drops a message identical to your previous one, which
    /// reads as the app being broken. Refuse it here and say why.
    DuplicateOfPrevious,
}

impl SendBlock {
    /// What to show under the input box.
    pub fn message(&self) -> String {
        match self {
            SendBlock::Empty => "Nothing to send.".into(),
            SendBlock::TooLong(n) => {
                format!("{n} characters — Twitch's limit is {MAX_MESSAGE_CHARS}.")
            }
            SendBlock::Cooldown(ms) => {
                format!("Slow down — {:.1}s.", (*ms as f64 / 1000.0).max(0.1))
            }
            SendBlock::DuplicateOfPrevious => {
                "Same as your last message — Twitch drops repeats silently.".into()
            }
        }
    }
}

/// Client-side send budget. Purely local bookkeeping, so the whole policy is
/// testable without a network or a clock.
#[derive(Default, Debug)]
pub struct SendLimiter {
    /// Send times (unix ms), oldest first, within the rolling window.
    recent: VecDeque<i64>,
    last_text: String,
}

impl SendLimiter {
    /// Whether `text` may be sent at `now_ms`, or why not.
    pub fn check(&self, text: &str, now_ms: i64) -> Result<(), SendBlock> {
        let text = text.trim();
        if text.is_empty() {
            return Err(SendBlock::Empty);
        }
        let chars = text.chars().count();
        if chars > MAX_MESSAGE_CHARS {
            return Err(SendBlock::TooLong(chars));
        }
        if text == self.last_text {
            return Err(SendBlock::DuplicateOfPrevious);
        }
        if let Some(&last) = self.recent.back() {
            let wait = MIN_SEND_GAP_MS - (now_ms - last);
            if wait > 0 {
                return Err(SendBlock::Cooldown(wait));
            }
        }
        let in_window = self.recent.iter().filter(|t| now_ms - **t < SEND_WINDOW_MS).count();
        if in_window >= MAX_PER_WINDOW {
            // Wait until the oldest send in the window ages out.
            let oldest = self.recent.iter().find(|t| now_ms - **t < SEND_WINDOW_MS).copied();
            let wait = oldest.map(|t| SEND_WINDOW_MS - (now_ms - t)).unwrap_or(SEND_WINDOW_MS);
            return Err(SendBlock::Cooldown(wait.max(1)));
        }
        Ok(())
    }

    /// Record a send that actually went out.
    pub fn record(&mut self, text: &str, now_ms: i64) {
        self.last_text = text.trim().to_string();
        self.recent.push_back(now_ms);
        while self.recent.front().is_some_and(|t| now_ms - *t >= SEND_WINDOW_MS) {
            self.recent.pop_front();
        }
    }
}

/// What Helix said about a send.
#[derive(Clone, Debug, PartialEq)]
pub enum SendOutcome {
    Sent {
        message_id: String,
    },
    /// Accepted by the API but not posted — AutoMod hold, channel restriction,
    /// and so on. `reason` is Twitch's own explanation.
    Dropped {
        code: String,
        reason: String,
    },
    /// The request itself failed (auth, rate limit, transport).
    Failed(String),
}

impl SendOutcome {
    pub fn message(&self) -> String {
        match self {
            SendOutcome::Sent { .. } => "Sent.".into(),
            SendOutcome::Dropped { code, reason } if !reason.is_empty() => {
                format!("Not posted: {reason} ({code})")
            }
            SendOutcome::Dropped { code, .. } => format!("Not posted: {code}"),
            SendOutcome::Failed(e) => format!("Send failed: {e}"),
        }
    }
    pub fn is_ok(&self) -> bool {
        matches!(self, SendOutcome::Sent { .. })
    }
}

/// The shape of a `POST /helix/chat/messages` response body.
#[derive(Deserialize)]
struct SendResponse {
    data: Vec<SendData>,
}

#[derive(Deserialize)]
struct SendData {
    #[serde(default)]
    message_id: String,
    #[serde(default)]
    is_sent: bool,
    #[serde(default)]
    drop_reason: Option<DropReason>,
}

#[derive(Deserialize)]
struct DropReason {
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
}

/// Turn a Helix status + body into an outcome. Split out from the request so
/// every branch — sent, AutoMod hold, 401, 429, malformed body — is testable
/// **without ever posting to a real channel**, which is a hard constraint on
/// this feature: no code path here has been exercised against anyone's chat.
pub fn interpret_response(status: u16, body: &str) -> SendOutcome {
    match status {
        200 => {}
        401 => {
            return SendOutcome::Failed(
                "Twitch rejected the credentials — reconnect the account in Settings → Accounts."
                    .into(),
            );
        }
        403 => {
            return SendOutcome::Failed(
                "Not allowed to post in this channel (banned, or the chat is restricted).".into(),
            );
        }
        422 => return SendOutcome::Failed("Twitch rejected the message.".into()),
        429 => return SendOutcome::Failed("Rate limited by Twitch — wait a moment.".into()),
        s => return SendOutcome::Failed(format!("Twitch returned {s}.")),
    }
    let Ok(parsed) = serde_json::from_str::<SendResponse>(body) else {
        return SendOutcome::Failed("Twitch returned an unrecognised response.".into());
    };
    let Some(d) = parsed.data.into_iter().next() else {
        return SendOutcome::Failed("Twitch returned no result.".into());
    };
    if d.is_sent {
        return SendOutcome::Sent { message_id: d.message_id };
    }
    let r = d.drop_reason.unwrap_or(DropReason { code: String::new(), message: String::new() });
    SendOutcome::Dropped {
        code: if r.code.is_empty() { "dropped".into() } else { r.code },
        reason: r.message,
    }
}

/// Post one message to a channel's chat.
///
/// **Never exercised against a real channel.** The request shape follows
/// Twitch's published documentation and every response branch is unit-tested
/// through [`interpret_response`], but no test and no probe in this repo has
/// ever posted to anyone's chat — the first real send is the user's. Treat a
/// surprise here (an unexpected `drop_reason`, a scope typo) as unproven
/// rather than impossible.
pub async fn send_message(
    http: &reqwest::Client,
    client_id: &str,
    token: &str,
    broadcaster_id: &str,
    sender_id: &str,
    text: &str,
) -> SendOutcome {
    let resp = http
        .post("https://api.twitch.tv/helix/chat/messages")
        .header("Client-Id", client_id)
        .bearer_auth(token)
        .json(&serde_json::json!({
            "broadcaster_id": broadcaster_id,
            "sender_id": sender_id,
            "message": text,
        }))
        .send()
        .await;
    match resp {
        Ok(r) => {
            let status = r.status().as_u16();
            let body = r.text().await.unwrap_or_default();
            interpret_response(status, &body)
        }
        Err(e) => SendOutcome::Failed(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_limiter_refuses_empty_overlong_and_repeated_messages() {
        let mut l = SendLimiter::default();
        assert_eq!(l.check("", 0), Err(SendBlock::Empty));
        assert_eq!(l.check("   ", 0), Err(SendBlock::Empty));

        let long: String = "a".repeat(MAX_MESSAGE_CHARS + 1);
        assert_eq!(l.check(&long, 0), Err(SendBlock::TooLong(MAX_MESSAGE_CHARS + 1)));
        assert!(l.check(&"a".repeat(MAX_MESSAGE_CHARS), 0).is_ok(), "exactly at the limit is fine");
        // Counted in characters, not bytes — a 500-emoji message is legal.
        assert!(l.check(&"あ".repeat(MAX_MESSAGE_CHARS), 0).is_ok());

        // Twitch silently drops an exact repeat, which reads as a broken app.
        l.record("hello", 0);
        assert_eq!(l.check("hello", 10_000), Err(SendBlock::DuplicateOfPrevious));
        assert_eq!(l.check("  hello  ", 10_000), Err(SendBlock::DuplicateOfPrevious));
        assert!(l.check("hello!", 10_000).is_ok());
    }

    #[test]
    fn the_limiter_spaces_sends_and_caps_the_rolling_window() {
        let mut l = SendLimiter::default();
        l.record("a", 1_000);
        // Too soon.
        match l.check("b", 1_500) {
            Err(SendBlock::Cooldown(ms)) => assert_eq!(ms, MIN_SEND_GAP_MS - 500),
            other => panic!("expected a cooldown, got {other:?}"),
        }
        assert!(l.check("b", 1_000 + MIN_SEND_GAP_MS).is_ok());

        // Sending as fast as the gap allows stays inside the budget forever:
        // 20 messages 1.5s apart span 28.5s, so by the time the 21st is due
        // the first has aged out of the 30s window. The gap IS the binding
        // constraint — the rolling cap is a backstop for the cases it can't
        // cover (a clock jump, or someone loosening the gap later).
        let mut l = SendLimiter::default();
        for i in 0..MAX_PER_WINDOW * 2 {
            let t = i as i64 * MIN_SEND_GAP_MS;
            assert!(l.check(&format!("m{i}"), t).is_ok(), "message {i} inside the budget");
            l.record(&format!("m{i}"), t);
        }

        // The backstop itself: bunch the sends up (as a backwards clock jump
        // would) and the cap refuses the one past the budget.
        let mut l = SendLimiter::default();
        for i in 0..MAX_PER_WINDOW {
            l.record(&format!("m{i}"), i as i64 * 100);
        }
        let t = MAX_PER_WINDOW as i64 * 100 + MIN_SEND_GAP_MS;
        assert!(
            matches!(l.check("one more", t), Err(SendBlock::Cooldown(_))),
            "{MAX_PER_WINDOW} sends inside {SEND_WINDOW_MS}ms is the budget"
        );
        // Once the window has fully rolled past, it's clear again.
        assert!(l.check("one more", t + SEND_WINDOW_MS).is_ok());
    }

    /// Every response branch, without ever posting to a real channel — which
    /// is the point: this feature ships unproven end-to-end by design, so the
    /// parts that CAN be pinned down are.
    #[test]
    fn helix_responses_map_to_outcomes() {
        let sent = interpret_response(
            200,
            r#"{"data":[{"message_id":"abc","is_sent":true}]}"#,
        );
        assert_eq!(sent, SendOutcome::Sent { message_id: "abc".into() });
        assert!(sent.is_ok());

        // Accepted by the API, held by AutoMod — the common case a user needs
        // to actually see, rather than wondering why nothing appeared.
        let held = interpret_response(
            200,
            r#"{"data":[{"message_id":"abc","is_sent":false,
                 "drop_reason":{"code":"automod_held","message":"held by AutoMod"}}]}"#,
        );
        assert_eq!(
            held,
            SendOutcome::Dropped { code: "automod_held".into(), reason: "held by AutoMod".into() }
        );
        assert!(!held.is_ok());
        assert!(held.message().contains("held by AutoMod"));

        // Not sent, no reason given.
        assert_eq!(
            interpret_response(200, r#"{"data":[{"is_sent":false}]}"#),
            SendOutcome::Dropped { code: "dropped".into(), reason: String::new() }
        );

        for (status, needle) in [
            (401u16, "reconnect"),
            (403, "banned"),
            (429, "Rate limited"),
            (500, "500"),
        ] {
            let o = interpret_response(status, "");
            assert!(!o.is_ok(), "{status}");
            assert!(o.message().contains(needle), "{status}: {}", o.message());
        }

        // Malformed / empty bodies must not panic.
        assert!(!interpret_response(200, "not json").is_ok());
        assert!(!interpret_response(200, r#"{"data":[]}"#).is_ok());
    }
}
