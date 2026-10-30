//! End to end against the fake app: a scripted planner plays the model,
//! the real executor acts, the YAML is written and replayed.

use std::sync::{Arc, Mutex};

use velocity_common::{Platform, VerifyLevel};
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

#[tokio::test]
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

#[tokio::test]
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

#[tokio::test]
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

#[tokio::test]
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

#[tokio::test]
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

#[tokio::test]
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
    let notes = r.planner.seen_notes.lock().unwrap();
    assert!(notes[2].contains("did not change the screen"), "{notes:?}");
}

#[tokio::test]
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

#[tokio::test]
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
    let notes = r.planner.seen_notes.lock().unwrap();
    assert!(notes[2].contains("did not confirm"), "{notes:?}");
}

#[tokio::test]
async fn verify_without_a_checker_is_a_config_error() {
    let r = rig(settings_app(), r#"{ "steps": [] }"#);
    let err = r.session(opts("x")).run().await.unwrap_err();
    assert_eq!(err.exit_code(), 2, "{err}");
}

#[tokio::test]
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

#[tokio::test]
async fn a_stopped_session_says_so() {
    let r = rig(settings_app(), NETWORK);
    let session = r.session(opts("x"));
    session.handle().stop();
    let report = session.run().await.unwrap();
    assert_eq!(report.outcome, Outcome::Stopped);
    assert_eq!(r.planner.usage().calls, 0);
}

#[tokio::test]
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

#[tokio::test]
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

#[tokio::test]
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

#[tokio::test]
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

#[tokio::test]
async fn an_answer_must_be_grounded_and_then_ends_the_run_as_success() {
    let r = rig(
        settings_app(),
        r#"{
          "steps": [
            { "op": "answer", "value": "Bluetooth", "memo": "looking at the list" },
            { "op": "answer", "value": "Connected devices", "target": "Connected devices" },
            { "op": "done", "outcome": "success", "reason": "answered" }
          ],
          "check": { "holds": false, "reason": "a question can't be checked on the screen" }
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
    // The checker was not asked: the answer was grounded by code.
    assert!(!ev.contains("\"checked\""), "{ev}");
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

#[tokio::test]
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
    // Every turn carries the benchmark note.
    assert!(notes
        .iter()
        .all(|n| n.contains("judged on the device's final state")));
    let history = r.planner.seen_history.lock().unwrap().join("\n");
    assert!(history.contains("openApp \"Settings\""), "{history}");
    assert!(history.contains("pressKey home"), "{history}");
    assert!(history.contains("pressKey enter"), "{history}");
}

#[tokio::test]
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

#[tokio::test]
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

#[tokio::test]
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

#[tokio::test]
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
    let notes = r.planner.seen_notes.lock().unwrap().clone();
    assert_eq!(notes.len(), 5, "{notes:?}");
    // Turn 1 scrolled up at the top of the list: nothing moved.
    assert!(
        notes[1].contains("(scroll up) had no visible effect: the content did not move"),
        "{notes:?}"
    );
    // Turns 2 and 3 went somewhere and came back.
    assert!(!notes[2].contains("no visible effect"), "{notes:?}");
    assert!(
        notes[3].contains("You are back on a screen you saw 2 turns ago (at turn 2)"),
        "{notes:?}"
    );
    // Turn 4 tapped a row that leads nowhere.
    assert!(
        notes[4].contains("(tap { text: \"Apps\" }) had no visible effect"),
        "{notes:?}"
    );
    assert_eq!(
        notes.iter().filter(|n| n.contains("You are back")).count(),
        1,
        "{notes:?}"
    );
    assert!(r.event_json().contains("had no visible effect"));
}

// Expected post-conditions (Q-Reflector): the model says what should hold
// after its action; code checks it on the next screen.

#[tokio::test]
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

#[tokio::test]
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
    let notes = r.planner.seen_notes.lock().unwrap().clone();
    assert!(
        notes[1].contains("You expected \"Saved\" to appear after tap { text: \"Network & internet\" }, but no element on the screen now shows it"),
        "{notes:?}"
    );
    let history = r.planner.seen_history.lock().unwrap().clone();
    assert!(
        history[1].contains("(expected \"Saved\" to appear: not met)"),
        "{history:?}"
    );
    assert!(r.event_json().contains("You expected \\\"Saved\\\""));
    let yaml = result.yaml.unwrap();
    assert!(!yaml.contains("Saved"), "{yaml}");
    assert!(result.replay.unwrap().all_passed());
}

#[tokio::test]
async fn missed_expectations_count_toward_stuck() {
    let r = rig(
        settings_app(),
        r#"{ "steps": [
            { "op": "tap", "target": "Network & internet", "expect": { "appears": "Saved" } },
            { "op": "back", "expect": { "appears": "Saved" } },
            { "op": "tap", "target": "Network & internet", "expect": { "appears": "Saved" } },
            { "op": "done", "outcome": "giveUp", "reason": "x" }
        ] }"#,
    );
    let mut o = opts("save it");
    o.verify = VerifyLevel::Off;
    let report = r.session(o).run().await.unwrap();
    assert_eq!(report.outcome, Outcome::Stuck, "{}", report.reason);
    assert!(
        report
            .reason
            .contains("missed what they were expected to do"),
        "{}",
        report.reason
    );
    assert_eq!(report.expectations.not_met, 3);
}

#[tokio::test]
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

#[tokio::test]
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
