//! D-48 (VEL-332): `--parallel N` on the web runs N browsers, the devices
//! `web-0` … `web-{N-1}`. It silently ran every test on one browser.
//! Needs a Chrome binary (skips without one, or with `VELOCITY_TEST_WEB=0`).
#![cfg(unix)]

use std::path::PathBuf;
use std::process::Command;

fn chrome() -> Option<String> {
    if matches!(
        std::env::var("VELOCITY_TEST_WEB").as_deref(),
        Ok("0") | Ok("false")
    ) {
        return None;
    }
    if let Some(p) = ["CHROME_PATH", "CHROME"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .find(|p| !p.is_empty())
    {
        return std::path::Path::new(&p).exists().then_some(p);
    }
    [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/snap/bin/chromium",
    ]
    .iter()
    .find(|p| std::path::Path::new(p).exists())
    .map(|p| p.to_string())
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("vwebpar-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn parallel_web_runs_one_browser_per_slot() {
    let Some(chrome) = chrome() else {
        eprintln!("skipped: no Chrome binary (or VELOCITY_TEST_WEB=0)");
        return;
    };
    let dir = scratch("two");
    std::fs::write(
        dir.join("page.html"),
        "<html><body><h1>hello</h1></body></html>",
    )
    .unwrap();
    let test = |name: &str| {
        format!(
            "  - name: {name}\n    steps:\n      - launchApp\n      - assertVisible: {{ text: hello }}\n      - wait: {{ ms: 300 }}\n"
        )
    };
    std::fs::write(
        dir.join("suite.yaml"),
        format!(
            "appId: \"file://{}/page.html\"\nconfig:\n  platform: web\n  web:\n    chromePath: \"{chrome}\"\n    extraArgs: [\"--no-sandbox\"]\ntests:\n{}{}{}{}",
            dir.display(),
            test("a"),
            test("b"),
            test("c"),
            test("d"),
        ),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_velocity"))
        .args(["run", "suite.yaml", "--parallel", "2", "--report", "json"])
        .current_dir(&dir)
        .env("VELOCITY_LEASE_DIR", dir.join("leases"))
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(text.contains("2 browsers"), "{text}");
    // W3: by default they are two contexts of one Chrome.
    assert!(text.contains("contexts of one Chrome"), "{text}");
    let results: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join("velocity-results/results.json")).unwrap(),
    )
    .unwrap();
    let mut devices: Vec<String> = results["tests"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["device"].as_str().map(String::from))
        .collect();
    devices.sort();
    devices.dedup();
    assert_eq!(devices, ["web-0", "web-1"], "{results}");
    let _ = std::fs::remove_dir_all(&dir);
}
