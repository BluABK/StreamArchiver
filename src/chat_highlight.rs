//! Chat highlights and mentions: which messages should light up in the replay,
//! and which should raise a notification.
//!
//! Matching runs in the **live IRC client** (`crate::chat`), not the chat
//! window. The window is a file-tail replay that may not even be open, and the
//! whole point of "pingable" is being told while you're doing something else;
//! doing it at the source also means it works for chat-only sessions, which
//! run with no recording at all.
//!
//! The chat window reads the same rules to decide which rows to accent, so
//! there is one definition of "this message matters", not two.

use serde::{Deserialize, Serialize};

/// Whether a message naming you raises a desktop toast and a 🔔 feed row.
/// Default **off** — this is the only setting in the app that can make an
/// unattended machine start talking to you.
pub const K_PINGABLE: &str = "chat_pingable";
/// The custom highlight rules, as a JSON array of [`HighlightRule`].
pub const K_HIGHLIGHTS: &str = "chat_highlight_rules";

/// At most one mention toast per channel per this many seconds. A chat that
/// decides to spam your name would otherwise spawn a toast per message; the
/// suppressed ones still land in the 🔔 feed, so nothing is lost, only the
/// interruption.
pub const MENTION_TOAST_COOLDOWN_SECS: i64 = 10;

fn d_true() -> bool {
    true
}

/// One custom highlight: a word, a phrase, or a regex.
///
/// Deliberately the same shape as [`crate::triggers::TriggerRule`]'s matching
/// half (label + regex flag + pattern) so the two read alike in the UI, but a
/// separate type: a trigger decides whether to *record a broadcast* and
/// carries a dozen fields about how, none of which mean anything here.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HighlightRule {
    #[serde(default = "d_true")]
    pub enabled: bool,
    /// Optional human-readable name, so a long regex isn't the only thing
    /// identifying the rule in the list or in a notification.
    #[serde(default)]
    pub label: String,
    /// `false` = case-insensitive substring; `true` = regex (case-insensitive
    /// unless the pattern opts out with `(?-i)`).
    #[serde(default)]
    pub regex: bool,
    pub pattern: String,
    /// Match only on whole words, so `art` doesn't fire on "start". Ignored
    /// for regex rules, which can express that themselves with `\b`.
    #[serde(default)]
    pub whole_word: bool,
    /// Raise a notification, not just a highlighted row. Off by default: most
    /// people want a handful of words to stand out and only their own name to
    /// interrupt them.
    #[serde(default)]
    pub notify: bool,
}

impl Default for HighlightRule {
    fn default() -> Self {
        HighlightRule {
            enabled: true,
            label: String::new(),
            regex: false,
            pattern: String::new(),
            whole_word: false,
            notify: false,
        }
    }
}

impl HighlightRule {
    /// How this rule reads in a notification or the rule list — its label if
    /// it has one, else the pattern itself.
    pub fn describe(&self) -> String {
        if !self.label.trim().is_empty() {
            return self.label.trim().to_string();
        }
        if self.regex {
            format!("/{}/", self.pattern.trim())
        } else {
            format!("\"{}\"", self.pattern.trim())
        }
    }
}

/// Validate a rule's pattern for the editor: `None` = fine, `Some(err)` = the
/// regex failed to compile. Mirrors `triggers::pattern_error`.
pub fn pattern_error(rule: &HighlightRule) -> Option<String> {
    if !rule.regex {
        return None;
    }
    regex_lite::Regex::new(&format!("(?i){}", rule.pattern.trim())).err().map(|e| e.to_string())
}

/// Whether `rule` hits `text`. An invalid regex never matches (the editor
/// flags it), and an empty pattern never matches — a blank row in the list
/// must not silently highlight every message.
pub fn rule_hits(rule: &HighlightRule, text: &str) -> bool {
    let pat = rule.pattern.trim();
    if !rule.enabled || pat.is_empty() {
        return false;
    }
    if rule.regex {
        return regex_lite::Regex::new(&format!("(?i){pat}"))
            .map(|re| re.is_match(text))
            .unwrap_or(false);
    }
    let (hay, needle) = (text.to_lowercase(), pat.to_lowercase());
    if !rule.whole_word {
        return hay.contains(&needle);
    }
    word_contains(&hay, &needle)
}

/// Substring search that only accepts matches on word boundaries, so `art`
/// doesn't fire on "start". Both arguments must already be lowercased.
///
/// "Word character" here means alphanumeric or `_`, matched per Unicode
/// `char::is_alphanumeric` rather than ASCII — chat is full of non-Latin
/// text, and an ASCII-only boundary would treat every CJK character as a
/// separator and fire on any substring of a Japanese sentence.
fn word_contains(hay: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let bytes_before = |i: usize| hay[..i].chars().next_back();
    let mut from = 0;
    while let Some(rel) = hay[from..].find(needle) {
        let at = from + rel;
        let end = at + needle.len();
        let left_ok = bytes_before(at).is_none_or(|c| !is_word(c));
        let right_ok = hay[end..].chars().next().is_none_or(|c| !is_word(c));
        if left_ok && right_ok {
            return true;
        }
        // Advance past this occurrence's first char, not past the whole
        // match: overlapping occurrences can differ in boundary-ness.
        from = at + hay[at..].chars().next().map(char::len_utf8).unwrap_or(1);
    }
    false
}

/// Does `text` name `login`? Matches `@name` and a bare mention on a word
/// boundary, case-insensitively.
///
/// Bare mentions count because that is how people actually address each other
/// in chat, and a "pingable" that only fired on `@` would miss most of what
/// the user is asking to be told about.
pub fn mentions_login(text: &str, login: &str) -> bool {
    let login = login.trim().to_lowercase();
    if login.is_empty() {
        return false;
    }
    word_contains(&text.to_lowercase(), &login)
}

/// Why a message is highlighted, if it is.
#[derive(Clone, Debug, PartialEq)]
pub enum HighlightHit {
    /// The message names the connected account.
    Mention,
    /// A custom rule matched; carries the rule's own description and whether
    /// it asked to notify.
    Rule { label: String, notify: bool },
}

impl HighlightHit {
    /// Whether this hit should raise a notification, as opposed to only
    /// accenting the row.
    pub fn notifies(&self) -> bool {
        match self {
            HighlightHit::Mention => true,
            HighlightHit::Rule { notify, .. } => *notify,
        }
    }
}

/// First reason `text` is highlighted, or `None`.
///
/// A mention of your own login outranks the custom rules: it is the more
/// specific fact and the one you are most likely to want named in the toast.
/// `login` empty (no connected account) simply skips the mention check.
pub fn first_hit(text: &str, login: &str, rules: &[HighlightRule]) -> Option<HighlightHit> {
    if mentions_login(text, login) {
        return Some(HighlightHit::Mention);
    }
    rules
        .iter()
        .find(|r| rule_hits(r, text))
        .map(|r| HighlightHit::Rule { label: r.describe(), notify: r.notify })
}

/// Whether one live chat message should raise a notification, and why.
///
/// Every rule about *interrupting someone* lives here, in one pure function,
/// rather than scattered through the IRC read loop:
///
/// - your own messages never ping you;
/// - a hit that didn't ask to notify only accents the row;
/// - and at most one toast per channel per [`MENTION_TOAST_COOLDOWN_SECS`],
///   because a chat that decides to spam your name would otherwise spawn one
///   per message. Suppressed hits still reach the 🔔 feed via the row accent —
///   nothing is lost, only the interruption.
///
/// `my_login` empty (no connected account, or "pingable" off) disables the
/// mention half; rules that opted in still fire.
pub fn notify_reason(
    text: &str,
    author_login: &str,
    my_login: &str,
    rules: &[HighlightRule],
    now: i64,
    last_toast: i64,
) -> Option<String> {
    if !my_login.is_empty() && author_login.eq_ignore_ascii_case(my_login) {
        return None;
    }
    let hit = first_hit(text, my_login, rules)?;
    if !hit.notifies() || now - last_toast < MENTION_TOAST_COOLDOWN_SECS {
        return None;
    }
    Some(match hit {
        HighlightHit::Mention => "mentioned you".to_string(),
        HighlightHit::Rule { label, .. } => format!("matched {label}"),
    })
}

/// Load the stored rules. A malformed blob yields an empty list rather than
/// an error — highlights are decoration, and failing to load them must never
/// take the chat logger down with it.
pub fn load_rules(store: &crate::store::Store) -> Vec<HighlightRule> {
    store
        .get_setting(K_HIGHLIGHTS)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .and_then(|s| serde_json::from_str::<Vec<HighlightRule>>(&s).ok())
        .unwrap_or_default()
}

pub fn save_rules(store: &crate::store::Store, rules: &[HighlightRule]) {
    match serde_json::to_string(rules) {
        Ok(json) => {
            let _ = store.set_setting(K_HIGHLIGHTS, &json);
        }
        Err(e) => tracing::warn!("chat highlights: failed to serialize rules: {e:#}"),
    }
}

/// Whether mention notifications are on. Default off.
pub fn pingable(store: &crate::store::Store) -> bool {
    store.get_setting(K_PINGABLE).ok().flatten().as_deref() == Some("1")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(pattern: &str) -> HighlightRule {
        HighlightRule { pattern: pattern.into(), ..Default::default() }
    }

    #[test]
    fn substring_rules_are_case_insensitive() {
        assert!(rule_hits(&rule("karaoke"), "late night KARAOKE stream"));
        assert!(rule_hits(&rule("KARAOKE"), "karaoke"));
        assert!(!rule_hits(&rule("karaoke"), "just chatting"));
    }

    /// The whole point of the whole-word option: `art` must not fire on
    /// "start", but must still fire on "art!" and "the art".
    #[test]
    fn whole_word_rules_respect_boundaries() {
        let r = HighlightRule { whole_word: true, ..rule("art") };
        assert!(!rule_hits(&r, "let's start"));
        assert!(!rule_hits(&r, "smart"));
        assert!(rule_hits(&r, "nice art!"));
        assert!(rule_hits(&r, "art"));
        assert!(rule_hits(&r, "(art)"));
        // Without the flag the substring wins, which is the default.
        assert!(rule_hits(&rule("art"), "let's start"));
    }

    /// Chat is full of CJK. An ASCII-only boundary check would treat every
    /// Japanese character as a separator and fire on any substring.
    #[test]
    fn whole_word_boundaries_understand_non_latin_text() {
        let r = HighlightRule { whole_word: true, ..rule("かわ") };
        assert!(!rule_hits(&r, "かわいい"), "not a whole word");
        assert!(rule_hits(&r, "かわ です"));
    }

    #[test]
    fn regex_rules_compile_and_invalid_ones_never_match() {
        let r = HighlightRule { regex: true, ..rule(r"\bgiveaway\b") };
        assert!(rule_hits(&r, "big giveaway today"));
        assert!(!rule_hits(&r, "giveaways"));
        assert!(pattern_error(&r).is_none());

        let bad = HighlightRule { regex: true, ..rule("(unclosed") };
        assert!(!rule_hits(&bad, "(unclosed"), "an invalid regex must not match anything");
        assert!(pattern_error(&bad).is_some());
    }

    #[test]
    fn a_disabled_or_blank_rule_never_matches() {
        assert!(!rule_hits(&HighlightRule { enabled: false, ..rule("karaoke") }, "karaoke"));
        // A blank row in the editor must not silently highlight everything.
        assert!(!rule_hits(&rule(""), "anything at all"));
        assert!(!rule_hits(&rule("   "), "anything at all"));
    }

    /// Bare mentions count: that's how people actually address each other,
    /// and an `@`-only ping would miss most of what the user asked to hear
    /// about. But a name inside a longer word is not a mention.
    #[test]
    fn mentions_match_at_and_bare_names_on_word_boundaries() {
        assert!(mentions_login("@bluabk have you seen this", "bluabk"));
        assert!(mentions_login("bluabk what do you think", "BluABK"));
        assert!(mentions_login("ask BluABK!", "bluabk"));
        assert!(!mentions_login("bluabking around", "bluabk"));
        assert!(!mentions_login("nothing to see", "bluabk"));
        // No connected account: nothing to mention.
        assert!(!mentions_login("@bluabk hi", ""));
    }

    /// A mention outranks a custom rule — it's the more specific fact and the
    /// one worth naming in a toast.
    #[test]
    fn a_mention_outranks_a_custom_rule() {
        let rules = vec![HighlightRule { notify: true, ..rule("hello") }];
        assert_eq!(first_hit("hello bluabk", "bluabk", &rules), Some(HighlightHit::Mention));
        assert_eq!(
            first_hit("hello there", "bluabk", &rules),
            Some(HighlightHit::Rule { label: "\"hello\"".into(), notify: true })
        );
        assert_eq!(first_hit("nothing", "bluabk", &rules), None);
    }

    /// Only a mention notifies by default; a plain rule highlights the row
    /// and stays quiet unless it opted in.
    #[test]
    fn only_mentions_and_opted_in_rules_notify() {
        assert!(HighlightHit::Mention.notifies());
        assert!(HighlightHit::Rule { label: "x".into(), notify: true }.notifies());
        assert!(!HighlightHit::Rule { label: "x".into(), notify: false }.notifies());
    }

    /// Everything about whether a message interrupts you, in one place.
    #[test]
    fn notify_reason_covers_self_suppression_gating_and_cooldown() {
        let quiet = vec![HighlightRule { notify: false, ..rule("karaoke") }];
        let loud = vec![HighlightRule { notify: true, label: "Karaoke".into(), ..rule("karaoke") }];

        // A mention pings, and names the reason.
        assert_eq!(
            notify_reason("hey bluabk", "someone", "bluabk", &[], 100, 0).as_deref(),
            Some("mentioned you")
        );

        // Your own message never pings you, however it matched.
        assert_eq!(notify_reason("hey bluabk", "BluABK", "bluabk", &[], 100, 0), None);
        assert_eq!(notify_reason("karaoke!", "bluabk", "bluabk", &loud, 100, 0), None);

        // A rule that didn't ask to notify only accents the row.
        assert_eq!(notify_reason("karaoke!", "someone", "bluabk", &quiet, 100, 0), None);
        assert_eq!(
            notify_reason("karaoke!", "someone", "bluabk", &loud, 100, 0).as_deref(),
            Some("matched Karaoke")
        );

        // Cooldown: a chat spamming your name gets one toast, not fifty.
        assert_eq!(notify_reason("hey bluabk", "a", "bluabk", &[], 100, 95), None);
        assert!(
            notify_reason("hey bluabk", "a", "bluabk", &[], 100 + MENTION_TOAST_COOLDOWN_SECS, 100)
                .is_some()
        );

        // "Pingable" off (empty login) silences mentions but not opted-in rules.
        assert_eq!(notify_reason("hey bluabk", "a", "", &[], 100, 0), None);
        assert!(notify_reason("karaoke!", "a", "", &loud, 100, 0).is_some());

        // Nothing matched at all.
        assert_eq!(notify_reason("just chatting", "a", "bluabk", &loud, 100, 0), None);
    }

    #[test]
    fn a_rule_describes_itself_by_label_then_pattern() {
        assert_eq!(HighlightRule { label: "Giveaways".into(), ..rule("give") }.describe(), "Giveaways");
        assert_eq!(rule("give").describe(), "\"give\"");
        assert_eq!(HighlightRule { regex: true, ..rule(r"\bgive\b") }.describe(), r"/\bgive\b/");
    }
}
