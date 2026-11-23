// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

use std::sync::Arc;
use std::time::Duration;

use atalanta_common::{Platform, PlatformDriver};
use atalanta_inspector::console::{
    self, mjpeg, ConsoleHooks, FrameFeed, ImageFrame, LeaseFail, Leases, LiveSource,
};
use atalanta_inspector::InspectorServer;
use clap::Args;
use colored::Colorize;

#[derive(Args)]
pub struct InspectArgs {
    /// Only this platform's devices (ios, android or web). Default: every
    /// platform's, in one console
    #[arg(short, long)]
    platform: Option<String>,

    /// Port to serve the inspector UI
    #[arg(long, default_value_t = 9876)]
    port: u16,

    /// Address to listen on. Defaults to loopback; anything else (e.g.
    /// 0.0.0.0) exposes the device to the network, still behind the
    /// per-session token in the printed URL
    #[arg(long, default_value = "127.0.0.1")]
    host: std::net::IpAddr,

    /// Open the console in the default browser
    #[arg(long)]
    open: bool,

    /// Directory recorded tests are saved in (new files only).
    /// `--flows-dir` is the old name
    #[arg(
        long = "tests-dir",
        alias = "flows-dir",
        default_value = "flows",
        value_name = "DIR"
    )]
    flows_dir: std::path::PathBuf,

    /// Artifacts folder the screenshot review page (`/review`) reads; console
    /// runs write theirs under <results-dir>/console/
    #[arg(long, default_value = "atalanta-results")]
    results_dir: std::path::PathBuf,

    /// Target device ID (auto-detects if omitted)
    #[arg(short, long)]
    device: Option<String>,

    /// App identifier/package name for performance profiling in the inspector
    #[arg(long)]
    app_id: Option<String>,

    /// Page the web device opens when it starts
    #[arg(long, value_name = "URL")]
    url: Option<String>,
}

/// The live paths of the real drivers.
struct CliLive {
    android: Option<Arc<atalanta_android::AndroidDriver>>,
    ios: Option<Arc<atalanta_ios::IosDriver>>,
    web: Option<Arc<atalanta_web::WebDriver>>,
}

#[async_trait::async_trait]
impl LiveSource for CliLive {
    async fn h264(
        &self,
        device_id: &str,
    ) -> Option<Result<Arc<atalanta_android::video::VideoStream>, String>> {
        if Platform::of_device_id(device_id) != Platform::Android || device_id.starts_with("web") {
            return None;
        }
        Some(self.android.as_ref()?.video_stream(device_id).await)
    }

    async fn frames(&self, device_id: &str) -> Option<Result<FrameFeed, String>> {
        match Platform::of_device_id(device_id) {
            Platform::Ios => {
                let ios = self.ios.as_ref()?;
                let urls = match ios.mjpeg_urls(device_id).await {
                    Ok(u) if !u.is_empty() => u,
                    Ok(_) => return None,
                    Err(e) => return Some(Err(e.to_string())),
                };
                let mut why = Vec::new();
                for url in urls {
                    match mjpeg::open(&url, Duration::from_secs(3), "ios-mjpeg").await {
                        Ok(feed) => return Some(Ok(feed)),
                        Err(e) => why.push(e),
                    }
                }
                Some(Err(why.join("; ")))
            }
            _ if device_id == "web" || device_id.starts_with("web-") => {
                let web = self.web.as_ref()?;
                let mut cast = match web.live_screencast(device_id, 1280).await {
                    Ok(c) => c,
                    Err(e) => return Some(Err(e.to_string())),
                };
                let (tx, rx) = tokio::sync::mpsc::channel(2);
                let task = tokio::spawn(async move {
                    while let Some(f) = cast.frames.recv().await {
                        let frame = ImageFrame::jpeg(f.jpeg, f.width, f.height);
                        if tx.send(frame).await.is_err() {
                            break;
                        }
                    }
                    drop(cast);
                });
                Some(Ok(FrameFeed {
                    frames: rx,
                    source: "web-screencast",
                    guard: Box::new(AbortOnDrop(task)),
                }))
            }
            _ => None,
        }
    }

    async fn still(&self, device_id: &str) -> Option<ImageFrame> {
        if Platform::of_device_id(device_id) != Platform::Android || device_id.starts_with("web") {
            return None;
        }
        let jpeg = self.android.as_ref()?.screenshot_jpeg(device_id).await?;
        Some(ImageFrame::jpeg(jpeg, 0, 0))
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// `atalanta run`'s cross-process device leases.
struct CliLeases;
impl Leases for CliLeases {
    fn try_lock(&self, device_id: &str) -> Result<Box<dyn std::any::Any + Send + Sync>, LeaseFail> {
        match atalanta_runner::lease::try_lock(device_id) {
            Ok(lock) => Ok(Box::new(lock)),
            Err(atalanta_runner::lease::LeaseError::Held(who)) => Err(LeaseFail::Held(who)),
            Err(atalanta_runner::lease::LeaseError::Io(e)) => Err(LeaseFail::Io(e)),
        }
    }
}

fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    if let Err(e) = std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        eprintln!("could not open a browser ({opener}: {e}); open the URL above yourself");
    }
}

pub async fn execute(args: InspectArgs) -> anyhow::Result<i32> {
    let only = match args.platform.as_deref() {
        Some("ios") => Some(Platform::Ios),
        Some("android") => Some(Platform::Android),
        Some("web") => Some(Platform::Web),
        Some(other) => usage_bail!("Unknown platform '{other}'. Use 'ios', 'android' or 'web'."),
        None => None,
    };

    // Bind first, so "running" is only printed once it is, and a port in
    // use fails at once, before any device work (VEL-171, VEL-173).
    let listener = InspectorServer::bind_on(args.host, args.port).await?;
    let addr = listener.local_addr()?;

    let wants = |p: Platform| only.is_none_or(|o| o == p);
    let android =
        wants(Platform::Android).then(|| Arc::new(atalanta_android::AndroidDriver::new()));
    let ios = wants(Platform::Ios).then(|| Arc::new(atalanta_ios::IosDriver::new()));
    let web = wants(Platform::Web).then(|| {
        Arc::new(atalanta_web::WebDriver::new(
            atalanta_web::WebDriverConfig {
                start_url: args.url.clone(),
                ..Default::default()
            },
        ))
    });
    let mut drivers: Vec<(Platform, Arc<dyn PlatformDriver>)> = Vec::new();
    if let Some(d) = &android {
        drivers.push((Platform::Android, d.clone()));
    }
    if let Some(d) = &ios {
        drivers.push((Platform::Ios, d.clone()));
    }
    if let Some(d) = &web {
        drivers.push((Platform::Web, d.clone()));
    }
    let driver: Arc<dyn PlatformDriver> = Arc::new(console::routing::RoutingDriver::new(drivers));

    let device_id = match args.device {
        Some(id) => Some(id),
        None => {
            let devices = driver.list_devices().await?;
            // A booted phone first; the web browser only if asked for.
            let booted = devices.iter().find(|d| {
                d.state == atalanta_common::DeviceState::Booted
                    && (d.platform != Platform::Web || only == Some(Platform::Web))
            });
            match booted {
                Some(d) => {
                    eprintln!(
                        "{} Auto-selected device: {} ({})",
                        "=>".cyan().bold(),
                        d.name,
                        d.id
                    );
                    Some(d.id.clone())
                }
                None => {
                    eprintln!(
                        "{} No booted device found — pick one in the console",
                        "=>".yellow().bold(),
                    );
                    None
                }
            }
        }
    };

    // Bootstrap platform services (e.g. WDA on iOS)
    if let Some(ref id) = device_id {
        eprintln!(
            "{} Preparing device (bootstrapping platform services)...",
            "=>".cyan().bold(),
        );
        driver.prepare(id).await?;
    }

    if !args.host.is_loopback() {
        eprintln!(
            "{} Listening on {addr}: anyone who can reach it and has the URL below can drive the device",
            "=>".yellow().bold(),
        );
    }

    let project_dir = std::env::current_dir()?;
    let hooks = ConsoleHooks {
        live: Arc::new(CliLive {
            android: android.clone(),
            ios: ios.clone(),
            web: web.clone(),
        }),
        leases: Arc::new(CliLeases),
        diagnostics: Arc::new(Doctor {
            opts: atalanta_doctor::Options {
                cwd: project_dir.clone(),
                ..Default::default()
            },
        }),
        explore: Arc::new(console::tasks::ExploreChild {
            launcher: Arc::new(console::tasks::AtalantaBinary::current()),
        }),
        launcher: Arc::new(console::tasks::AtalantaBinary::current()),
        runs_dir: project_dir.join(&args.results_dir).join("console"),
        project_dir,
    };
    let mut server = InspectorServer::new(driver.clone(), device_id.clone(), args.app_id)
        .with_flows_dir(args.flows_dir)
        .with_results_dir(args.results_dir)
        .with_console(hooks);
    if let Some(id) = &device_id {
        server = server.with_prepared(id);
    }

    let url = server.url(addr);
    eprintln!(
        "{} Inspector running at {}",
        "=>".cyan().bold(),
        url.underline()
    );

    eprintln!(
        "{} Screenshot review at {}",
        "=>".cyan().bold(),
        server.review_url(addr)
    );

    // D-42: a source build without frontend/dist serves the API only.
    if !atalanta_inspector::embedded::ui_available() {
        eprintln!(
            "{} {}",
            "warning:".yellow().bold(),
            atalanta_inspector::embedded::MISSING_UI
        );
    }

    if args.open {
        open_browser(&url);
    }

    // Ctrl-C / SIGTERM: stop serving, stop the console's run and explore
    // children (they run in their own process groups, so the terminal's
    // signal never reaches them), then clean the drivers up. A second
    // signal kills what is left and exits at once (`signals`).
    crate::signals::install();
    let seen = crate::signals::count();
    let shutdown = async move {
        crate::signals::after(seen).await;
        crate::signals::cleaning_up();
    };
    let served = server.start_with_listener_until(listener, shutdown).await;
    driver.cleanup().await;
    served?;

    if crate::signals::count() > seen {
        return Ok(crate::signals::exit_code());
    }
    Ok(0)
}

/// The console's checks: `atalanta doctor`'s, in this process (the same
/// report `atalanta doctor --json` prints).
struct Doctor {
    opts: atalanta_doctor::Options,
}

#[async_trait::async_trait]
impl console::Diagnostics for Doctor {
    async fn run(&self) -> console::diagnostics::DiagResult {
        use console::diagnostics::DiagResult;
        let report = Box::pin(atalanta_doctor::run_checks(&self.opts)).await;
        match serde_json::to_value(&report) {
            Ok(v) => DiagResult::Report(v),
            Err(e) => DiagResult::Failed(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use atalanta_inspector::console::diagnostics::DiagResult;
    use atalanta_inspector::console::Diagnostics;

    /// The console's diagnostics are the doctor's report, as JSON with the
    /// fields the page reads.
    #[tokio::test]
    async fn diagnostics_are_the_doctor_report() {
        let dir = std::env::temp_dir().join(format!("vel-inspect-doctor-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let env = std::collections::HashMap::from([
            ("PATH".to_string(), dir.display().to_string()),
            ("HOME".to_string(), dir.display().to_string()),
        ]);
        let d = super::Doctor {
            opts: atalanta_doctor::Options {
                cwd: dir.clone(),
                env: atalanta_doctor::Env::from_vars(env),
                areas: vec![atalanta_doctor::Area::Android],
                ..Default::default()
            },
        };
        let DiagResult::Report(v) = d.run().await else {
            panic!("a report");
        };
        assert!(v["verdict"].is_string(), "{v}");
        let checks = v["checks"].as_array().unwrap();
        assert!(!checks.is_empty(), "{v}");
        for c in checks {
            assert!(c["id"].is_string() && c["status"].is_string() && c["summary"].is_string());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
