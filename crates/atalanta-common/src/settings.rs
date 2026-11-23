// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! Named driver and runtime settings: one table for every knob that used to
//! be reachable only as a `ATALANTA_*` environment variable.
//!
//! A suite sets them under `config: settings:` (flat `android.scroll:` keys
//! or nested `android: { scroll: … }`), and `atalanta run --set key=value`
//! overrides them. They are installed as an override layer that
//! [`crate::env::var`] reads first, so every driver sees them without any
//! plumbing of its own. Precedence: `--set` > suite `settings:` > the
//! process environment.

use std::collections::{BTreeMap, HashMap};

/// One setting: its name, the variable it drives, its default, what it does.
pub struct Setting {
    pub key: &'static str,
    pub env: &'static str,
    pub default: &'static str,
    pub values: &'static str,
    pub help: &'static str,
}

pub const SETTINGS: &[Setting] = &[
    Setting { key: "android.text", env: "ATALANTA_ANDROID_TEXT", default: "auto", values: "auto|fast|keys", help: "How inputText/clearText put text in: one ACTION_SET_TEXT (fast), key events (keys), or fast for 16+ characters (auto)" },
    Setting { key: "android.imeEnter", env: "ATALANTA_ANDROID_IME_ENTER", default: "1", values: "1|0", help: "pressKey enter uses the keyboard's own action (Search/Go/Send) first" },
    Setting { key: "android.scroll", env: "ATALANTA_ANDROID_SCROLL", default: "drag", values: "drag|semantic", help: "scrollUntilVisible: touch drags, or the list's own scroll action (~3x faster, no touch)" },
    Setting { key: "android.keepAccessibility", env: "ATALANTA_ANDROID_KEEP_A11Y", default: "0", values: "1|0", help: "Keep TalkBack and other accessibility services running during the run" },
    Setting { key: "android.eventSettle", env: "ATALANTA_ANDROID_EVENT_SETTLE", default: "0", values: "1|0", help: "Settle on the device from accessibility events instead of tree reads" },
    Setting { key: "android.keepAnimations", env: "ATALANTA_KEEP_ANIMATIONS", default: "0", values: "1|0", help: "Leave device animations on (Atalanta turns them off by default)" },
    Setting { key: "android.treeServer", env: "ATALANTA_ANDROID_TREE_SERVER", default: "1", values: "1|0", help: "Use Atalanta's on-device tree server (0: uiautomator dump, much slower)" },
    Setting { key: "android.composeTestTag", env: "ATALANTA_ANDROID_COMPOSE_TESTTAG", default: "1", values: "1|0", help: "Read a Compose testTag as the element's id when the app doesn't set testTagsAsResourceId (tree server, API 26+; costs up to 50 ms of main-thread reads per full dump, cached per node)" },
    Setting { key: "android.focusCheck", env: "ATALANTA_ANDROID_FOCUS_CHECK", default: "1", values: "1|0", help: "inputText fails when its tap left the focus on another element, instead of typing there" },
    Setting { key: "android.adbMode", env: "ATALANTA_ADB_MODE", default: "subprocess", values: "subprocess|tcp", help: "How adb is reached: the adb binary, or the adb server's TCP protocol" },
    Setting { key: "ios.runner", env: "ATALANTA_IOS_RUNNER", default: "wda", values: "wda|atalanta", help: "iOS automation server: WebDriverAgent, or Atalanta's own XCUITest runner" },
    Setting { key: "ios.launch", env: "ATALANTA_IOS_LAUNCH", default: "simctl", values: "simctl|wda", help: "Simulator relaunch in a warm session: simctl (~0.85 s) or WDA (~1.7 s)" },
    Setting { key: "ios.systemAlerts", env: "ATALANTA_IOS_SYSTEM_ALERTS", default: "1", values: "1|0", help: "Look for SpringBoard alerts (permissions, Open in…) so they can be answered" },
    Setting { key: "ios.text", env: "ATALANTA_IOS_TEXT", default: "auto", values: "auto|paste|keys", help: "How inputText puts text in: the pasteboard and Cmd+V (paste, ~0.4 s for any length), typed keys (~9 ms a character), or paste into empty fields when quicker: 50+ characters, 100+ for the first after a launch (auto)" },
    Setting { key: "ios.typingFrequency", env: "ATALANTA_IOS_TYPING_FREQUENCY", default: "180", values: "keys per minute", help: "WDA typing speed" },
    Setting { key: "ios.fullSource", env: "ATALANTA_IOS_FULL_SOURCE", default: "0", values: "1|0", help: "Read WDA's full source (visible/accessible attributes; ~7x slower)" },
    Setting { key: "ios.keepWda", env: "ATALANTA_KEEP_WDA", default: "0", values: "1|0", help: "Leave WDA running after the run so the next one reattaches" },
    Setting { key: "ios.wdaRef", env: "ATALANTA_WDA_REF", default: "pinned", values: "a WDA git tag", help: "WebDriverAgent version to build" },
    Setting { key: "ios.teamId", env: "ATALANTA_IOS_TEAM_ID", default: "", values: "Apple team id", help: "Team that signs the runner for real devices" },
    Setting { key: "ios.wdaBundleId", env: "ATALANTA_WDA_BUNDLE_ID", default: "com.<team>.atalanta.WebDriverAgentRunner", values: "bundle id", help: "Runner bundle id on real devices" },
    Setting { key: "ios.swipeMs", env: "ATALANTA_IOS_SWIPE_MS", default: "250", values: "ms", help: "Stroke time of an iOS swipe gesture (it flings)" },
    Setting { key: "ios.scrollMs", env: "ATALANTA_IOS_SCROLL_MS", default: "350", values: "ms", help: "Stroke time of an iOS scroll drag (ends slowly, so it doesn't fling)" },
    Setting { key: "ios.tapGuardStatusBar", env: "ATALANTA_IOS_TAP_GUARD_STATUS_BAR", default: "1", values: "1|0", help: "Refuse a tap whose only free part is under the status bar (0: tap there; for apps that hide the bar)" },
    Setting { key: "ios.allowWifiRunner", env: "ATALANTA_IOS_ALLOW_WIFI_RUNNER", default: "0", values: "1|0", help: "Run the runner on an iPhone reached over Wi-Fi only (needs ios.runner: atalanta, whose per-launch token is then required; an ios.runnerToken must be 16+ characters); otherwise such a phone fails with \"connect a cable\"" },
    Setting { key: "ios.runnerToken", env: "ATALANTA_IOS_RUNNER_TOKEN", default: "", values: "16+ characters", help: "Fixed bearer token for AtalantaRunner in place of the random one Atalanta makes per launch (a runner at ATALANTA_WDA_URL started by hand needs it as its ATALANTA_RUNNER_TOKEN); every request carries the token (masked; use ${VAR})" },
    Setting { key: "ios.accessibilityDeadlineMs", env: "ATALANTA_IOS_ACCESSIBILITY_DEADLINE_MS", default: "5000", values: "ms", help: "Longest WDA spends on one accessibility query; past it WDA aborts the request with an error (WDA's accessibilityDeadline setting, sent in seconds). The 5000 default is sent only to a WebDriverAgent Atalanta built from v16.14.1 or later; any other WDA gets nothing unless this is set. 0 leaves WDA's default" },
    Setting { key: "ios.snapshotMaxDepth", env: "ATALANTA_IOS_SNAPSHOT_MAX_DEPTH", default: "60", values: "depth", help: "How deep WDA (snapshotMaxDepth) and AtalantaRunner (its deep-tree recovery read) read the element tree; deeper nodes are left out (0 leaves the runners' defaults)" },
    Setting { key: "ios.verifyTap", env: "ATALANTA_IOS_VERIFY_TAP", default: "0", values: "1|0", help: "Before a tap, ask AtalantaRunner what is drawn at the tap point (a private XCTest hit test, skipped where it is missing) and fail when something else is on top; also lets a tap under the status bar through when the app hides the bar. Needs ios.runner: atalanta" },
    Setting { key: "ios.alertRecheckMs", env: "ATALANTA_IOS_ALERT_RECHECK_MS", default: "1000", values: "ms", help: "How long a \"no system alert\" answer is trusted (each check ~80 ms)" },
    Setting { key: "web.networkIdle", env: "ATALANTA_WEB_NETWORK_IDLE", default: "1", values: "1|0", help: "The settle after an action also waits for the page's requests to finish (not websockets, event streams or long polls)" },
    Setting { key: "web.networkIgnore", env: "ATALANTA_WEB_NETWORK_IGNORE", default: "", values: "URL globs, comma-separated", help: "Requests the network-idle wait doesn't wait for (analytics, polling); also sent to the in-app sync probe as ignore_urls" },
    Setting { key: "web.networkIdleMaxMs", env: "ATALANTA_WEB_NETWORK_IDLE_MAX_MS", default: "5000", values: "ms", help: "A request open longer than this is a long poll: the settle stops waiting for it" },
    Setting { key: "visibility.minArea", env: "ATALANTA_VISIBILITY_MIN_AREA", default: "0", values: "0..1", help: "Share of an element's box that must be on screen for assertVisible/assertNotVisible to count it as visible (0: any pixel; Maestro uses 0.1)" },
    Setting { key: "web.focusCheck", env: "ATALANTA_WEB_FOCUS_CHECK", default: "1", values: "1|0", help: "inputText fails when its click left the focus on something that takes no text (a focused cross-origin iframe counts as unknown and passes, unless it already had the focus before the click)" },
    Setting { key: "web.frames", env: "ATALANTA_WEB_FRAMES", default: "1", values: "1|0", help: "Reach into cross-origin iframes: selectors find elements there, taps and typing land there, and their requests count for the network-idle wait (0: such frames are opaque boxes)" },
    Setting { key: "web.visibility", env: "ATALANTA_WEB_VISIBILITY", default: "viewport", values: "viewport|page", help: "What visible means for web assertions: on screen (in the viewport, not clipped by an overflow ancestor) or anywhere on the page (the old rule)" },
    Setting { key: "web.dialogs", env: "ATALANTA_WEB_DIALOGS", default: "dismiss", values: "dismiss|accept", help: "How a page's confirm() and prompt() are answered, in the page and in its cross-origin frames (alert() and beforeunload are always accepted); accept lets a suite test a confirm-guarded action" },
    Setting { key: "web.promptText", env: "ATALANTA_WEB_PROMPT_TEXT", default: "", values: "text", help: "What an accepted prompt() returns (with web.dialogs: accept)" },
    Setting { key: "web.insecureCerts", env: "ATALANTA_WEB_INSECURE_CERTS", default: "0", values: "1|0", help: "Accept any TLS certificate in Chrome (a self-signed dev server); off by default, so a network attacker can't serve a page (that a secret is typed into) with a forged certificate" },
    Setting { key: "web.downloads", env: "ATALANTA_WEB_DOWNLOADS", default: "deny", values: "deny|a folder", help: "Where files the page downloads are saved (relative to the working directory); deny refuses them, with a warning naming the file" },
    Setting { key: "selectors.strict", env: "ATALANTA_SELECTORS_STRICT", default: "0", values: "1|0", help: "Fail a tap, press or text entry whose selector matches several elements on screen, listing them (default: act on the first and warn; while unset the warning says 1.0 makes this an error, 0 keeps today's behaviour)" },
    Setting { key: "actionableWaitMs", env: "ATALANTA_ACTIONABLE_WAIT_MS", default: "2000", values: "ms", help: "Wait for a tap target to be enabled and still before acting anyway" },
    Setting { key: "crash.watchAfterMs", env: "ATALANTA_CRASH_WATCH_AFTER_MS", default: "1000", values: "ms", help: "When a slow post-action settle starts checking the crash log" },
    Setting { key: "crash.watchEveryMs", env: "ATALANTA_CRASH_WATCH_EVERY_MS", default: "500", values: "ms", help: "How often it checks after that" },
    Setting { key: "crash.checkEveryMs", env: "ATALANTA_CRASH_CHECK_EVERY_MS", default: "1000", values: "ms", help: "How often a waiting element lookup checks for a crash, and how often a physical iPhone's crash watcher asks for the app's state (at least 250 ms)" },
    Setting { key: "breaker.threshold", env: "ATALANTA_BREAKER_THRESHOLD", default: "5", values: "failures", help: "Device failures in a row before the circuit breaker opens" },
    Setting { key: "breaker.resetMs", env: "ATALANTA_BREAKER_RESET_MS", default: "30000", values: "ms", help: "How long an open breaker waits before restarting the session and probing" },
    Setting { key: "commandTimeoutMs", env: "ATALANTA_COMMAND_TIMEOUT_MS", default: "60000", values: "ms", help: "Longest a single adb/simctl/xcrun command may run" },
    Setting { key: "secrets", env: "ATALANTA_SECRETS", default: "", values: "VAR,VAR,…", help: "Extra variables whose values are masked in every report and log" },
    Setting { key: "nl.model", env: "ATALANTA_NL_MODEL", default: "", values: "model id", help: "Model that plans natural-language steps (default: the provider's step model, claude-haiku-5-5 for Anthropic)" },
    Setting { key: "nl.effort", env: "ATALANTA_NL_EFFORT", default: "low", values: "low|medium|high", help: "Planner reasoning effort" },
    Setting { key: "nl.cache", env: "ATALANTA_NL_CACHE", default: "normal", values: "normal|read-only|frozen|prune", help: "How the natural-language plan cache is used" },
    Setting { key: "nl.traces", env: "ATALANTA_NL_TRACES", default: "1", values: "1|0", help: "Save planner traces for `atalanta nl-eval`" },
];

/// A millisecond setting read from `env` (default `default_ms`).
pub fn ms(env: &str, default_ms: u64) -> std::time::Duration {
    std::time::Duration::from_millis(num(env, default_ms))
}

/// A number setting read from `env` (default `default`).
pub fn num(env: &str, default: u64) -> u64 {
    crate::env::var(env)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

/// The setting called `key` (case-insensitive).
pub fn find(key: &str) -> Option<&'static Setting> {
    SETTINGS.iter().find(|s| s.key.eq_ignore_ascii_case(key))
}

/// Flatten a suite's `settings:` (nested maps become dotted keys; values
/// become strings: `true` → `1`, `false` → `0`).
pub fn flatten(value: &serde_json::Value) -> BTreeMap<String, String> {
    fn walk(prefix: &str, v: &serde_json::Value, out: &mut BTreeMap<String, String>) {
        match v {
            serde_json::Value::Object(m) => {
                for (k, v) in m {
                    let key = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    };
                    walk(&key, v, out);
                }
            }
            serde_json::Value::Bool(b) => {
                out.insert(prefix.to_string(), if *b { "1" } else { "0" }.to_string());
            }
            serde_json::Value::String(s) => {
                out.insert(prefix.to_string(), s.clone());
            }
            serde_json::Value::Null => {}
            other => {
                out.insert(prefix.to_string(), other.to_string());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk("", value, &mut out);
    out
}

/// Check `pairs` (setting key → value) and turn them into the variables
/// they drive. An unknown key is an error naming the closest one.
pub fn to_env(pairs: &BTreeMap<String, String>) -> Result<HashMap<String, String>, String> {
    let mut env = HashMap::new();
    for (k, v) in pairs {
        let Some(s) = find(k) else {
            let near = SETTINGS
                .iter()
                .min_by_key(|s| distance(&s.key.to_lowercase(), &k.to_lowercase()))
                .map(|s| s.key)
                .unwrap_or("");
            return Err(format!(
                "unknown setting `{k}` (did you mean `{near}`? `atalanta settings` lists them)"
            ));
        };
        // The normalised value is what's installed: the drivers always
        // accepted `true`/`off`/`async`, and a check of the trimmed value
        // must not pass an untrimmed one through (M-3).
        let v = normalise(s, v)?;
        env.insert(s.env.to_string(), v);
    }
    Ok(env)
}

/// Other spellings the drivers accept for a setting's values, as
/// (setting key, alias, canonical value).
const ALIASES: &[(&str, &str, &str)] = &[("android.adbMode", "async", "tcp")];

/// The value to install for `v`: trimmed, with the spellings the drivers
/// have always accepted mapped to the listed value (`true`/`on`/`yes` → `1`,
/// `false`/`off`/`no` → `0`, case-insensitive enum values, aliases such as
/// `async` for `tcp`). A value the setting can't take is an error naming
/// what it can. A typo (`android.scroll: semantics`, `breaker.resetMs:
/// 30s`) silently fell back to the default (VEL-230). Free-form settings
/// take anything.
fn normalise(s: &Setting, v: &str) -> Result<String, String> {
    let v = v.trim();
    if s.values == "1|0" {
        match v.to_ascii_lowercase().as_str() {
            "1" | "true" | "on" | "yes" => return Ok("1".into()),
            "0" | "false" | "off" | "no" => return Ok("0".into()),
            _ => {}
        }
    }
    if s.values.contains('|') && !s.values.contains(' ') {
        if let Some(ok) = s.values.split('|').find(|ok| ok.eq_ignore_ascii_case(v)) {
            return Ok(ok.to_string());
        }
        if let Some((_, _, to)) = ALIASES
            .iter()
            .find(|(k, alias, _)| *k == s.key && alias.eq_ignore_ascii_case(v))
        {
            return Ok(to.to_string());
        }
        return Err(format!(
            "setting `{}` can't be `{v}`: use one of {}",
            s.key,
            s.values.replace('|', ", ")
        ));
    }
    if s.values == "0..1" && !v.parse::<f64>().is_ok_and(|f| (0.0..=1.0).contains(&f)) {
        return Err(format!(
            "setting `{}` needs a number from 0 to 1, not `{v}`",
            s.key
        ));
    }
    if matches!(s.values, "ms" | "failures" | "keys per minute" | "depth")
        && v.parse::<u64>().is_err()
    {
        return Err(format!(
            "setting `{}` needs a whole number ({}), not `{v}`",
            s.key, s.values
        ));
    }
    Ok(v.to_string())
}

fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            cur.push(
                (prev[j] + usize::from(ca != *cb))
                    .min(prev[j + 1] + 1)
                    .min(cur[j] + 1),
            );
        }
        prev = cur;
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fi56: the help said the default applied to every session and that
    /// WDA "answers with what it has"; it aborts with an error, and the
    /// default goes only to a v16.14.1+ WDA Atalanta built.
    #[test]
    fn the_accessibility_deadline_help_says_what_it_does() {
        let s = SETTINGS
            .iter()
            .find(|s| s.key == "ios.accessibilityDeadlineMs")
            .unwrap();
        assert!(
            s.help.contains("aborts the request with an error"),
            "{}",
            s.help
        );
        assert!(
            s.help
                .contains("only to a WebDriverAgent Atalanta built from v16.14.1"),
            "{}",
            s.help
        );
        assert!(!s.help.contains("answering with what it has"), "{}", s.help);
    }

    #[test]
    fn nested_and_flat_settings_map_to_their_variables() {
        let v = serde_json::json!({ "android": { "scroll": "semantic", "imeEnter": false }, "ios.launch": "wda" });
        let flat = flatten(&v);
        assert_eq!(flat["android.scroll"], "semantic");
        assert_eq!(flat["android.imeEnter"], "0");
        let env = to_env(&flat).unwrap();
        assert_eq!(env["ATALANTA_ANDROID_SCROLL"], "semantic");
        assert_eq!(env["ATALANTA_ANDROID_IME_ENTER"], "0");
        assert_eq!(env["ATALANTA_IOS_LAUNCH"], "wda");
    }

    #[test]
    fn visibility_settings_check_their_values() {
        // D-1.
        let set = |k: &str, v: &str| {
            let mut m = BTreeMap::new();
            m.insert(k.to_string(), v.to_string());
            to_env(&m)
        };
        assert_eq!(
            set("visibility.minArea", "0.1").unwrap()["ATALANTA_VISIBILITY_MIN_AREA"],
            "0.1"
        );
        assert!(set("visibility.minArea", "10%").is_err());
        assert!(set("visibility.minArea", "1.5").is_err());
        assert_eq!(
            set("web.visibility", "PAGE").unwrap()["ATALANTA_WEB_VISIBILITY"],
            "page"
        );
        assert!(set("web.visibility", "dom").is_err());
    }

    #[test]
    fn an_unknown_setting_names_the_closest_one() {
        let mut bad = BTreeMap::new();
        bad.insert("android.scrol".to_string(), "semantic".to_string());
        let err = to_env(&bad).unwrap_err();
        assert!(err.contains("android.scroll"), "{err}");
    }

    // VEL-230: a value typo fell back to the default without a word.
    #[test]
    fn values_are_checked() {
        let one = |k: &str, v: &str| {
            let mut m = BTreeMap::new();
            m.insert(k.to_string(), v.to_string());
            to_env(&m)
        };
        let e = one("android.scroll", "semantics").unwrap_err();
        assert!(e.contains("drag, semantic"), "{e}");
        assert!(one("breaker.resetMs", "30s").is_err());
        assert!(one("breaker.resetMs", "30000").is_ok());
        assert!(one("android.imeEnter", "0").is_ok());
        assert!(one("nl.cache", "read-only").is_ok());
        assert!(one("ios.teamId", "ABCDE12345").is_ok());
        assert!(one("web.networkIgnore", "*/analytics/*").is_ok());
    }

    // M-3: values the drivers have always accepted exited 2 once values
    // were checked; and the untrimmed value was installed.
    #[test]
    fn accepted_spellings_are_normalised() {
        let one = |k: &str, v: &str| {
            let mut m = BTreeMap::new();
            m.insert(k.to_string(), v.to_string());
            to_env(&m).map(|e| e.into_values().next().unwrap())
        };
        assert_eq!(one("android.keepAnimations", "true").unwrap(), "1");
        assert_eq!(one("android.keepAnimations", "TRUE").unwrap(), "1");
        assert_eq!(one("web.networkIdle", "off").unwrap(), "0");
        assert_eq!(one("android.treeServer", "false").unwrap(), "0");
        assert_eq!(one("ios.keepWda", "yes").unwrap(), "1");
        assert_eq!(one("android.adbMode", "async").unwrap(), "tcp");
        assert_eq!(one("android.scroll", " drag").unwrap(), "drag");
        assert_eq!(one("android.scroll", "Semantic").unwrap(), "semantic");
        assert_eq!(one("breaker.resetMs", " 500 ").unwrap(), "500");
        assert!(one("android.keepAnimations", "maybe").is_err());
        assert!(one("android.adbMode", "carrier-pigeon").is_err());
    }

    #[test]
    fn every_setting_has_a_unique_key_and_variable() {
        let mut keys = std::collections::HashSet::new();
        let mut envs = std::collections::HashSet::new();
        for s in SETTINGS {
            assert!(keys.insert(s.key), "duplicate key {}", s.key);
            assert!(envs.insert(s.env), "duplicate env {}", s.env);
            assert!(s.env.starts_with("ATALANTA_"), "{}", s.env);
        }
    }
}
