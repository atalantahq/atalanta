//! Environment diagnostics: the probes behind `velocity doctor`.
//!
//! One typed API so the CLI and the MCP server (`diagnose`) report the same
//! checks the same way:
//!
//! ```no_run
//! # async fn demo() {
//! use velocity_doctor::{run_checks, apply_safe_fixes, Options};
//! let opts = Options::default();
//! let report = run_checks(&opts).await;
//! println!("{}", report.render(false));
//! if report.has_safe_fixes() {
//!     for outcome in apply_safe_fixes(&report, &opts).await {
//!         println!("{}: {}", outcome.check_id, outcome.message);
//!     }
//! }
//! # }
//! ```
//!
//! - **Read-only by default.** Checks only look: list devices and apps,
//!   read versions and files. The one exception is `--deep`'s tree-server
//!   probe, which pushes a jar to `/data/local/tmp` and, when nobody else
//!   holds the device's lease, starts the server for one PING.
//! - **Bounded.** Every external command runs with a deadline
//!   ([`Options::timeout`], devicectl a little longer) and each area as a
//!   whole is capped, so a wedged `adb` or CoreDevice can't hang the run.
//!   Areas run concurrently; a typical run takes 1-3 s.
//! - **Secrets stay out.** The AI area reports which provider keys are
//!   set by NAME only; no value is read into a check.
//! - **Fixes are data.** A [`Fix`] is a command to run or guidance to
//!   follow. Only fixes with `safe_auto` (a [`SafeAction`]) are applied by
//!   [`apply_safe_fixes`]: restarting a wedged adb server, reconnecting
//!   offline devices, removing dead-pid lease files, re-pushing the tree
//!   server. Nothing interactive, nothing needing sudo.
//!
//! The verdict: any `Fail` → [`Verdict::Blocked`]; else any `Warn` →
//! [`Verdict::Degraded`]; else [`Verdict::Ready`]. [`Report::exit_code`]
//! maps it onto Velocity's exit codes (2 config, 3 device/environment).
#![forbid(unsafe_code)]

mod ai;
mod android;
mod env;
mod fixes;
mod host;
mod ios;
mod project;
mod render;
mod web;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub use env::Env;
pub use fixes::{apply_safe_fixes, FixOutcome};
pub use velocity_common::Platform;

/// The group a check belongs to (also the order they print in).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Area {
    Host,
    Android,
    Ios,
    Web,
    Ai,
    Project,
}

impl Area {
    pub fn title(self) -> &'static str {
        match self {
            Self::Host => "Host",
            Self::Android => "Android",
            Self::Ios => "iOS",
            Self::Web => "Web",
            Self::Ai => "AI",
            Self::Project => "Project",
        }
    }
}

/// One check's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Works.
    Ok,
    /// Works with a caveat, or something optional is missing.
    Warn,
    /// Something Velocity needs here is broken or missing.
    Fail,
    /// Not checked: not applicable here, or needs `--deep`.
    Skip,
}

/// The whole report's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// Every check passed (or was skipped).
    Ready,
    /// Usable, with warnings.
    Degraded,
    /// At least one check failed.
    Blocked,
}

/// What to do about a check that isn't `Ok`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fix {
    /// A command (`Run`) or advice (`Guidance`); serialized as
    /// `"kind": "run" | "guidance", "value": "…"`.
    #[serde(flatten)]
    pub kind: FixKind,
    /// `velocity doctor --fix` may apply it without asking: it is
    /// non-interactive, needs no privileges, and can't lose user data.
    pub safe_auto: bool,
    /// How `--fix` applies it (only when `safe_auto`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<SafeAction>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "lowercase")]
pub enum FixKind {
    /// A shell command that fixes it.
    Run(String),
    /// What to do, in words.
    Guidance(String),
}

/// A fix [`apply_safe_fixes`] knows how to apply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SafeAction {
    /// `adb kill-server; adb start-server`.
    RestartAdbServer,
    /// `adb reconnect offline`.
    ReconnectOffline,
    /// Remove these lease files (each re-checked under its lock first).
    RemoveStaleLeases { paths: Vec<PathBuf> },
    /// Remove the tree-server jar from the device and push it again.
    RepushTreeServer { device: String },
}

impl Fix {
    pub fn run(cmd: impl Into<String>) -> Self {
        Self {
            kind: FixKind::Run(cmd.into()),
            safe_auto: false,
            action: None,
        }
    }

    pub fn guidance(text: impl Into<String>) -> Self {
        Self {
            kind: FixKind::Guidance(text.into()),
            safe_auto: false,
            action: None,
        }
    }

    /// A command `--fix` applies itself through `action`.
    pub fn safe(cmd: impl Into<String>, action: SafeAction) -> Self {
        Self {
            kind: FixKind::Run(cmd.into()),
            safe_auto: true,
            action: Some(action),
        }
    }
}

/// One diagnostic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Check {
    /// Stable id, `<area>.<name>` (`android.adb_server`).
    pub id: String,
    pub area: Area,
    pub status: Status,
    /// One line.
    pub summary: String,
    /// More: per-device lines, paths, the error text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<Fix>,
    /// Wall time of the check, ms.
    #[serde(default)]
    pub elapsed_ms: u64,
}

impl Check {
    pub fn new(id: &str, area: Area, status: Status, summary: impl Into<String>) -> Self {
        Self {
            id: id.to_string(),
            area,
            status,
            summary: summary.into(),
            detail: None,
            fix: None,
            elapsed_ms: 0,
        }
    }

    pub fn ok(id: &str, area: Area, summary: impl Into<String>) -> Self {
        Self::new(id, area, Status::Ok, summary)
    }
    pub fn warn(id: &str, area: Area, summary: impl Into<String>) -> Self {
        Self::new(id, area, Status::Warn, summary)
    }
    pub fn fail(id: &str, area: Area, summary: impl Into<String>) -> Self {
        Self::new(id, area, Status::Fail, summary)
    }
    pub fn skip(id: &str, area: Area, summary: impl Into<String>) -> Self {
        Self::new(id, area, Status::Skip, summary)
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        let d = detail.into();
        if !d.trim().is_empty() {
            self.detail = Some(d);
        }
        self
    }

    pub fn with_fix(mut self, fix: Fix) -> Self {
        self.fix = Some(fix);
        self
    }

    /// A failure of the user's configuration (a suite, the impact map, an
    /// invalid `VELOCITY_NL_PROVIDER` / model) rather than of the machine
    /// or a device: exit 2, not 3 (`VelocityError::Config` is 2 everywhere
    /// else).
    pub fn is_config(&self) -> bool {
        self.area == Area::Project || (self.id == "ai.keys" && self.status == Status::Fail)
    }
}

/// The result of [`run_checks`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub verdict: Verdict,
    /// In area order, then the order each area ran them.
    pub checks: Vec<Check>,
    pub velocity_version: String,
    pub elapsed_ms: u64,
}

impl Report {
    /// Build a report (and its verdict) from checks.
    pub fn new(mut checks: Vec<Check>, elapsed: Duration) -> Self {
        checks.sort_by_key(|c| c.area);
        Self {
            verdict: verdict(&checks),
            checks,
            velocity_version: env!("CARGO_PKG_VERSION").to_string(),
            elapsed_ms: elapsed.as_millis() as u64,
        }
    }

    /// Velocity's exit code for this report: 0 when ready, or degraded
    /// unless `strict` (then 3, or 2 when every problem is configuration);
    /// blocked: 2 when any failure is configuration (a suite, the impact
    /// map: the folder-run order puts 2 before 3), else 3.
    pub fn exit_code(&self, strict: bool) -> i32 {
        let bad = |s: Status| self.checks.iter().filter(move |c| c.status == s);
        match self.verdict {
            Verdict::Ready => 0,
            Verdict::Degraded if !strict => 0,
            Verdict::Degraded => {
                if bad(Status::Warn).all(Check::is_config) {
                    2
                } else {
                    3
                }
            }
            Verdict::Blocked => {
                if bad(Status::Fail).any(Check::is_config) {
                    2
                } else {
                    3
                }
            }
        }
    }

    pub fn count(&self, status: Status) -> usize {
        self.checks.iter().filter(|c| c.status == status).count()
    }

    /// Whether any check carries a fix `--fix` can apply.
    pub fn has_safe_fixes(&self) -> bool {
        self.checks
            .iter()
            .any(|c| c.fix.as_ref().is_some_and(|f| f.safe_auto))
    }

    pub fn get(&self, id: &str) -> Option<&Check> {
        self.checks.iter().find(|c| c.id == id)
    }

    /// The human report: one aligned line per check (`✓ ! ✗ -`), a
    /// `Run:` / `Guidance:` line under each fix, and the verdict. `color`
    /// adds ANSI colours.
    pub fn render(&self, color: bool) -> String {
        render::human(self, color)
    }
}

fn verdict(checks: &[Check]) -> Verdict {
    if checks.iter().any(|c| c.status == Status::Fail) {
        Verdict::Blocked
    } else if checks.iter().any(|c| c.status == Status::Warn) {
        Verdict::Degraded
    } else {
        Verdict::Ready
    }
}

/// What to check and how.
#[derive(Debug, Clone)]
pub struct Options {
    /// Also run the slow or device-touching probes: the tree-server push
    /// and handshake, the AI API host's reachability.
    pub deep: bool,
    /// Platforms the user means to test. A missing toolchain for one of
    /// these fails; for any other it is only skipped. Empty: none named
    /// (the project's suites still add theirs).
    pub platforms: Vec<Platform>,
    /// A suite file or folder to check. `None`: `velocity.yaml` or the
    /// `velocity/` folder in `cwd`, if there is one.
    pub project: Option<PathBuf>,
    /// The directory the project is looked for in (and state dirs that are
    /// relative, like `velocity-results/`).
    pub cwd: PathBuf,
    /// Where variables and tools are looked up.
    pub env: Env,
    /// Deadline for each external command.
    pub timeout: Duration,
    /// Only these areas (empty: all).
    pub areas: Vec<Area>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            deep: false,
            platforms: Vec::new(),
            project: None,
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            env: Env::process(),
            // Tests run fake tools, whose first exec can take seconds on a
            // loaded macOS host (the system scans new executables).
            timeout: Duration::from_secs(if cfg!(test) { 30 } else { 3 }),
            areas: Vec::new(),
        }
    }
}

impl Options {
    fn wants(&self, area: Area) -> bool {
        self.areas.is_empty() || self.areas.contains(&area)
    }

    /// The whole budget for one area: its slowest command, a few times,
    /// plus the slack some commands get on top of `timeout` (`adb devices`
    /// 2 s, devicectl 3 s). Without it, `--timeout 500ms` and a hung adb
    /// ran out the area (2 s) before the 2.5 s `adb devices` call could say
    /// "wedged" and offer the restart.
    fn area_budget(&self) -> Duration {
        self.timeout * 4
            + Duration::from_secs(3)
            + if self.deep {
                Duration::from_secs(20)
            } else {
                Duration::ZERO
            }
    }
}

/// Run every check (concurrently by area) and build the report.
pub async fn run_checks(opts: &Options) -> Report {
    let started = Instant::now();
    // The project first (cheap, local): it says which platforms matter.
    let proj = if opts.wants(Area::Project) {
        project::checks(opts).await
    } else {
        project::Findings::default()
    };
    let mut required = opts.platforms.clone();
    for p in &proj.platforms {
        if !required.contains(p) {
            required.push(*p);
        }
    }
    let ctx = Ctx {
        opts: opts.clone(),
        required,
        apps: proj.apps.clone(),
    };

    let host = bounded(Area::Host, opts.wants(Area::Host), &ctx, host::checks(&ctx));
    let android = bounded(
        Area::Android,
        opts.wants(Area::Android),
        &ctx,
        android::checks(&ctx),
    );
    let ios = bounded(Area::Ios, opts.wants(Area::Ios), &ctx, ios::checks(&ctx));
    let web = bounded(Area::Web, opts.wants(Area::Web), &ctx, web::checks(&ctx));
    let ai = bounded(Area::Ai, opts.wants(Area::Ai), &ctx, ai::checks(&ctx));
    let (host, android, ios, web, ai) = tokio::join!(host, android, ios, web, ai);

    let mut checks = proj.checks;
    let any_target = android.1 || ios.1 || web.1;
    checks.extend(host.0);
    checks.extend(android.0);
    checks.extend(ios.0);
    checks.extend(web.0);
    checks.extend(ai.0);
    if opts.wants(Area::Project) {
        let apps = tokio::time::timeout(opts.area_budget(), project::apps_installed(&ctx)).await;
        checks.extend(app_lookup(apps, opts.area_budget()));
    }
    // Nothing at all to test on is the one host-level failure.
    if opts.areas.is_empty() && !any_target {
        checks.push(
            Check::fail(
                "host.targets",
                Area::Host,
                "no device, simulator or browser to test on",
            )
            .with_fix(Fix::guidance(
                "boot an emulator or simulator (`velocity devices up --android 1` / `--ios 1`), \
                 connect a device, or install Chrome for web tests",
            )),
        );
    }
    let mut report = Report::new(checks, started.elapsed());
    report.elapsed_ms = started.elapsed().as_millis() as u64;
    report
}

/// The app-installed check, or (when its budget ran out) a check saying so:
/// dropping it made a hung device look like "no app to look for". Skip, not
/// Warn: a Project warning exits 2.
fn app_lookup(
    ran: Result<Option<Check>, tokio::time::error::Elapsed>,
    budget: Duration,
) -> Option<Check> {
    match ran {
        Ok(c) => c,
        Err(_) => Some(Check::skip(
            "project.app_installed",
            Area::Project,
            format!(
                "looking for the app on the devices did not finish in {:.0} s",
                budget.as_secs_f32()
            ),
        )),
    }
}

/// What every area sees.
pub(crate) struct Ctx {
    pub opts: Options,
    /// Platforms whose missing toolchain is a failure.
    pub required: Vec<Platform>,
    /// (platform, app id) pairs the project's suites launch.
    pub apps: Vec<(Platform, String)>,
}

impl Ctx {
    pub fn env(&self) -> &Env {
        &self.opts.env
    }
    pub fn requires(&self, p: Platform) -> bool {
        self.required.contains(&p)
    }
    /// `Fail` when `p` is required, else `Skip`.
    pub fn missing(&self, p: Platform) -> Status {
        if self.requires(p) {
            Status::Fail
        } else {
            Status::Skip
        }
    }
}

/// An area's checks and whether it has something to test on.
pub(crate) type AreaResult = (Vec<Check>, bool);

/// Run an area with its budget; a timeout becomes one failed check (the
/// commands it ran are killed with it).
async fn bounded(
    area: Area,
    wanted: bool,
    ctx: &Ctx,
    fut: impl std::future::Future<Output = AreaResult>,
) -> AreaResult {
    if !wanted {
        return (Vec::new(), false);
    }
    let budget = ctx.opts.area_budget();
    match tokio::time::timeout(budget, fut).await {
        Ok(r) => r,
        Err(_) => (
            vec![Check::fail(
                &format!("{}.timeout", area_id(area)),
                area,
                format!(
                    "{} checks did not finish in {:.0} s",
                    area.title(),
                    budget.as_secs_f32()
                ),
            )
            .with_fix(Fix::guidance(
                "a tool is not answering; run `velocity doctor` again, or check the tool by hand",
            ))],
            false,
        ),
    }
}

fn area_id(area: Area) -> &'static str {
    match area {
        Area::Host => "host",
        Area::Android => "android",
        Area::Ios => "ios",
        Area::Web => "web",
        Area::Ai => "ai",
        Area::Project => "project",
    }
}

/// Time a check: run `f` and stamp `elapsed_ms` on what it returns.
pub(crate) async fn timed<F, Fut>(f: F) -> Check
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Check>,
{
    let started = Instant::now();
    let mut c = f().await;
    c.elapsed_ms = started.elapsed().as_millis() as u64;
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(area: Area, status: Status) -> Check {
        Check::new("x.y", area, status, "s")
    }

    #[test]
    fn the_verdict_and_exit_code_follow_the_worst_check() {
        let d = Duration::ZERO;
        let ready = Report::new(
            vec![c(Area::Host, Status::Ok), c(Area::Ios, Status::Skip)],
            d,
        );
        assert_eq!(ready.verdict, Verdict::Ready);
        assert_eq!(ready.exit_code(true), 0);

        let degraded = Report::new(
            vec![c(Area::Host, Status::Ok), c(Area::Android, Status::Warn)],
            d,
        );
        assert_eq!(degraded.verdict, Verdict::Degraded);
        assert_eq!(degraded.exit_code(false), 0);
        assert_eq!(degraded.exit_code(true), 3);
        let degraded_cfg = Report::new(vec![c(Area::Project, Status::Warn)], d);
        assert_eq!(degraded_cfg.exit_code(true), 2);

        let blocked = Report::new(
            vec![c(Area::Android, Status::Fail), c(Area::Ai, Status::Warn)],
            d,
        );
        assert_eq!(blocked.verdict, Verdict::Blocked);
        assert_eq!(blocked.exit_code(false), 3);
        let blocked_cfg = Report::new(
            vec![
                c(Area::Android, Status::Fail),
                c(Area::Project, Status::Fail),
            ],
            d,
        );
        assert_eq!(blocked_cfg.exit_code(false), 2);
    }

    #[test]
    fn a_bad_ai_provider_setting_is_a_config_failure() {
        let bad = Report::new(
            vec![Check::fail(
                "ai.keys",
                Area::Ai,
                "AI settings: unknown provider",
            )],
            Duration::ZERO,
        );
        assert_eq!(bad.exit_code(false), 2);
        // Only that check: a failed AI network probe or any other area stays 3.
        let other = Report::new(
            vec![Check::fail("ai.network", Area::Ai, "x")],
            Duration::ZERO,
        );
        assert_eq!(other.exit_code(false), 3);
    }

    #[test]
    fn checks_sort_by_area_and_serialize_with_flat_fixes() {
        let r = Report::new(
            vec![
                c(Area::Project, Status::Ok),
                Check::warn("android.devices", Area::Android, "offline").with_fix(Fix::safe(
                    "adb reconnect offline",
                    SafeAction::ReconnectOffline,
                )),
                c(Area::Host, Status::Ok),
            ],
            Duration::ZERO,
        );
        let areas: Vec<Area> = r.checks.iter().map(|c| c.area).collect();
        assert_eq!(areas, [Area::Host, Area::Android, Area::Project]);
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["verdict"], "degraded");
        let fix = &json["checks"][1]["fix"];
        assert_eq!(fix["kind"], "run");
        assert_eq!(fix["value"], "adb reconnect offline");
        assert_eq!(fix["safe_auto"], true);
        assert_eq!(fix["action"]["action"], "reconnect_offline");
        let back: Report = serde_json::from_value(json).unwrap();
        assert_eq!(back, r);
    }

    /// A lookup that ran out of its budget used to vanish from the report.
    #[tokio::test]
    async fn an_app_lookup_that_runs_out_of_budget_still_shows_in_the_report() {
        let late =
            tokio::time::timeout(Duration::ZERO, std::future::pending::<Option<Check>>()).await;
        let c = app_lookup(late, Duration::from_secs(12)).expect("reported");
        assert_eq!(c.id, "project.app_installed");
        assert_eq!(c.status, Status::Skip);
        assert!(c.summary.contains("did not finish in 12 s"), "{c:?}");
        assert!(app_lookup(Ok(None), Duration::ZERO).is_none());
    }

    /// F11 at the call site: `run_checks` itself must keep the app check
    /// when the lookup outruns its budget (budget = 4 x timeout + 3 s; each
    /// `pm path` is cut at `timeout`, so enough devices make the sum exceed it).
    #[tokio::test]
    async fn run_checks_reports_a_skip_when_the_app_lookup_outruns_its_budget() {
        use crate::env::fake::Tools;
        let t = Tools::new();
        let mut devices = String::from("List of devices attached\\n");
        for i in 0..20 {
            devices.push_str(&format!("emulator-{}\\tdevice\\n", 5554 + 2 * i));
        }
        t.tool(
            "adb",
            &format!(
                "while [ \"$1\" = \"-s\" ]; do shift 2; done\n\
                 case \"$1\" in\n  devices) printf '{devices}';;\n  shell) sleep 30;;\nesac"
            ),
        );
        let proj = t.path("proj/velocity");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join("app.yaml"),
            "appId: com.example.app\nconfig:\n  platform: android\ntests:\n  - name: opens\n    steps:\n      - launchApp: {}\n",
        )
        .unwrap();
        let opts = Options {
            env: t.env(),
            cwd: t.path("proj"),
            timeout: Duration::from_millis(250),
            areas: vec![Area::Project],
            ..Options::default()
        };
        let report = run_checks(&opts).await;
        let c = report
            .get("project.app_installed")
            .unwrap_or_else(|| panic!("the app check vanished: {report:?}"));
        assert_eq!(c.status, Status::Skip, "{c:?}");
        assert!(c.summary.contains("did not finish in"), "{c:?}");
    }

    /// The wedged-adb diagnosis (and its restart fix) must survive a short
    /// `--timeout`: the area budget has to outlast `adb devices`' slack.
    #[tokio::test]
    async fn a_short_timeout_still_diagnoses_a_wedged_adb_instead_of_timing_the_area_out() {
        use crate::env::fake::Tools;
        let t = Tools::new();
        t.tool("adb", "case \"$1\" in devices) sleep 30;; esac");
        let opts = Options {
            env: t.env(),
            cwd: t.path("proj"),
            timeout: Duration::from_millis(400),
            areas: vec![Area::Android],
            ..Options::default()
        };
        let report = run_checks(&opts).await;
        assert!(report.get("android.timeout").is_none(), "{report:?}");
        let c = report.get("android.adb_server").expect("diagnosed");
        assert_eq!(c.status, Status::Fail, "{c:?}");
        assert_eq!(
            c.fix.as_ref().unwrap().action,
            Some(SafeAction::RestartAdbServer)
        );
    }
}
