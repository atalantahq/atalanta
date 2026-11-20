//! End to end against the fake app: a scripted planner plays the model,
//! the real executor acts, the YAML is written and replayed.
//!
//! Every test runs on tokio's paused clock: the session's "look again in
//! 600 ms / for 2 s" waits and the executor's settle sleep in virtual time,
//! so the suite takes no wall time for them and can't flake on a loaded
//! runner (review 07 H3). What the tests assert is what happened (reads,
//! notes, steps), never how long it took.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use velocity_common::{Platform, VerifyLevel};
use velocity_explore::gates;
use velocity_explore::scripted::{Script, ScriptedChecker, ScriptedPlanner};
use velocity_explore::testkit::{FakeApp, Node};
use velocity_explore::*;
use velocity_nl::NaturalPlanner;

const TV: &str = "android.widget.TextView";

fn settings_rows() -> Vec<Node> {
    let mut rows = vec![
        Node::text(TV, "Settings").id("com.android.settings:id/homepage_title"),
        Node::text(TV, "Network & internet"),
        Node::text(TV, "Connected devices"),
        Node::text(TV, "Apps"),
        Node::text(TV, "Notifications"),
        Node::text(TV, "Battery"),
    ];
    for i in 0..8 {
        rows.push(Node::text(
            TV,
            &format!("Filler row {}", ["a", "b", "c", "d", "e", "f", "g", "h"][i]),
        ));
    }
    rows.push(Node::text(TV, "Display"));
    rows
}

fn network_rows() -> Vec<Node> {
    vec![
        Node::text(TV, "Network & internet").id("com.android.settings:id/collapsing_toolbar"),
        Node::text(TV, "Internet"),
        Node::text(TV, "SIMs"),
        Node::text(TV, "Airplane mode"),
    ]
}

fn settings_app() -> FakeApp {
    FakeApp::new()
        .screen("home", settings_rows())
        .screen("network", network_rows())
        .screen(
            "display",
            vec![
                Node::text(TV, "Display").id("com.android.settings:id/collapsing_toolbar"),
                Node::text(TV, "Dark theme"),
            ],
        )
        .link("home", "Network & internet", "network")
        .link("home", "Display", "display")
}

fn login_app() -> FakeApp {
    FakeApp::new()
        .screen(
            "login",
            vec![
                Node::text(TV, "Sign in to Loyalty").id("title"),
                Node::text("android.widget.EditText", "")
                    .id("email")
                    .label("Email"),
                Node::text("android.widget.EditText", "")
                    .id("password")
                    .label("Password"),
                Node::text("android.widget.Button", "Sign in").id("submit"),
            ],
        )
        .screen(
            "welcome",
            vec![
                Node::text(TV, "Welcome back"),
                Node::text(TV, "Balance $12.40"),
                Node::text(TV, "Signed in as alice@example.com"),
            ],
        )
        .link("login", "submit", "welcome")
}

fn opts(goal: &str) -> ExploreOptions {
    let mut o = ExploreOptions::new(goal, "com.android.settings", "fake-1", Platform::Android);
    o.config = FakeApp::config();
    o.launch = LaunchMode::Clean;
    // No demonstrations from whatever YAML is around the crate (the golden
    // file is one): the tests that want them name their folder.
    o.demo_dirs = vec![];
    o
}

struct Rig {
    app: Arc<FakeApp>,
    planner: Arc<ScriptedPlanner>,
    checker: Option<Arc<ScriptedChecker>>,
    events: Arc<Mutex<Vec<ExploreEvent>>>,
}

fn rig(app: FakeApp, script: &str) -> Rig {
    let script = Script::from_json(script).unwrap();
    Rig {
        app: Arc::new(app),
        planner: Arc::new(ScriptedPlanner::new(&script)),
        checker: script
            .check
            .clone()
            .map(|c| Arc::new(ScriptedChecker::new(c))),
        events: Arc::new(Mutex::new(vec![])),
    }
}

impl Rig {
    fn job(&self, o: ExploreOptions) -> ExploreJob {
        let events = self.events.clone();
        ExploreJob::new(
            self.app.clone(),
            self.planner.clone(),
            self.checker
                .clone()
                .map(|c| c as Arc<dyn velocity_nl::ScreenChecker>),
            o,
        )
        .with_events(Box::new(move |e| events.lock().unwrap().push(e.clone())))
    }

    fn session(&self, o: ExploreOptions) -> ExploreSession {
        let events = self.events.clone();
        ExploreSession::new(
            self.app.clone(),
            self.planner.clone(),
            self.checker
                .clone()
                .map(|c| c as Arc<dyn velocity_nl::ScreenChecker>),
            o,
        )
        .with_events(Box::new(move |e| events.lock().unwrap().push(e.clone())))
    }

    /// The notes the model got at the start of each turn, as code decided
    /// them: what tests assert on, rather than the prose rendered from them.
    fn notes_at(&self, turn: usize) -> Vec<TurnNote> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                ExploreEvent::Notes { turn: t, notes, .. } if *t == turn => Some(notes.clone()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    fn all_notes(&self) -> Vec<TurnNote> {
        (1..=64).flat_map(|t| self.notes_at(t)).collect()
    }

    fn event_json(&self) -> String {
        self.events
            .lock()
            .unwrap()
            .iter()
            .map(|e| serde_json::to_string(e).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

const NETWORK: &str = r#"{
  "steps": [
    { "op": "tap", "target": "Network & internet" },
    { "op": "assert", "target": "Internet", "reason": "the Internet entry is on the page" },
    { "op": "done", "outcome": "success", "reason": "the Network & internet page is open" }
  ],
  "check": { "holds": true, "evidence": ["SIMs", "Airplane mode"], "reason": "the page lists SIMs" }
}"#;

#[tokio::test(start_paused = true)]
async fn goal_reached_writes_a_test_with_assertions_that_replays_without_the_model() {
    let r = rig(settings_app(), NETWORK);
    let mut job = r.job(opts("open Network & internet"));
    job.replays = 3;
    let result = job.run().await.unwrap();
    assert_eq!(
        result.report.outcome,
        Outcome::Success,
        "{}",
        result.report.reason
    );
    let yaml = result.yaml.clone().expect("a test is written");
    assert!(yaml.contains("clearState: true"), "{yaml}");
    assert!(yaml.contains("tap:"), "{yaml}");
    assert!(yaml.contains("assertVisible"), "{yaml}");
    // The screen assertion, the proposed one and the checker's evidence.
    let g = result.generated.as_ref().unwrap();
    assert!(g.assertions >= 3, "{yaml}");
    // The status-bar clock is never asserted.
    assert!(!yaml.contains("12:"), "{yaml}");
    assert!(!yaml.contains("clock"), "{yaml}");
    // Replays: all passed, no model calls (the scripted planner counts a
    // call per turn; it made exactly the three turns of the session).
    let replay = result.replay.as_ref().unwrap();
    assert_eq!(replay.runs.len(), 3);
    assert!(replay.all_passed(), "{replay:?}");
    assert_eq!(replay.model_calls, 0);
    assert_eq!(r.planner.usage().calls, 3);
    assert!(result.ok());
    // Launches: the session's and one per replay, all clean.
    assert_eq!(r.app.launches(), vec![true; 4]);
    let events = r.event_json();
    for kind in [
        "\"observed\"",
        "\"planned\"",
        "\"acted\"",
        "\"asserted\"",
        "\"checked\"",
        "\"finished\"",
        "\"replayRun\"",
    ] {
        assert!(events.contains(kind), "{kind} missing:\n{events}");
    }
}

#[tokio::test(start_paused = true)]
async fn generated_yaml_matches_the_golden_file() {
    let r = rig(settings_app(), NETWORK);
    let mut job = r.job(opts("open Network & internet"));
    job.replays = 0;
    let yaml = job.run().await.unwrap().yaml.unwrap();
    let golden = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/network.yaml");
    if std::env::var("VELOCITY_BLESS").is_ok() {
        std::fs::create_dir_all(golden.parent().unwrap()).unwrap();
        std::fs::write(&golden, &yaml).unwrap();
    }
    let want = std::fs::read_to_string(&golden).expect("golden file (VELOCITY_BLESS=1 writes it)");
    assert_eq!(yaml, want);
}

#[tokio::test(start_paused = true)]
async fn a_target_below_the_fold_becomes_scroll_until_visible() {
    let r = rig(
        settings_app(),
        r#"{
          "steps": [
            { "op": "tap", "target": "Display", "scroll": "down" },
            { "op": "done", "outcome": "success", "reason": "Display is open" }
          ],
          "check": { "holds": true, "evidence": ["Dark theme"], "reason": "dark theme toggle shown" }
        }"#,
    );
    let mut job = r.job(opts("open Display"));
    job.replays = 1;
    let result = job.run().await.unwrap();
    assert_eq!(
        result.report.outcome,
        Outcome::Success,
        "{}\n{}",
        result.report.reason,
        r.event_json()
    );
    let yaml = result.yaml.unwrap();
    assert!(yaml.contains("scrollUntilVisible"), "{yaml}");
    assert!(!yaml.contains("swipe"), "{yaml}");
    assert!(result.replay.unwrap().all_passed());
}

#[tokio::test(start_paused = true)]
async fn secrets_and_data_become_placeholders_and_user_data_is_never_asserted() {
    let r = rig(
        login_app(),
        r#"{
          "steps": [
            { "op": "inputText", "target": "Email", "value": "EMAIL" },
            { "op": "inputText", "target": "Password", "value": "password" },
            { "op": "tap", "target": "Sign in" },
            { "op": "done", "outcome": "success", "reason": "welcome shown" }
          ],
          "check": { "holds": true, "evidence": ["Welcome back", "Balance $12.40", "Signed in as alice@example.com"], "reason": "welcome" }
        }"#,
    );
    let secret = "S3cr3t-pw-T5-explore";
    let mut o = opts("sign in");
    o.app_id = "com.loyalty".into();
    o.data = vec![("EMAIL".into(), "alice@example.com".into())];
    o.secrets = vec![("PASSWORD".into(), secret.into())];
    let mut job = r.job(o);
    job.replays = 1;
    job.keep_nl = true;
    let result = job.run().await.unwrap();
    assert_eq!(
        result.report.outcome,
        Outcome::Success,
        "{}",
        result.report.reason
    );
    let yaml = result.yaml.clone().unwrap();
    assert!(
        yaml.contains("${EMAIL}") && yaml.contains("${PASSWORD}"),
        "{yaml}"
    );
    assert!(
        !yaml.contains(secret) && !yaml.contains("alice@example.com"),
        "{yaml}"
    );
    // Volatile and user-data texts are skipped; the stable one is kept.
    assert!(yaml.contains("Welcome back"), "{yaml}");
    assert!(!yaml.contains("12.40"), "{yaml}");
    let nl = result.nl_yaml.unwrap();
    assert!(!nl.contains(secret) && nl.contains("${PASSWORD}"), "{nl}");
    // The replay typed the real values from the overrides.
    assert!(result.replay.unwrap().all_passed());
    let typed = r.app.typed();
    assert_eq!(typed.len(), 4, "{typed:?}");
    assert!(
        typed
            .iter()
            .filter(|(id, v)| id == "password" && v == secret)
            .count()
            == 2
    );
    // Nothing reported or sent to the model holds the secret.
    assert!(!r.event_json().contains(secret));
    assert!(!serde_json::to_string(&result.report)
        .unwrap()
        .contains(secret));
    let seen = r.planner.seen_notes.lock().unwrap().join("\n")
        + &r.planner.seen_history.lock().unwrap().join("\n");
    assert!(!seen.contains(secret), "{seen}");
}

#[tokio::test(start_paused = true)]
async fn a_typed_value_must_be_named_not_written() {
    let r = rig(
        login_app(),
        r#"{
          "steps": [
            { "op": "inputText", "target": "Email", "value": "mallory@evil.test" },
            { "op": "inputText", "target": "Email", "value": "EMAIL" },
            { "op": "done", "outcome": "giveUp", "reason": "stop here" }
          ]
        }"#,
    );
    let mut o = opts("type the email");
    o.verify = VerifyLevel::Off;
    o.data = vec![("EMAIL".into(), "alice@example.com".into())];
    let report = r.session(o).run().await.unwrap();
    assert_eq!(report.outcome, Outcome::GaveUp);
    // The made-up value was refused; only the named one was typed.
    assert_eq!(
        r.app.typed(),
        vec![("email".to_string(), "alice@example.com".to_string())]
    );
    assert!(r.event_json().contains("\"rejected\""));
    let notes = r.planner.seen_notes.lock().unwrap();
    assert!(
        notes[1].contains("value must be one of the listed names"),
        "{notes:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn waits_between_no_ops_do_not_reset_the_stuck_count() {
    let r = rig(
        settings_app(),
        r#"{ "steps": [
            { "op": "tap", "target": "Apps" },
            { "op": "wait", "ms": 10 },
            { "op": "tap", "target": "Apps" },
            { "op": "wait", "ms": 10 },
            { "op": "tap", "target": "Apps" },
            { "op": "wait", "ms": 10 },
            { "op": "tap", "target": "Apps" },
            { "op": "wait", "ms": 10 }
        ] }"#,
    );
    let mut o = opts("open apps");
    o.verify = VerifyLevel::Off;
    let report = r.session(o).run().await.unwrap();
    assert_eq!(report.outcome, Outcome::Stuck, "{}", report.reason);
    assert_eq!(
        report.turns, 5,
        "three no-op taps, the waits between them neutral"
    );
}

#[tokio::test(start_paused = true)]
async fn idle_waits_alone_end_as_stuck() {
    let steps = [r#"{ "op": "wait", "ms": 10 }"#; 8].join(",");
    let r = rig(settings_app(), &format!(r#"{{ "steps": [{steps}] }}"#));
    let mut o = opts("open apps");
    o.verify = VerifyLevel::Off;
    let report = r.session(o).run().await.unwrap();
    assert_eq!(report.outcome, Outcome::Stuck, "{}", report.reason);
    // Two waits are free; the next three count.
    assert_eq!(report.turns, 5);
}

#[tokio::test(start_paused = true)]
async fn a_screen_that_never_changes_is_stuck() {
    let r = rig(
        settings_app(),
        r#"{ "steps": [
            { "op": "tap", "target": "Apps" },
            { "op": "tap", "target": "Apps" },
            { "op": "tap", "target": "Apps" },
            { "op": "tap", "target": "Apps" },
            { "op": "tap", "target": "Apps" }
        ] }"#,
    );
    let mut o = opts("open apps");
    o.verify = VerifyLevel::Off;
    let report = r.session(o).run().await.unwrap();
    assert_eq!(report.outcome, Outcome::Stuck, "{}", report.reason);
    assert_eq!(report.turns, 3);
    // The model was told before the run gave up.
    assert!(
        r.notes_at(3).contains(&TurnNote::Streak {
            count: 2,
            missed: false
        }),
        "{:?}",
        r.notes_at(3)
    );
}

#[tokio::test(start_paused = true)]
async fn the_turn_budget_ends_the_run() {
    let r = rig(
        settings_app(),
        r#"{ "steps": [
            { "op": "tap", "target": "Network & internet" },
            { "op": "back" },
            { "op": "tap", "target": "Network & internet" },
            { "op": "back" }
        ] }"#,
    );
    let mut o = opts("wander");
    o.verify = VerifyLevel::Off;
    o.max_steps = 2;
    let report = r.session(o).run().await.unwrap();
    assert_eq!(report.outcome, Outcome::BudgetExhausted);
    assert_eq!(report.turns, 2);
    assert_eq!(r.planner.usage().calls, 2);
    // History: the second turn saw the first summarised in one line.
    let history = r.planner.seen_history.lock().unwrap();
    assert!(history[1].contains("#1 tap"), "{history:?}");
    assert!(
        history[1].contains("-> \"Network & internet\""),
        "{history:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_checker_citing_nothing_real_is_not_success() {
    let r = rig(
        settings_app(),
        r#"{
          "steps": [
            { "op": "tap", "target": "Network & internet" },
            { "op": "done", "outcome": "success", "reason": "done" },
            { "op": "done", "outcome": "success", "reason": "really done" }
          ],
          "check": { "holds": true, "evidence_ids": [99, -1], "reason": "trust me" }
        }"#,
    );
    let result = r.job(opts("open Network & internet")).run().await.unwrap();
    assert_eq!(
        result.report.outcome,
        Outcome::Unverified,
        "{}",
        result.report.reason
    );
    assert!(result.yaml.is_none());
    assert!(!result.ok());
    assert_eq!(result.report.checks.len(), 2);
    // The model was told the first check failed.
    assert!(
        r.notes_at(3)
            .iter()
            .any(|n| matches!(n, TurnNote::CheckRejected { .. })),
        "{:?}",
        r.notes_at(3)
    );
}

#[tokio::test(start_paused = true)]
async fn verify_without_a_checker_is_a_config_error() {
    let r = rig(settings_app(), r#"{ "steps": [] }"#);
    let err = r.session(opts("x")).run().await.unwrap_err();
    assert_eq!(err.exit_code(), 2, "{err}");
}

#[tokio::test(start_paused = true)]
async fn injected_instructions_reach_the_next_turn() {
    let r = rig(
        settings_app(),
        r#"{ "steps": [
            { "op": "tap", "target": "Network & internet" },
            { "op": "done", "outcome": "giveUp", "reason": "told to stop" }
        ] }"#,
    );
    let mut o = opts("x");
    o.verify = VerifyLevel::Off;
    let session = r.session(o);
    let handle = session.handle();
    handle.inject("use the search box instead".into());
    let report = session.run().await.unwrap();
    assert_eq!(report.outcome, Outcome::GaveUp);
    let notes = r.planner.seen_notes.lock().unwrap();
    assert!(
        notes[0].contains("Operator instruction: use the search box instead"),
        "{notes:?}"
    );
    assert!(!notes[1].contains("search box"), "consumed once: {notes:?}");
    assert!(r.event_json().contains("\"injected\""));
}

#[tokio::test(start_paused = true)]
async fn a_stopped_session_says_so() {
    let r = rig(settings_app(), NETWORK);
    let session = r.session(opts("x"));
    session.handle().stop();
    let report = session.run().await.unwrap();
    assert_eq!(report.outcome, Outcome::Stopped);
    assert_eq!(r.planner.usage().calls, 0);
}

#[tokio::test(start_paused = true)]
async fn a_failed_replay_is_repaired_once() {
    // The app "ships an update" after the session: the network page loses
    // its toolbar id and the Internet row, so the screen assertion fails on
    // replay. The repair drops it and the second replay passes.
    let updated = vec![
        Node::text(TV, "Network and internet"),
        Node::text(TV, "SIMs"),
        Node::text(TV, "Airplane mode"),
    ];
    let r = rig(
        settings_app().change_after_launches(2, "network", updated),
        r#"{
          "steps": [
            { "op": "tap", "target": "Network & internet" },
            { "op": "done", "outcome": "success", "reason": "open" }
          ],
          "check": { "holds": true, "evidence": ["SIMs"], "reason": "SIMs listed" }
        }"#,
    );
    let mut job = r.job(opts("open Network & internet"));
    job.replays = 1;
    let result = job.run().await.unwrap();
    assert!(result
        .replay_before_repair
        .as_ref()
        .is_some_and(|r| !r.all_passed()));
    assert!(
        result.repair.as_deref().unwrap_or("").contains("dropped"),
        "{:?}",
        result.repair
    );
    assert!(
        result.replay.as_ref().unwrap().all_passed(),
        "{:?}",
        result.replay
    );
    assert!(result.ok());
    assert!(r.event_json().contains("\"repair\""));
}

#[tokio::test(start_paused = true)]
async fn a_screenshot_goes_only_with_a_poor_tree_and_an_image_model() {
    let poor = || {
        FakeApp::new()
            .screen("canvas", vec![Node::text(TV, "Game")])
            .screen("rich", settings_rows())
            .link("canvas", "Game", "rich")
    };
    let script = r#"{ "steps": [
        { "op": "tap", "target": "Game" },
        { "op": "done", "outcome": "giveUp", "reason": "x" }
    ] }"#;
    for images in [true, false] {
        let r = rig(poor(), script);
        let mut o = opts("play");
        o.verify = VerifyLevel::Off;
        o.images = images;
        let rep = r.session(o).run().await.unwrap();
        assert_eq!(
            rep.outcome,
            Outcome::GaveUp,
            "{}\n{}",
            rep.reason,
            r.event_json()
        );
        // The canvas lists 2 elements (with the clock); the next screen many.
        assert_eq!(*r.planner.seen_shots.lock().unwrap(), vec![images, false]);
    }
}

// Benchmark mode (`--benchmark`, `--values-from-goal`, `answer`, `openApp`).

fn bench_opts(goal: &str) -> ExploreOptions {
    let mut o = opts(goal);
    o.benchmark = true;
    o.launch = LaunchMode::Attach;
    o.verify = VerifyLevel::Off;
    o
}

#[tokio::test(start_paused = true)]
async fn values_from_goal_types_grounded_text_and_refuses_invented_text() {
    let r = rig(
        login_app(),
        r#"{
          "steps": [
            { "op": "inputText", "target": "Email", "value": "mallory@evil.test" },
            { "op": "inputText", "target": "Email", "value": "Alice@Example.com" },
            { "op": "done", "outcome": "giveUp", "reason": "stop here" }
          ]
        }"#,
    );
    let mut o = bench_opts("Sign in as alice@example.com");
    o.values_from_goal = true;
    let report = r.session(o).run().await.unwrap();
    assert_eq!(report.outcome, Outcome::GaveUp);
    // Only the text the goal holds was typed (as the model wrote it).
    assert_eq!(
        r.app.typed(),
        vec![("email".to_string(), "Alice@Example.com".to_string())]
    );
    let notes = r.planner.seen_notes.lock().unwrap();
    assert!(notes[1].contains("is not in the goal"), "{notes:?}");
    // The model is told it may copy text.
    let values = r.planner.seen_values.lock().unwrap();
    assert!(
        values[0].contains("copied exactly from the goal"),
        "{values:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn without_values_from_goal_copied_text_is_still_refused() {
    let r = rig(
        login_app(),
        r#"{ "steps": [
            { "op": "inputText", "target": "Email", "value": "alice@example.com" },
            { "op": "done", "outcome": "giveUp", "reason": "stop" }
        ] }"#,
    );
    let report = r
        .session(bench_opts("Sign in as alice@example.com"))
        .run()
        .await
        .unwrap();
    assert_eq!(report.outcome, Outcome::GaveUp);
    assert!(r.app.typed().is_empty());
    assert!(!r.planner.seen_values.lock().unwrap()[0].contains("copied"));
}

#[tokio::test(start_paused = true)]
async fn an_answer_the_check_rejects_does_not_end_the_run() {
    let r = rig(
        settings_app(),
        r#"{
          "steps": [
            { "op": "answer", "value": "Connected devices" },
            { "op": "done", "outcome": "success", "reason": "answered" },
            { "op": "done", "outcome": "giveUp", "reason": "can't finish" }
          ],
          "check": { "holds": false, "reason": "the goal also asked to open Network & internet, which the log never shows" }
        }"#,
    );
    let mut o = bench_opts("Open Network & internet, then tell me which entry comes after it.");
    o.verify = VerifyLevel::EXPLORE_DEFAULT;
    let report = r.session(o).run().await.unwrap();
    assert_ne!(report.outcome, Outcome::Success, "{}", report.reason);
    let notes = r.planner.seen_notes.lock().unwrap();
    assert!(
        notes.iter().any(|n| n.contains("did not confirm it")),
        "the agent is told the check failed: {notes:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn an_answer_must_be_grounded_and_then_ends_the_run_as_success() {
    let r = rig(
        settings_app(),
        r#"{
          "steps": [
            { "op": "answer", "value": "Bluetooth", "memo": "looking at the list" },
            { "op": "answer", "value": "Connected devices", "target": "Connected devices" },
            { "op": "done", "outcome": "success", "reason": "answered" }
          ],
          "check": { "holds": true, "evidence": ["Connected devices"], "reason": "the answer names the entry after Network & internet" }
        }"#,
    );
    let mut o = bench_opts("Which entry comes after Network & internet? Answer with the title.");
    o.verify = VerifyLevel::EXPLORE_DEFAULT;
    let report = r.session(o).run().await.unwrap();
    assert_eq!(report.outcome, Outcome::Success, "{}", report.reason);
    assert_eq!(report.answer.as_deref(), Some("Connected devices"));
    assert_eq!(
        report.prompt_version,
        velocity_nl::goal::GOAL_PROMPT_VERSION
    );
    let ev = r.event_json();
    assert!(ev.contains("\"answered\""), "{ev}");
    assert!(ev.contains("\"rejected\""), "{ev}");
    // An answer no longer skips the final check (iOSWorld: every false
    // success went through that shortcut).
    assert!(ev.contains("\"checked\""), "{ev}");
    // The check is told what code itself found on screen, in its own block.
    assert!(ev.contains("<answer-"), "{ev}");
    assert!(ev.contains("shown on a screen (verified by code)"), "{ev}");
    assert_eq!(report.answer_sources, ["screen"]);
    let notes = r.planner.seen_notes.lock().unwrap();
    assert!(
        notes[1].contains("not supported by text shown"),
        "{notes:?}"
    );
    // The memo came back on the next turn.
    let memos = r.planner.seen_memos.lock().unwrap();
    assert_eq!(memos[0], "");
    assert_eq!(memos[1], "looking at the list");
}

#[tokio::test(start_paused = true)]
async fn open_app_home_and_enter_act_and_launch_is_refused_in_a_benchmark() {
    let r = rig(
        settings_app(),
        r#"{
          "steps": [
            { "op": "launch" },
            { "op": "assert", "target": "Apps" },
            { "op": "home" },
            { "op": "openApp", "value": "Mail" },
            { "op": "openApp", "value": "settings" },
            { "op": "enter" },
            { "op": "done", "outcome": "giveUp", "reason": "stop" }
          ]
        }"#,
    );
    let mut o = bench_opts("open Settings");
    o.apps = vec![("Settings".into(), "com.android.settings".into())];
    // The fake app's screen doesn't change on these keys.
    o.stuck_limit = 6;
    let report = r.session(o).run().await.unwrap();
    assert_eq!(report.outcome, Outcome::GaveUp);
    // Attach mode: the only launch is the openApp.
    assert_eq!(r.app.launches(), vec![false]);
    let notes = r.planner.seen_notes.lock().unwrap();
    assert!(
        notes[1].contains("launch is off in a benchmark"),
        "{notes:?}"
    );
    assert!(
        notes[2].contains("assert is off in a benchmark"),
        "{notes:?}"
    );
    assert!(
        notes[4].contains("one of the apps you may open (Settings)"),
        "{notes:?}"
    );
    // Every turn asks for the benchmark instructions (no assert, no
    // launch), and no note repeats them.
    assert!(r.planner.seen_benchmark.lock().unwrap().iter().all(|b| *b));
    assert!(!notes.iter().any(|n| n.contains("judged on the device")));
    let history = r.planner.seen_history.lock().unwrap().join("\n");
    assert!(history.contains("openApp \"Settings\""), "{history}");
    assert!(history.contains("pressKey home"), "{history}");
    assert!(history.contains("pressKey enter"), "{history}");
}

#[tokio::test(start_paused = true)]
async fn scroll_inside_a_named_list() {
    let r = rig(
        settings_app(),
        r#"{ "steps": [
            { "op": "scroll", "target": "Apps", "direction": "down" },
            { "op": "swipe", "target": "Battery", "direction": "left" },
            { "op": "done", "outcome": "giveUp", "reason": "stop" }
        ] }"#,
    );
    let report = r.session(bench_opts("find Display")).run().await.unwrap();
    assert_eq!(report.outcome, Outcome::GaveUp);
    let history = r.planner.seen_history.lock().unwrap().join("\n");
    assert!(history.contains("scroll down inside \"Apps\""), "{history}");
    assert!(
        history.contains("swipe left inside \"Battery\""),
        "{history}"
    );
}

// Demonstrations from the project's tests, and on-demand reflection.

const NETWORK_SUITE: &str = r#"
appId: com.android.settings
tests:
  - name: Open Network & internet
    description: open Network & internet and check the Internet entry
    steps:
      - launchApp
      - tap: { text: "Secret Menu" }
      - inputText: { selector: { id: "search" }, text: "hunter2" }
      - tap: { text: "Network & internet" }
      - assertVisible: { text: "Internet" }
  - name: Dark theme
    steps:
      - tap: { text: "Display" }
      - tap: { text: "Dark theme" }
"#;

#[tokio::test(start_paused = true)]
async fn similar_tests_reach_the_prompt_but_grounding_still_decides() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("net.yaml"), NETWORK_SUITE).unwrap();
    let r = rig(
        settings_app(),
        r#"{ "steps": [
            { "op": "tap", "element": 99 },
            { "op": "inputText", "element": 1, "value": "hunter2" },
            { "op": "tap", "target": "Network & internet" },
            { "op": "done", "outcome": "giveUp", "reason": "stop" }
        ] }"#,
    );
    let mut o = opts("open Network & internet");
    o.verify = VerifyLevel::Off;
    o.demo_dirs = vec![dir.path().to_path_buf()];
    let report = r.session(o).run().await.unwrap();
    assert_eq!(report.outcome, Outcome::GaveUp, "{}", report.reason);
    assert_eq!(report.demos, ["net.yaml: Open Network & internet"]);

    // Every turn carries the demonstration, typed text only by name.
    let demos = r.planner.seen_demos.lock().unwrap().clone();
    assert_eq!(demos.len(), 4);
    assert!(demos.iter().all(|d| d == &demos[0]));
    assert!(
        demos[0].starts_with("Test \"Open Network & internet\" (net.yaml)"),
        "{}",
        demos[0]
    );
    assert!(demos[0].contains("- tap { text: \"Secret Menu\" }"));
    assert!(demos[0].contains("<- (text)"));
    assert!(!demos[0].contains("hunter2"));
    assert!(!demos[0].contains("Dark theme"));
    assert!(r
        .event_json()
        .contains("showing the model how similar goals were done"));

    // The demo's element isn't on the screen and its typed text isn't a
    // value: both refused, and only the listed element was tapped.
    let notes = r.planner.seen_notes.lock().unwrap();
    assert!(
        notes[1].contains("element 99 is not in the list"),
        "{notes:?}"
    );
    assert!(notes[2].contains("no values to type"), "{notes:?}");
    assert_eq!(r.app.taps().len(), 1, "{:?}", r.app.taps());
    assert!(r.app.typed().is_empty());
}

#[tokio::test(start_paused = true)]
async fn demonstrations_can_be_turned_off_and_stay_out_of_benchmarks() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("net.yaml"), NETWORK_SUITE).unwrap();
    let script = r#"{ "steps": [ { "op": "done", "outcome": "giveUp", "reason": "x" } ] }"#;
    for (off, benchmark) in [(true, false), (false, true)] {
        let r = rig(settings_app(), script);
        let mut o = if benchmark {
            bench_opts("open Network & internet")
        } else {
            opts("open Network & internet")
        };
        o.verify = VerifyLevel::Off;
        o.demo_dirs = vec![dir.path().to_path_buf()];
        o.demos = !off;
        let report = r.session(o).run().await.unwrap();
        assert!(report.demos.is_empty());
        assert_eq!(*r.planner.seen_demos.lock().unwrap(), vec![String::new()]);
    }
}

#[tokio::test(start_paused = true)]
async fn the_model_is_told_when_an_action_did_nothing_or_led_back() {
    let r = rig(
        settings_app(),
        r#"{ "steps": [
            { "op": "scroll", "direction": "up" },
            { "op": "tap", "target": "Network & internet" },
            { "op": "back" },
            { "op": "tap", "target": "Apps" },
            { "op": "done", "outcome": "giveUp", "reason": "x" }
        ] }"#,
    );
    let mut o = opts("look around");
    o.verify = VerifyLevel::Off;
    let report = r.session(o).run().await.unwrap();
    assert_eq!(report.outcome, Outcome::GaveUp, "{}", report.reason);
    assert_eq!(r.planner.seen_notes.lock().unwrap().len(), 5);
    let no_effect = |n: &TurnNote| matches!(n, TurnNote::NoEffect { .. });
    // Turn 1 scrolled up at the top of the list: nothing moved.
    assert!(r.notes_at(2).contains(&TurnNote::NoEffect {
        action: "scroll up".into(),
        why: NoEffect::Moved
    }));
    // Turns 2 and 3 went somewhere and came back.
    assert!(!r.notes_at(3).iter().any(no_effect), "{:?}", r.notes_at(3));
    assert!(r.notes_at(4).contains(&TurnNote::LoopBack {
        turns_ago: 2,
        at_turn: 2
    }));
    // Turn 4 tapped a row that leads nowhere.
    assert!(r.notes_at(5).contains(&TurnNote::NoEffect {
        action: "tap { text: \"Apps\" }".into(),
        why: NoEffect::Unchanged
    }));
    let loops = r
        .all_notes()
        .into_iter()
        .filter(|n| matches!(n, TurnNote::LoopBack { .. }))
        .count();
    assert_eq!(loops, 1);
    // The decisions reach the event stream with their kind.
    assert!(r.event_json().contains(r#""kind":"noEffect""#));
}

#[tokio::test(start_paused = true)]
async fn a_back_that_led_round_is_refused_once_on_that_screen() {
    let r = rig(
        settings_app(),
        r#"{ "steps": [
            { "op": "tap", "target": "Network & internet" },
            { "op": "back" },
            { "op": "tap", "target": "Network & internet" },
            { "op": "back" },
            { "op": "back" },
            { "op": "done", "outcome": "giveUp", "reason": "x" }
        ] }"#,
    );
    let mut o = opts("look around");
    o.verify = VerifyLevel::Off;
    let report = r.session(o).run().await.unwrap();
    assert_eq!(report.outcome, Outcome::GaveUp, "{}", report.reason);
    let refused_at = |turn: usize| {
        r.events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, ExploreEvent::Rejected { turn: t, .. } if *t == turn))
    };
    // Turn 4, back on the Network page: told, and its back refused.
    assert!(r.notes_at(4).contains(&TurnNote::BackLedRound));
    assert!(refused_at(4));
    // Turn 5: the same back again is allowed (it may be the only way out).
    assert!(!r.notes_at(5).contains(&TurnNote::BackLedRound));
    assert!(!refused_at(5));
    let backs = report
        .steps
        .iter()
        .filter(|s| s.label.contains("back"))
        .count();
    assert_eq!(
        backs,
        2,
        "{:?}",
        report.steps.iter().map(|s| &s.label).collect::<Vec<_>>()
    );
    // A tap repeated on a looped screen is never refused.
    assert!(!refused_at(3));
}

fn late_page(web: bool) -> FakeApp {
    let page = if web {
        "android.webkit.WebView"
    } else {
        "android.widget.FrameLayout"
    };
    FakeApp::new()
        .screen(
            "page",
            vec![
                Node::text(page, "Memory Task"),
                Node::text(TV, "3").id("number"),
                Node::text("android.widget.Button", "Click Me").id("button"),
            ],
        )
        .screen(
            "next",
            vec![
                Node::text(page, "Memory Task"),
                Node::text(TV, "7").id("number"),
                Node::text("android.widget.Button", "Click Me").id("button"),
            ],
        )
        .link("page", "button", "next")
        .late_by_reads(3)
}

#[tokio::test(start_paused = true)]
async fn a_web_page_that_updates_late_is_not_a_no_op() {
    let script = r#"{ "steps": [
        { "op": "tap", "target": "Click Me" },
        { "op": "done", "outcome": "giveUp", "reason": "x" }
    ] }"#;
    // In a web view the change shows up late; explore keeps looking.
    let r = rig(late_page(true), script);
    let mut o = opts("click it");
    o.verify = VerifyLevel::Off;
    r.session(o).run().await.unwrap();
    let no_effect = |n: &TurnNote| matches!(n, TurnNote::NoEffect { .. });
    assert!(!r.notes_at(2).iter().any(no_effect), "{:?}", r.notes_at(2));
    assert_eq!(r.app.current(), "next");
    // Only a number changed: the history names it.
    let history = r.planner.seen_history.lock().unwrap().clone();
    assert!(
        history[1].contains(r#"(text changed: "3" is now "7")"#),
        "{history:?}"
    );
    // A native screen gets the one short second look, as before.
    let r = rig(late_page(false), script);
    let mut o = opts("click it");
    o.verify = VerifyLevel::Off;
    r.session(o).run().await.unwrap();
    assert!(r.notes_at(2).iter().any(no_effect), "{:?}", r.notes_at(2));
}

#[tokio::test(start_paused = true)]
async fn the_final_check_sees_what_each_action_brought_on_screen() {
    let r = rig(settings_app(), NETWORK);
    let mut o = opts("open network settings");
    o.verify = VerifyLevel::Final;
    r.session(o).run().await.unwrap();
    let events = r.event_json();
    let claim = events
        .lines()
        .find(|l| l.contains("\"claim\""))
        .expect("a checked event");
    // The tap's line names the texts the Network page brought.
    assert!(claim.contains("[then shown: "), "{claim}");
    assert!(claim.contains("SIMs"), "{claim}");
    assert!(claim.contains("Airplane mode"), "{claim}");
}

#[tokio::test(start_paused = true)]
async fn a_result_that_comes_late_after_a_return_is_waited_for() {
    // Compose -> type -> Send: the field empties at once (the screen is
    // the one before typing), the sent bubble comes later.
    let app = FakeApp::new()
        .screen(
            "thread",
            vec![
                Node::text(TV, "Noa"),
                Node::text("android.widget.Button", "Write").id("write"),
            ],
        )
        .screen(
            "typing",
            vec![
                Node::text(TV, "Noa"),
                Node::text(TV, "Inconceivable! (draft)"),
                Node::text("android.widget.Button", "Send").id("send"),
            ],
        )
        .screen(
            "sent",
            vec![
                Node::text(TV, "Noa"),
                Node::text(TV, "Inconceivable!"),
                Node::text("android.widget.Button", "Write").id("write"),
            ],
        )
        .link("thread", "write", "typing")
        .link("typing", "send", "sent")
        .interim("typing", "send", "thread", 3);
    let r = rig(
        app,
        r#"{ "steps": [
            { "op": "tap", "target": "Write" },
            { "op": "tap", "target": "Send" },
            { "op": "done", "outcome": "giveUp", "reason": "x" }
        ] }"#,
    );
    let mut o = opts("send it");
    o.verify = VerifyLevel::Off;
    let report = r.session(o).run().await.unwrap();
    let notes = r.planner.seen_notes.lock().unwrap().clone();
    assert_eq!(
        notes.len(),
        3,
        "{} {notes:?} {}",
        report.reason,
        r.event_json()
    );
    assert!(
        !r.notes_at(3)
            .iter()
            .any(|n| matches!(n, TurnNote::LoopBack { .. })),
        "{:?}",
        r.notes_at(3)
    );
    assert_eq!(r.app.current(), "sent");
}

#[tokio::test(start_paused = true)]
async fn a_web_tap_that_shows_nothing_is_not_to_be_repeated() {
    let app = FakeApp::new().screen(
        "draw",
        vec![
            Node::text("android.webkit.WebView", "Draw"),
            Node::text("android.widget.Button", "")
                .id("teal")
                .label("teal"),
            Node::text("android.widget.Button", "Submit").id("submit"),
        ],
    );
    let r = rig(
        app,
        r#"{ "steps": [
            { "op": "tap", "target": "teal" },
            { "op": "tap", "target": "teal" },
            { "op": "done", "outcome": "giveUp", "reason": "x" }
        ] }"#,
    );
    let mut o = opts("draw with teal");
    o.verify = VerifyLevel::Off;
    r.session(o).run().await.unwrap();
    // Both times, also at the second (a streak): go on, don't tap again.
    for turn in [2, 3] {
        assert!(
            r.notes_at(turn).iter().any(|n| matches!(
                n,
                TurnNote::NoEffect {
                    why: NoEffect::WebTap,
                    ..
                }
            )),
            "{turn}: {:?}",
            r.notes_at(turn)
        );
    }
}

// Expected post-conditions (Q-Reflector): the model says what should hold
// after its action; code checks it on the next screen.

#[tokio::test(start_paused = true)]
async fn a_met_expectation_becomes_an_assertion_built_from_the_real_element() {
    let r = rig(
        settings_app(),
        r#"{ "steps": [
            { "op": "tap", "target": "Network & internet", "expect": { "appears": "airplane" } },
            { "op": "done", "outcome": "success", "reason": "open" }
          ],
          "check": { "holds": true, "evidence": ["SIMs"], "reason": "listed" } }"#,
    );
    let mut job = r.job(opts("open Network & internet"));
    job.replays = 2;
    let result = job.run().await.unwrap();
    assert_eq!(
        result.report.outcome,
        Outcome::Success,
        "{}",
        result.report.reason
    );
    let e = result.report.expectations;
    assert_eq!((e.met, e.not_met, e.not_checkable), (1, 0, 0));
    let yaml = result.yaml.clone().unwrap();
    // The selector is the matched element's own text, not the model's.
    assert!(yaml.contains("text: Airplane mode"), "{yaml}");
    assert!(!yaml.contains("airplane"), "{yaml}");
    // Right after the tap, before the final evidence.
    assert!(yaml.find("Airplane mode").unwrap() < yaml.find("SIMs").unwrap());
    let g = result.generated.as_ref().unwrap();
    assert!(g
        .origins
        .contains(&velocity_explore::yaml::Origin::ExpectAssert(0)));
    let replay = result.replay.unwrap();
    assert!(replay.all_passed(), "{replay:?}");
    assert_eq!(replay.model_calls, 0);
    assert!(r.event_json().contains("\"outcome\":\"met\""));
}

#[tokio::test(start_paused = true)]
async fn a_missed_expectation_is_a_note_and_never_an_assertion() {
    let r = rig(
        settings_app(),
        r#"{ "steps": [
            { "op": "tap", "target": "Network & internet", "expect": { "appears": "Saved" } },
            { "op": "done", "outcome": "success", "reason": "open" }
          ],
          "check": { "holds": true, "evidence": ["SIMs"], "reason": "listed" } }"#,
    );
    let mut job = r.job(opts("open Network & internet"));
    job.replays = 1;
    let result = job.run().await.unwrap();
    assert_eq!(
        result.report.outcome,
        Outcome::Success,
        "{}",
        result.report.reason
    );
    assert_eq!(result.report.expectations.not_met, 1);
    assert!(
        r.notes_at(2).iter().any(|n| matches!(
            n,
            TurnNote::ExpectedNotMet { expected, action, .. }
                if expected == "\"Saved\" to appear"
                    && action == "tap { text: \"Network & internet\" }"
        )),
        "{:?}",
        r.notes_at(2)
    );
    let history = r.planner.seen_history.lock().unwrap().clone();
    assert!(
        history[1].contains("(expected \"Saved\" to appear: not met)"),
        "{history:?}"
    );
    assert!(r.event_json().contains(r#""kind":"expectedNotMet""#));
    let yaml = result.yaml.unwrap();
    assert!(!yaml.contains("Saved"), "{yaml}");
    assert!(result.replay.unwrap().all_passed());
}

#[tokio::test(start_paused = true)]
async fn missed_expectations_count_toward_stuck() {
    let r = rig(
        settings_app(),
        r#"{ "steps": [
            { "op": "tap", "target": "Network & internet", "expect": { "appears": "Saved" } },
            { "op": "back", "expect": { "appears": "Saved" } },
            { "op": "tap", "target": "Network & internet", "expect": { "appears": "Saved" } },
            { "op": "back", "expect": { "appears": "Saved" } },
            { "op": "back", "expect": { "appears": "Saved" } },
            { "op": "tap", "target": "Network & internet", "expect": { "appears": "Saved" } },
            { "op": "back", "expect": { "appears": "Saved" } },
            { "op": "done", "outcome": "giveUp", "reason": "x" }
        ] }"#,
    );
    let mut o = opts("save it");
    o.verify = VerifyLevel::Off;
    // The loop rule would end this first (the same two screens, nothing
    // learned): this test is about the wander count.
    o.max_loops = 10;
    let report = r.session(o).run().await.unwrap();
    // Each action changed the screen but missed: wandering, which is
    // allowed twice the stuck limit (3) before the run stops. (The second
    // back is refused once: back from that page already led round.)
    assert_eq!(report.outcome, Outcome::Stuck, "{}", report.reason);
    assert!(
        report
            .reason
            .contains("missed what they were expected to do"),
        "{}",
        report.reason
    );
    assert_eq!(report.expectations.not_met, 6);
}

#[tokio::test(start_paused = true)]
async fn model_text_matching_nothing_is_never_a_selector() {
    let r = rig(
        login_app(),
        r#"{ "steps": [
            { "op": "inputText", "target": "Email", "value": "EMAIL", "expect": { "appears": "EMAIL" } },
            { "op": "tap", "target": "Sign in", "expect": { "gone": "Forgot password" } },
            { "op": "done", "outcome": "success", "reason": "signed in" }
          ],
          "check": { "holds": true, "evidence": ["Welcome back"], "reason": "welcome" } }"#,
    );
    let mut o = opts("sign in");
    o.data = vec![("EMAIL".into(), "alice@example.com".into())];
    let mut job = r.job(o);
    job.replays = 1;
    let result = job.run().await.unwrap();
    assert_eq!(
        result.report.outcome,
        Outcome::Success,
        "{}",
        result.report.reason
    );
    // A value's name and an element not on the list: not checkable.
    let e = result.report.expectations;
    assert_eq!((e.met, e.not_met, e.not_checkable), (0, 0, 2));
    let yaml = result.yaml.unwrap();
    assert!(!yaml.contains("Forgot"), "{yaml}");
    assert!(!yaml.contains("assertNotVisible"), "{yaml}");
    assert!(!yaml.contains("alice@"), "{yaml}");
}

#[tokio::test(start_paused = true)]
async fn gone_and_checked_expectations_become_not_visible_and_state_assertions() {
    let app = FakeApp::new()
        .screen(
            "home",
            vec![
                Node::text(TV, "Settings").id("com.android.settings:id/homepage_title"),
                Node::text(TV, "Network & internet"),
                Node::text(TV, "Battery"),
            ],
        )
        .screen(
            "network",
            vec![
                Node::text(TV, "Network & internet")
                    .id("com.android.settings:id/collapsing_toolbar"),
                Node::text("android.widget.Switch", "Wi-Fi")
                    .id("wifi_switch")
                    .checkable(false),
            ],
        )
        .link("home", "Network & internet", "network");
    let r = rig(
        app,
        r#"{ "steps": [
            { "op": "tap", "target": "Network & internet", "expect": { "gone": "Battery" } },
            { "op": "tap", "target": "Wi-Fi", "expect": { "checked": "Wi-Fi" } },
            { "op": "done", "outcome": "success", "reason": "on" }
          ],
          "check": { "holds": true, "evidence": ["Wi-Fi"], "reason": "on" } }"#,
    );
    let mut job = r.job(opts("turn on Wi-Fi"));
    job.replays = 2;
    let result = job.run().await.unwrap();
    assert_eq!(
        result.report.outcome,
        Outcome::Success,
        "{}",
        result.report.reason
    );
    assert_eq!(result.report.expectations.met, 2);
    let yaml = result.yaml.unwrap();
    assert!(
        yaml.contains("assertNotVisible:\n      text: Battery"),
        "{yaml}"
    );
    assert!(yaml.contains("checked: true"), "{yaml}");
    let replay = result.replay.unwrap();
    assert!(replay.all_passed(), "{replay:?}\n{yaml}");
}

// Row 1.5 (AI-021): `--after login.yaml` replays the login with no model,
// the model never sees the secret, the test starts with the login by name,
// and it replays.
#[tokio::test(start_paused = true)]
async fn after_a_stored_login_the_model_starts_logged_in_and_never_sees_the_secret() {
    const PASSWORD: &str = "vel15b-correct-horse";
    let mut env = velocity_common::env::settings_snapshot();
    env.insert("VEL15B_EMAIL".into(), "alice@example.com".into());
    env.insert("VEL15B_PASSWORD".into(), PASSWORD.into());
    velocity_common::env::set_settings(env);
    let login = r#"
appId: com.android.settings
tests:
  - name: login
    steps:
      - inputText: { selector: { id: email }, text: "${VEL15B_EMAIL}" }
      - inputText: { selector: { id: password }, text: "${VEL15B_PASSWORD}" }
      - tap: { id: submit }
      - assertVisible: { text: "Welcome back" }
"#;
    let prefix =
        velocity_explore::after::login_prefix(login, None, &std::collections::HashMap::new())
            .unwrap();
    let r = rig(
        login_app(),
        r#"{
          "steps": [
            { "op": "assert", "target": "Balance $12.40", "reason": "the balance shows" },
            { "op": "done", "outcome": "success", "reason": "the balance is on screen" }
          ],
          "check": { "holds": true, "evidence": ["Balance $12.40"], "reason": "balance shown" }
        }"#,
    );
    let mut job = r.job(opts("check my balance"));
    job.after = prefix;
    job.replays = 1;
    let result = job.run().await.unwrap();
    assert_eq!(
        result.report.outcome,
        Outcome::Success,
        "{}",
        result.report.reason
    );
    // The login ran before the first planning turn: the model's first
    // request came on the welcome screen.
    let typed = r.app.typed();
    assert!(
        typed.contains(&("password".to_string(), PASSWORD.to_string())),
        "{typed:?}"
    );
    for seen in [
        r.planner.seen_history.lock().unwrap().join("\n"),
        r.planner.seen_memos.lock().unwrap().join("\n"),
        r.planner.seen_notes.lock().unwrap().join("\n"),
        r.planner.seen_values.lock().unwrap().join("\n"),
        r.event_json(),
        serde_json::to_string(&result.report).unwrap(),
    ] {
        assert!(!seen.contains(PASSWORD), "the secret leaked: {seen}");
    }
    let yaml = result.yaml.unwrap();
    assert!(
        yaml.contains("${VEL15B_PASSWORD}") && yaml.contains("${VEL15B_EMAIL}"),
        "{yaml}"
    );
    assert!(!yaml.contains(PASSWORD), "{yaml}");
    let replay = result.replay.unwrap();
    assert!(replay.runs.iter().all(|r| r.passed), "{replay:?}");
}

// Row 1.3: the trust gates. Network & internet's test fails when the tap
// goes to Display instead (rejected), names goal text, and burns in.
#[tokio::test(start_paused = true)]
async fn the_gates_trust_a_test_that_fails_on_the_wrong_path() {
    let r = rig(settings_app(), NETWORK);
    let mut job = r.job(opts("open Network & internet"));
    job.replays = 3;
    job.gates = true;
    job.replay_jitter_ms = 5;
    let result = job.run().await.unwrap();
    let g = result.gates.clone().expect("gates ran");
    assert!(g.trusted, "{g:?}");
    assert_eq!((g.replays, g.replays_passed), (3, 3));
    assert!(
        matches!(
            &g.wrong_path,
            velocity_explore::gates::WrongPath::Rejected { .. }
        ),
        "{g:?}"
    );
    assert!(g.goal_text.is_some(), "{g:?}");
    assert!(result.ok());
}

// Two rows lead to screens that look alike: a test that only checks the
// shared text passes on the wrong path too, so it is not trusted.
#[tokio::test(start_paused = true)]
async fn the_gates_refuse_a_test_that_passes_on_the_wrong_path() {
    let app = FakeApp::new()
        .screen(
            "home",
            vec![
                Node::text(TV, "Plans"),
                Node::text(TV, "Monthly"),
                Node::text(TV, "Yearly"),
            ],
        )
        .screen(
            "monthly",
            vec![Node::text(TV, "Checkout"), Node::text(TV, "Pay now")],
        )
        .screen(
            "yearly",
            vec![Node::text(TV, "Checkout"), Node::text(TV, "Pay now")],
        )
        .link("home", "Monthly", "monthly")
        .link("home", "Yearly", "yearly");
    let r = rig(
        app,
        r#"{
          "steps": [
            { "op": "tap", "target": "Monthly" },
            { "op": "assert", "target": "Checkout", "reason": "checkout opened" },
            { "op": "done", "outcome": "success", "reason": "monthly checkout is open" }
          ],
          "check": { "holds": true, "evidence": ["Checkout"], "reason": "checkout shown" }
        }"#,
    );
    let mut job = r.job(opts("buy the monthly plan"));
    job.replays = 2;
    job.gates = true;
    job.replay_jitter_ms = 0;
    let result = job.run().await.unwrap();
    let g = result.gates.clone().expect("gates ran");
    assert!(!g.trusted, "{g:?}");
    assert!(
        matches!(
            &g.wrong_path,
            velocity_explore::gates::WrongPath::Passed { .. }
        ),
        "{g:?}"
    );
    assert!(g.reasons.iter().any(|r| r.contains("wrong path")), "{g:?}");
    assert!(!result.ok());
}

// Row 1.3: `velocity gate` reuses the same gates as `velocity explore`, on a
// file that was never explored (or was exported from a session).
#[tokio::test(start_paused = true)]
async fn gate_trusts_a_one_test_file_that_fails_on_the_wrong_path() {
    let r = rig(settings_app(), NETWORK);
    let mut job = r.job(opts("open Network & internet"));
    // replays = 0: explore writes the YAML without gating it.
    job.replays = 0;
    let result = job.run().await.unwrap();
    let yaml = result.yaml.clone().expect("a test is written");

    let opts = gates::CheckOptions {
        device_id: "fake-1",
        platform: Platform::Android,
        overrides: &HashMap::new(),
        config: &FakeApp::config(),
        replays: 3,
        jitter_ms: 5,
    };
    let checked = gates::check(&*r.app, &yaml, "open Network & internet", &opts)
        .await
        .unwrap();
    assert!(checked.gates.trusted, "{:?}", checked.gates);
    assert_eq!(
        (checked.gates.replays, checked.gates.replays_passed),
        (3, 3)
    );
    assert!(
        matches!(checked.gates.wrong_path, gates::WrongPath::Rejected { .. }),
        "{:?}",
        checked.gates
    );
    assert!(checked.gates.goal_text.is_some(), "{:?}", checked.gates);
}

#[tokio::test(start_paused = true)]
async fn gate_rejects_a_goal_with_no_shared_words() {
    let r = rig(settings_app(), NETWORK);
    let mut job = r.job(opts("open Network & internet"));
    job.replays = 0;
    let result = job.run().await.unwrap();
    let yaml = result.yaml.clone().expect("a test is written");

    let opts = gates::CheckOptions {
        device_id: "fake-1",
        platform: Platform::Android,
        overrides: &HashMap::new(),
        config: &FakeApp::config(),
        replays: 3,
        jitter_ms: 5,
    };
    let checked = gates::check(&*r.app, &yaml, "zzz qqq", &opts)
        .await
        .unwrap();
    assert!(!checked.gates.trusted, "{:?}", checked.gates);
    assert!(
        checked
            .gates
            .reasons
            .iter()
            .any(|r| r.contains("goal text")),
        "{:?}",
        checked.gates
    );
}

#[tokio::test(start_paused = true)]
async fn gate_errors_on_a_two_test_file() {
    let r = rig(settings_app(), NETWORK);
    let yaml = "appId: a\ntests:\n  - name: one\n    steps:\n      - tap: { text: X }\n  - name: two\n    steps:\n      - tap: { text: Y }\n";
    let opts = gates::CheckOptions {
        device_id: "fake-1",
        platform: Platform::Android,
        overrides: &HashMap::new(),
        config: &FakeApp::config(),
        replays: 3,
        jitter_ms: 5,
    };
    let err = gates::check(&*r.app, yaml, "open X", &opts)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("one test"), "{err}");
}

#[tokio::test(start_paused = true)]
async fn gate_errors_on_zero_replays() {
    let r = rig(settings_app(), NETWORK);
    let yaml = "appId: a\ntests:\n  - name: t\n    steps:\n      - tap: { text: X }\n";
    let opts = gates::CheckOptions {
        device_id: "fake-1",
        platform: Platform::Android,
        overrides: &HashMap::new(),
        config: &FakeApp::config(),
        replays: 0,
        jitter_ms: 5,
    };
    let err = gates::check(&*r.app, yaml, "open X", &opts)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("replays"), "{err}");
}

#[tokio::test(start_paused = true)]
async fn gate_refuses_a_test_that_passes_on_the_wrong_path() {
    let app = FakeApp::new()
        .screen(
            "home",
            vec![
                Node::text(TV, "Plans"),
                Node::text(TV, "Monthly"),
                Node::text(TV, "Yearly"),
            ],
        )
        .screen(
            "monthly",
            vec![Node::text(TV, "Checkout"), Node::text(TV, "Pay now")],
        )
        .screen(
            "yearly",
            vec![Node::text(TV, "Checkout"), Node::text(TV, "Pay now")],
        )
        .link("home", "Monthly", "monthly")
        .link("home", "Yearly", "yearly");
    let r = rig(
        app,
        r#"{
          "steps": [
            { "op": "tap", "target": "Monthly" },
            { "op": "assert", "target": "Checkout", "reason": "checkout opened" },
            { "op": "done", "outcome": "success", "reason": "monthly checkout is open" }
          ],
          "check": { "holds": true, "evidence": ["Checkout"], "reason": "checkout shown" }
        }"#,
    );
    let mut job = r.job(opts("buy the monthly plan"));
    job.replays = 0;
    let result = job.run().await.unwrap();
    let yaml = result.yaml.clone().expect("a test is written");

    let opts = gates::CheckOptions {
        device_id: "fake-1",
        platform: Platform::Android,
        overrides: &HashMap::new(),
        config: &FakeApp::config(),
        replays: 3,
        jitter_ms: 5,
    };
    let checked = gates::check(&*r.app, &yaml, "buy the monthly plan", &opts)
        .await
        .unwrap();
    assert!(!checked.gates.trusted, "{:?}", checked.gates);
    assert!(
        matches!(checked.gates.wrong_path, gates::WrongPath::Passed { .. }),
        "{:?}",
        checked.gates
    );
}

#[tokio::test(start_paused = true)]
async fn the_same_loop_with_nothing_learned_ends_the_run_but_a_reading_loop_does_not() {
    let walk = |memo: bool| {
        let m = |s: &str| if memo { s.to_string() } else { String::new() };
        format!(
            r#"{{ "steps": [
            {{ "op": "tap", "target": "Network & internet", "memo": "{}" }},
            {{ "op": "back" }},
            {{ "op": "tap", "target": "Network & internet", "memo": "{}" }},
            {{ "op": "back" }},
            {{ "op": "back" }},
            {{ "op": "tap", "target": "Network & internet", "memo": "{}" }},
            {{ "op": "back" }},
            {{ "op": "tap", "target": "Network & internet", "memo": "{}" }},
            {{ "op": "done", "outcome": "giveUp", "reason": "x" }}
        ] }}"#,
            m("read 1"),
            m("read 2"),
            m("read 3"),
            m("read 4")
        )
    };
    let r = rig(settings_app(), &walk(false));
    let mut o = opts("look around");
    o.verify = VerifyLevel::Off;
    let report = r.session(o).run().await.unwrap();
    assert_eq!(report.outcome, Outcome::Stuck, "{}", report.reason);
    assert!(report.reason.contains("same loop"), "{}", report.reason);
    assert!(report.revisit_turns > 0);

    // The same walk, reading something into the memo each time round: no
    // back is refused and no loop ends the run.
    let r = rig(settings_app(), &walk(true));
    let mut o = opts("look around");
    o.verify = VerifyLevel::Off;
    let report = r.session(o).run().await.unwrap();
    assert_eq!(report.outcome, Outcome::GaveUp, "{}", report.reason);
    assert!(!r.all_notes().contains(&TurnNote::BackLedRound));
    assert_eq!(report.revisit_turns, 0);
}

#[tokio::test(start_paused = true)]
async fn an_end_state_shown_only_by_the_log_holds_without_assertions_from_the_screen() {
    let r = rig(
        settings_app(),
        r#"{ "steps": [
            { "op": "tap", "target": "Network & internet" },
            { "op": "back" },
            { "op": "done", "outcome": "success", "reason": "it was opened" }
          ],
          "check": { "holds": true, "evidence_turns": [1], "reason": "turn 1 opened it" } }"#,
    );
    let mut o = opts("open Network & internet, then go back");
    o.verify = VerifyLevel::Final;
    let report = r.session(o).run().await.unwrap();
    assert_eq!(report.outcome, Outcome::Success, "{}", report.reason);
    assert_eq!(report.final_basis, Some("log"));
    assert!(
        report.final_assertions.is_empty(),
        "{:?}",
        report.final_assertions
    );
    assert!(
        report.reason.contains("holds by the log"),
        "{}",
        report.reason
    );
    // Citing a turn the log doesn't have is no evidence at all.
    let r = rig(
        settings_app(),
        r#"{ "steps": [
            { "op": "tap", "target": "Network & internet" },
            { "op": "done", "outcome": "success", "reason": "x" },
            { "op": "done", "outcome": "success", "reason": "x" }
          ],
          "check": { "holds": true, "evidence_turns": [42], "reason": "trust me" } }"#,
    );
    let mut o = opts("open Network & internet");
    o.verify = VerifyLevel::Final;
    let report = r.session(o).run().await.unwrap();
    assert_eq!(report.outcome, Outcome::Unverified, "{}", report.reason);
}

#[tokio::test(start_paused = true)]
async fn a_screen_answer_still_on_the_final_screen_is_asserted() {
    let r = rig(
        settings_app(),
        r#"{ "steps": [
            { "op": "answer", "target": "Battery", "value": "Battery" },
            { "op": "done", "outcome": "success", "reason": "answered" }
          ],
          "check": { "holds": true, "evidence": ["Apps"], "reason": "listed" } }"#,
    );
    let mut o = opts("Which settings entry comes after Notifications?");
    o.verify = VerifyLevel::Final;
    let report = r.session(o).run().await.unwrap();
    assert_eq!(report.outcome, Outcome::Success, "{}", report.reason);
    assert_eq!(report.final_basis, Some("screen"));
    assert!(
        report
            .final_assertions
            .contains(&velocity_common::Selector::Text("Battery".into())),
        "{:?}",
        report.final_assertions
    );
}
