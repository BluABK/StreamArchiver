//! Trigger words — force-start a recording when the live stream's title or
//! game/category matches a configured rule, **even when Auto-record is off**.
//!
//! Streams titled "unarchived", "karaoke" etc. usually have no VOD (or get it
//! muted), so they must be captured live. Rules are structured — each one is
//! *(field, match-type, pattern, per-rule action overrides)* — and resolve
//! through the same **three-level inheritance chain as the VOD-archive scopes**
//! ([`crate::vod_archive`]): global list < per-channel scope < per-instance
//! scope, all stored as JSON in `app_settings` (no schema migration).
//!
//! Unlike the boolean scopes, a *list* override needs more than inherit/on/off,
//! so each scope level carries a [`TriggerMode`]: `Inherit` the level above,
//! `Extend` it with additional rules, `Replace` it entirely, or `Off` (no
//! triggers at this level, suppressing inherited ones).
//!
//! Matching happens in the downloader's `try_begin` gate on every live poll
//! (the scheduler keeps polling Auto-off monitors), so both go-live titles and
//! mid-stream title/category flips are seen.
//!
//! **Blacklist triggers** are the exact inverse: a title/game match VETOES any
//! automatic start (Auto-record and trigger-word matches alike) — only an
//! explicit user ▶ Start records. They reuse the same rule/scope structures
//! and three-level resolution under their own settings keys; the per-rule
//! action overrides (`capture_from_start`, `stop_on_unmatch`, `lead_secs`,
//! `end_delay_secs`) are meaningless for a veto and ignored.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::store::Store;

// ---------- settings keys ----------

/// Global rule list (JSON `Vec<TriggerRule>`).
pub const K_TRIGGER_RULES: &str = "trigger_rules";
/// Per-channel scope map (`{channel_id -> TriggerScope}`).
pub const K_CHANNEL_TRIGGER_SCOPE: &str = "channel_trigger_scope";
/// Per-monitor scope map (`{monitor_id -> TriggerScope}`).
pub const K_MONITOR_TRIGGER_SCOPE: &str = "monitor_trigger_scope";
/// Global blacklist rule list (JSON `Vec<TriggerRule>`).
pub const K_BLOCK_RULES: &str = "trigger_block_rules";
/// Per-channel blacklist scope map (`{channel_id -> TriggerScope}`).
pub const K_CHANNEL_BLOCK_SCOPE: &str = "channel_trigger_block_scope";
/// Per-monitor blacklist scope map (`{monitor_id -> TriggerScope}`).
pub const K_MONITOR_BLOCK_SCOPE: &str = "monitor_trigger_block_scope";

// ---------- rule model ----------

/// Which stream field a rule matches against.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TriggerField {
    /// Title OR game/category.
    #[default]
    Any,
    Title,
    Game,
}

impl TriggerField {
    pub const ALL: [TriggerField; 3] = [TriggerField::Any, TriggerField::Title, TriggerField::Game];

    pub fn label(self) -> &'static str {
        match self {
            TriggerField::Any => "Any field",
            TriggerField::Title => "Title",
            TriggerField::Game => "Game",
        }
    }
}

fn d_true() -> bool {
    true
}

/// One trigger rule. Every field is `serde(default)` so future per-rule action
/// overrides can be added without breaking stored JSON.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TriggerRule {
    /// Per-rule kill switch (kept in the list but ignored when false).
    #[serde(default = "d_true")]
    pub enabled: bool,
    #[serde(default)]
    pub field: TriggerField,
    /// `false` = case-insensitive substring; `true` = regex (case-insensitive
    /// by default — prepend your own `(?-i)` inside the pattern to opt out).
    #[serde(default)]
    pub regex: bool,
    pub pattern: String,
    /// Per-rule override of the monitor's "capture from start" flag for the
    /// recording this rule starts. `None` = keep the monitor's setting.
    #[serde(default)]
    pub capture_from_start: Option<bool>,
    /// Stop the recording once this rule no longer matches (after
    /// `end_delay_secs`), instead of recording until the stream itself ends —
    /// e.g. archiving just one game segment of a multi-day marathon. `false`
    /// (default) keeps today's "record until the stream ends" behavior.
    #[serde(default)]
    pub stop_on_unmatch: bool,
    /// Backfill this many seconds of Twitch CDN footage from *before* the
    /// match was detected, in case the title/game update landed a little
    /// late relative to when the segment actually started. `0` = off.
    /// Reuses the head-backfill mechanism, so it's Twitch-only.
    #[serde(default)]
    pub lead_secs: i64,
    /// When `stop_on_unmatch` is on, keep recording this many seconds after
    /// the rule stops matching before actually stopping — a grace period in
    /// case the title/game flips back (or the update landed a little early).
    /// `0` = stop as soon as an unmatch is confirmed. Ignored when
    /// `stop_on_unmatch` is false.
    #[serde(default)]
    pub end_delay_secs: i64,
}

impl Default for TriggerRule {
    fn default() -> Self {
        TriggerRule {
            enabled: true,
            field: TriggerField::Any,
            regex: false,
            pattern: String::new(),
            capture_from_start: None,
            stop_on_unmatch: false,
            lead_secs: 0,
            end_delay_secs: 0,
        }
    }
}

/// How a channel/instance scope combines with the level above it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TriggerMode {
    /// Use the inherited rules as-is.
    #[default]
    Inherit,
    /// Inherited rules PLUS this scope's own rules.
    Extend,
    /// Only this scope's own rules.
    Replace,
    /// No trigger rules at all for this channel/instance.
    Off,
}

/// A channel- or monitor-level trigger override.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TriggerScope {
    #[serde(default)]
    pub mode: TriggerMode,
    #[serde(default)]
    pub rules: Vec<TriggerRule>,
}

impl TriggerScope {
    /// True when this scope changes nothing — persisted as a removal so the
    /// map only holds real overrides.
    pub fn is_inherit(&self) -> bool {
        self.mode == TriggerMode::Inherit && self.rules.is_empty()
    }

    /// Combine the inherited rule list with this scope.
    fn apply(&self, inherited: Vec<TriggerRule>) -> Vec<TriggerRule> {
        match self.mode {
            TriggerMode::Inherit => inherited,
            TriggerMode::Extend => {
                let mut out = inherited;
                out.extend(self.rules.iter().cloned());
                out
            }
            TriggerMode::Replace => self.rules.clone(),
            TriggerMode::Off => Vec::new(),
        }
    }
}

// ---------- persistence (clone of the vod_archive scope-map pattern) ----------

/// The global rule list.
pub fn load_global_rules(store: &Store) -> Vec<TriggerRule> {
    store
        .get_setting(K_TRIGGER_RULES)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_global_rules(store: &Store, rules: &[TriggerRule]) -> anyhow::Result<()> {
    store.set_setting(K_TRIGGER_RULES, &serde_json::to_string(rules)?)?;
    Ok(())
}

fn load_scope_map(store: &Store, key: &str) -> HashMap<String, TriggerScope> {
    store
        .get_setting(key)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_scope(store: &Store, key: &str, id: i64, cfg: &TriggerScope) -> anyhow::Result<()> {
    let mut map = load_scope_map(store, key);
    if cfg.is_inherit() {
        map.remove(&id.to_string());
    } else {
        map.insert(id.to_string(), cfg.clone());
    }
    store.set_setting(key, &serde_json::to_string(&map)?)?;
    Ok(())
}

pub fn load_channel_trigger_scope(store: &Store, channel_id: i64) -> TriggerScope {
    load_scope_map(store, K_CHANNEL_TRIGGER_SCOPE)
        .remove(&channel_id.to_string())
        .unwrap_or_default()
}

pub fn save_channel_trigger_scope(
    store: &Store,
    channel_id: i64,
    cfg: &TriggerScope,
) -> anyhow::Result<()> {
    save_scope(store, K_CHANNEL_TRIGGER_SCOPE, channel_id, cfg)
}

pub fn load_monitor_trigger_scope(store: &Store, monitor_id: i64) -> TriggerScope {
    load_scope_map(store, K_MONITOR_TRIGGER_SCOPE)
        .remove(&monitor_id.to_string())
        .unwrap_or_default()
}

pub fn save_monitor_trigger_scope(
    store: &Store,
    monitor_id: i64,
    cfg: &TriggerScope,
) -> anyhow::Result<()> {
    save_scope(store, K_MONITOR_TRIGGER_SCOPE, monitor_id, cfg)
}

// ---------- blacklist persistence (same shapes, own keys) ----------

/// The global blacklist rule list.
pub fn load_global_block_rules(store: &Store) -> Vec<TriggerRule> {
    store
        .get_setting(K_BLOCK_RULES)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_global_block_rules(store: &Store, rules: &[TriggerRule]) -> anyhow::Result<()> {
    store.set_setting(K_BLOCK_RULES, &serde_json::to_string(rules)?)?;
    Ok(())
}

pub fn load_channel_block_scope(store: &Store, channel_id: i64) -> TriggerScope {
    load_scope_map(store, K_CHANNEL_BLOCK_SCOPE)
        .remove(&channel_id.to_string())
        .unwrap_or_default()
}

pub fn save_channel_block_scope(
    store: &Store,
    channel_id: i64,
    cfg: &TriggerScope,
) -> anyhow::Result<()> {
    save_scope(store, K_CHANNEL_BLOCK_SCOPE, channel_id, cfg)
}

pub fn load_monitor_block_scope(store: &Store, monitor_id: i64) -> TriggerScope {
    load_scope_map(store, K_MONITOR_BLOCK_SCOPE)
        .remove(&monitor_id.to_string())
        .unwrap_or_default()
}

pub fn save_monitor_block_scope(
    store: &Store,
    monitor_id: i64,
    cfg: &TriggerScope,
) -> anyhow::Result<()> {
    save_scope(store, K_MONITOR_BLOCK_SCOPE, monitor_id, cfg)
}

// ---------- resolution ----------

/// Pure resolver: global → channel scope → monitor scope, then drop disabled /
/// empty-pattern rules.
pub fn effective_rules_from(
    global: Vec<TriggerRule>,
    channel_scope: &TriggerScope,
    monitor_scope: &TriggerScope,
) -> Vec<TriggerRule> {
    monitor_scope
        .apply(channel_scope.apply(global))
        .into_iter()
        .filter(|r| r.enabled && !r.pattern.trim().is_empty())
        .collect()
}

/// Store-hitting resolver for one channel+monitor pair.
pub fn effective_rules(store: &Store, channel_id: i64, monitor_id: i64) -> Vec<TriggerRule> {
    effective_rules_from(
        load_global_rules(store),
        &load_channel_trigger_scope(store, channel_id),
        &load_monitor_trigger_scope(store, monitor_id),
    )
}

/// Store-hitting BLACKLIST resolver for one channel+monitor pair — the same
/// three-level resolution over the blacklist keys.
pub fn effective_block_rules(store: &Store, channel_id: i64, monitor_id: i64) -> Vec<TriggerRule> {
    effective_rules_from(
        load_global_block_rules(store),
        &load_channel_block_scope(store, channel_id),
        &load_monitor_block_scope(store, monitor_id),
    )
}

// ---------- matching ----------

/// A successful rule match: the rule plus what it hit.
#[derive(Clone, Debug, PartialEq)]
pub struct TriggerHit {
    pub rule: TriggerRule,
    /// Which concrete field matched ("title" or "game").
    pub field: &'static str,
    /// The full field value that matched (for the notification body).
    pub matched: String,
}

impl TriggerHit {
    /// Short human description, e.g. `title ~ "karaoke"` — stored on the
    /// recording row and shown in notifications / badge tooltips.
    pub fn describe(&self) -> String {
        let mut s = format!(
            "{} ~ {}{}{}",
            self.field,
            if self.rule.regex { "/" } else { "\"" },
            self.rule.pattern,
            if self.rule.regex { "/" } else { "\"" },
        );
        match self.rule.capture_from_start {
            Some(true) => s.push_str(" · capture-from-start forced on"),
            Some(false) => s.push_str(" · capture-from-start forced off"),
            None => {}
        }
        if self.rule.lead_secs > 0 {
            s.push_str(&format!(" · lead {}s", self.rule.lead_secs));
        }
        if self.rule.stop_on_unmatch {
            if self.rule.end_delay_secs > 0 {
                s.push_str(&format!(" · stops when unmatched (+{}s)", self.rule.end_delay_secs));
            } else {
                s.push_str(" · stops when unmatched");
            }
        }
        s
    }
}

/// Whether `pattern` (with the rule's match type) hits `value`. Invalid regexes
/// never match (the UI flags them at edit time).
fn pattern_matches(rule: &TriggerRule, value: &str) -> bool {
    let pat = rule.pattern.trim();
    if pat.is_empty() {
        return false;
    }
    if rule.regex {
        regex_lite::Regex::new(&format!("(?i){pat}"))
            .map(|re| re.is_match(value))
            .unwrap_or(false)
    } else {
        value.to_lowercase().contains(&pat.to_lowercase())
    }
}

/// First rule (in order) that matches the title/game, or `None`.
pub fn first_match(
    rules: &[TriggerRule],
    title: Option<&str>,
    game: Option<&str>,
) -> Option<TriggerHit> {
    for rule in rules {
        if !rule.enabled {
            continue;
        }
        let candidates: [(&'static str, Option<&str>); 2] = [("title", title), ("game", game)];
        for (name, value) in candidates {
            let field_ok = match rule.field {
                TriggerField::Any => true,
                TriggerField::Title => name == "title",
                TriggerField::Game => name == "game",
            };
            if !field_ok {
                continue;
            }
            if let Some(v) = value
                && pattern_matches(rule, v)
            {
                return Some(TriggerHit {
                    rule: rule.clone(),
                    field: name,
                    matched: v.to_string(),
                });
            }
        }
    }
    None
}

/// Validate a rule's pattern for the editor: `None` = fine, `Some(err)` = the
/// regex failed to compile.
pub fn pattern_error(rule: &TriggerRule) -> Option<String> {
    if !rule.regex {
        return None;
    }
    regex_lite::Regex::new(&format!("(?i){}", rule.pattern.trim()))
        .err()
        .map(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(field: TriggerField, regex: bool, pattern: &str) -> TriggerRule {
        TriggerRule {
            field,
            regex,
            pattern: pattern.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn substring_matching_is_case_insensitive_and_field_scoped() {
        let rules = vec![rule(TriggerField::Title, false, "karaoke")];
        // Case-insensitive substring on the title.
        let hit = first_match(&rules, Some("UNARCHIVED KARAOKE NIGHT!!"), None).unwrap();
        assert_eq!(hit.field, "title");
        assert_eq!(hit.matched, "UNARCHIVED KARAOKE NIGHT!!");
        // Title-scoped rule must NOT hit the game field.
        assert!(first_match(&rules, None, Some("Karaoke")).is_none());
        // Any-field rule hits the game too.
        let any = vec![rule(TriggerField::Any, false, "just chatting")];
        assert_eq!(
            first_match(&any, Some("morning"), Some("Just Chatting")).unwrap().field,
            "game"
        );
        // Phrases match as substrings.
        let phrase = vec![rule(TriggerField::Title, false, "no vod")];
        assert!(first_match(&phrase, Some("chill stream (NO VOD)"), None).is_some());
        // Disabled rules never fire.
        let mut off = rule(TriggerField::Any, false, "karaoke");
        off.enabled = false;
        assert!(first_match(&[off], Some("karaoke"), None).is_none());
    }

    #[test]
    fn regex_matching_and_invalid_patterns() {
        let rules = vec![rule(TriggerField::Title, true, r"unarchi(v|ve)d")];
        assert!(first_match(&rules, Some("UNARCHIVED singing"), None).is_some());
        assert!(first_match(&rules, Some("archived rerun"), None).is_none());
        // Invalid regex: never matches, and pattern_error reports it.
        let bad = rule(TriggerField::Any, true, r"un[closed");
        assert!(first_match(&[bad.clone()], Some("un[closed"), None).is_none());
        assert!(pattern_error(&bad).is_some());
        assert!(pattern_error(&rules[0]).is_none());
        // describe() renders regex with slashes and notes the override.
        let mut r = rule(TriggerField::Title, true, "karaoke");
        r.capture_from_start = Some(true);
        let hit = first_match(&[r], Some("karaoke"), None).unwrap();
        assert_eq!(hit.describe(), "title ~ /karaoke/ · capture-from-start forced on");
    }

    #[test]
    fn describe_notes_lead_and_stop_on_unmatch() {
        let mut r = rule(TriggerField::Title, false, "gdq segment");
        r.lead_secs = 30;
        let hit = first_match(&[r], Some("gdq segment"), None).unwrap();
        assert_eq!(hit.describe(), "title ~ \"gdq segment\" · lead 30s");

        let mut r2 = rule(TriggerField::Title, false, "gdq segment");
        r2.stop_on_unmatch = true;
        r2.end_delay_secs = 15;
        let hit2 = first_match(&[r2], Some("gdq segment"), None).unwrap();
        assert_eq!(hit2.describe(), "title ~ \"gdq segment\" · stops when unmatched (+15s)");

        // stop_on_unmatch with no end delay omits the "+Ns" part.
        let mut r3 = rule(TriggerField::Title, false, "gdq segment");
        r3.stop_on_unmatch = true;
        let hit3 = first_match(&[r3], Some("gdq segment"), None).unwrap();
        assert_eq!(hit3.describe(), "title ~ \"gdq segment\" · stops when unmatched");
    }

    #[test]
    fn three_level_resolution_modes() {
        let global = vec![rule(TriggerField::Any, false, "unarchived")];
        let extra = rule(TriggerField::Title, false, "karaoke");
        let inherit = TriggerScope::default();
        let extend = TriggerScope { mode: TriggerMode::Extend, rules: vec![extra.clone()] };
        let replace = TriggerScope { mode: TriggerMode::Replace, rules: vec![extra.clone()] };
        let off = TriggerScope { mode: TriggerMode::Off, rules: vec![] };

        // Inherit at both levels → global only.
        let r = effective_rules_from(global.clone(), &inherit, &inherit);
        assert_eq!(r.len(), 1);
        // Channel extends → both; instance replace wins over channel.
        let r = effective_rules_from(global.clone(), &extend, &inherit);
        assert_eq!(r.len(), 2);
        let r = effective_rules_from(global.clone(), &extend, &replace);
        assert_eq!(r, vec![extra.clone()]);
        // Off at channel level suppresses global; instance can extend from nothing.
        let r = effective_rules_from(global.clone(), &off, &inherit);
        assert!(r.is_empty());
        let r = effective_rules_from(global.clone(), &off, &extend);
        assert_eq!(r, vec![extra.clone()]);
        // Instance Off silences everything.
        let r = effective_rules_from(global, &extend, &off);
        assert!(r.is_empty());
        // Disabled / blank-pattern rules are filtered out.
        let mut blank = extra.clone();
        blank.pattern = "   ".into();
        let r = effective_rules_from(vec![blank], &inherit, &inherit);
        assert!(r.is_empty());
    }

    #[test]
    fn blacklist_rules_persist_under_their_own_keys() {
        let store = Store::open_in_memory().unwrap();
        let block = vec![rule(TriggerField::Title, false, "rerun")];
        save_global_block_rules(&store, &block).unwrap();
        // Blacklist storage is fully separate from the whitelist…
        assert!(load_global_rules(&store).is_empty());
        assert_eq!(load_global_block_rules(&store), block);
        // …and resolves through the same three-level scopes on its own keys.
        save_channel_block_scope(&store, 7, &TriggerScope { mode: TriggerMode::Off, rules: vec![] })
            .unwrap();
        assert!(effective_block_rules(&store, 7, 1).is_empty());
        assert_eq!(effective_block_rules(&store, 8, 1), block);
        // The whitelist channel scope is untouched by the blacklist save.
        assert!(load_channel_trigger_scope(&store, 7).is_inherit());
    }

    #[test]
    fn serde_roundtrip_and_forward_compat() {
        let scope = TriggerScope {
            mode: TriggerMode::Extend,
            rules: vec![TriggerRule {
                enabled: true,
                field: TriggerField::Game,
                regex: true,
                pattern: "sing".into(),
                capture_from_start: Some(true),
                stop_on_unmatch: true,
                lead_secs: 30,
                end_delay_secs: 15,
            }],
        };
        let json = serde_json::to_string(&scope).unwrap();
        let back: TriggerScope = serde_json::from_str(&json).unwrap();
        assert_eq!(scope, back);
        // Minimal JSON (only a pattern) fills defaults; unknown fields from a
        // future version are tolerated.
        let r: TriggerRule = serde_json::from_str(
            r#"{"pattern":"karaoke","future_action":{"volume":11}}"#,
        )
        .unwrap();
        assert!(r.enabled);
        assert_eq!(r.field, TriggerField::Any);
        assert!(!r.regex);
        assert_eq!(r.capture_from_start, None);
        assert!(!r.stop_on_unmatch);
        assert_eq!(r.lead_secs, 0);
        assert_eq!(r.end_delay_secs, 0);
        assert!(TriggerScope::default().is_inherit());
        // Old JSON missing these fields entirely (pre-this-feature) still
        // deserializes fine with them defaulted off.
        let old: TriggerRule = serde_json::from_str(
            r#"{"enabled":true,"field":"title","regex":false,"pattern":"karaoke"}"#,
        )
        .unwrap();
        assert!(!old.stop_on_unmatch);
        assert_eq!(old.lead_secs, 0);
    }
}
