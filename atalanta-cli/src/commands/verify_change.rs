// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! `atalanta verify-change "<description>"` (row 1.4: AI-088, VOI-002,
//! VOI-003): did this change do what it says on the device? It answers
//! with evidence a reviewer can accept: pass/fail, a video and screenshots
//! of the flow, `failures.md`, and a manifest, in one folder ready for
//! `gh pr comment --body-file <dir>/comment.md`.
//!
//! The saved flow is a replay cache that must not lie:
//!
//! - **Key:** sha256 of the precondition (the app, the `--after` login's
//!   content), the goal and the platform. The entry also keeps the flow's
//!   own sha256, so an edited file is a miss.
//! - **Hit:** a trusted flow for the key replays with `atalanta run`: no
//!   model. If it fails, the flow may be stale or the app may be broken, so
//!   it is a miss.
//! - **Miss:** `atalanta explore` writes a new flow. Only a flow that passed
//!   explore's trust gates (burn-in, the wrong-path replay, goal text) is
//!   saved, and then it replays for the evidence.
//!
//! The hit rate is kept in the cache folder and reported.

use std::path::{Path, PathBuf};
use std::process::Command;

use clap::Args;
use colored::Colorize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

#[derive(Args)]
pub struct VerifyChangeArgs {
    /// What the change should do, in plain English ("the cart shows the
    /// discount"): the goal explored and checked
    description: String,

    /// The app under test (bundle id / package)
    #[arg(long = "app-id", alias = "app")]
    app_id: String,

    /// Device to use (default: the first booted one)
    #[arg(short, long)]
    device: Option<String>,

    /// ios, android or web (default: from the device id)
    #[arg(short, long)]
    platform: Option<String>,

    /// A login (or other setup) suite replayed with no model first
    #[arg(long, value_name = "SUITE")]
    after: Option<String>,

    /// Goal model as provider:model (default: the configured provider's)
    #[arg(long)]
    model: Option<String>,

    /// A fixed plan instead of a model (explore's `--script`, docs/EXPLORE.md)
    #[arg(long, value_name = "FILE", conflicts_with = "model")]
    script: Option<String>,

    /// A value for a ${NAME} (repeatable), NAME=VALUE
    #[arg(long = "env", value_name = "NAME=VALUE")]
    env: Vec<String>,

    /// Where the evidence goes: manifest.json, failures.md, comment.md and
    /// the run's reports, video and screenshots under run/
    #[arg(
        long,
        default_value = "atalanta-results/verify-change",
        value_name = "DIR"
    )]
    bundle: PathBuf,

    /// Where saved tests live (commit it to share them)
    #[arg(long, default_value = ".atalanta/verify-cache", value_name = "DIR")]
    cache: PathBuf,

    /// Explore even when a saved test exists
    #[arg(long)]
    no_cache: bool,
}

/// The cache key: precondition + goal + platform.
pub fn cache_key(app_id: &str, goal: &str, platform: &str, after: Option<&str>) -> String {
    let goal: String = goal
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let mut h = Sha256::new();
    for part in [
        "atalanta-verify-1",
        app_id,
        &goal,
        platform,
        after.unwrap_or(""),
    ] {
        h.update(part.as_bytes());
        h.update([0u8]);
    }
    hex(&h.finalize())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn sha256_of(text: &str) -> String {
    hex(&Sha256::digest(text.as_bytes()))
}

/// How the verification went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Passed,
    Failed,
    /// Not a verdict on the change: config, device or internal (its exit).
    Error(i32),
}

/// Everything the run needs that isn't the CLI's argument parsing.
pub struct Plan<'a> {
    pub program: &'a Path,
    pub args: &'a VerifyChangeArgs,
    pub platform_label: String,
    /// Extra environment for the `atalanta` children (tests use it).
    pub child_env: Vec<(String, String)>,
}

/// Run the verification and write the bundle; the verdict and the manifest.
pub fn verify(plan: &Plan) -> anyhow::Result<(Verdict, Value)> {
    let a = plan.args;
    let after_text = match &a.after {
        Some(p) => Some(std::fs::read_to_string(p).map_err(|e| usage_err!("--after {p}: {e}"))?),
        None => None,
    };
    let key = cache_key(
        &a.app_id,
        &a.description,
        &plan.platform_label,
        after_text.as_deref(),
    );
    std::fs::create_dir_all(&a.cache)?;
    std::fs::create_dir_all(&a.bundle)?;
    let run_dir = a.bundle.join("run");
    let _ = std::fs::remove_dir_all(&run_dir);
    for f in ["manifest.json", "failures.md", "comment.md"] {
        let _ = std::fs::remove_file(a.bundle.join(f));
    }
    let flow = a.cache.join(format!("{key}.yaml"));
    let entry_path = a.cache.join(format!("{key}.json"));
    let entry: Option<Value> = std::fs::read(&entry_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    let cached_ok = !a.no_cache
        && entry.as_ref().is_some_and(|e| {
            e["trusted"] == true
                && std::fs::read_to_string(&flow).is_ok_and(|y| e["flowSha256"] == sha256_of(&y))
        });

    let mut notes: Vec<String> = Vec::new();
    let mut explore_json = Value::Null;
    let mut model_calls = 0u64;
    let mut hit = false;
    let mut run_exit: Option<i32> = None;

    if cached_ok {
        let code = run_flow(plan, &flow, &run_dir)?;
        match code {
            0 => {
                hit = true;
                run_exit = Some(0);
            }
            1 => notes.push(
                "the saved flow failed on replay: explored again to tell a stale flow from a broken change"
                    .into(),
            ),
            other => run_exit = Some(other),
        }
    }
    if !hit && run_exit.is_none() {
        let tmp = a.cache.join(format!("{key}.new.yaml"));
        let (code, out) = explore(plan, &tmp)?;
        explore_json = out;
        model_calls = explore_json["report"]["modelCalls"].as_u64().unwrap_or(0)
            + explore_json["replay"]["modelCalls"].as_u64().unwrap_or(0);
        let trusted = explore_json["gates"]["trusted"] == true;
        match code {
            0 if trusted => {
                std::fs::rename(&tmp, &flow)?;
                let yaml = std::fs::read_to_string(&flow)?;
                let new_entry = json!({
                    "key": key,
                    "goal": a.description,
                    "appId": a.app_id,
                    "platform": plan.platform_label,
                    "after": a.after,
                    "flowSha256": sha256_of(&yaml),
                    "trusted": true,
                    "gates": explore_json["gates"],
                });
                std::fs::write(&entry_path, serde_json::to_vec_pretty(&new_entry)?)?;
                run_exit = Some(run_flow(plan, &flow, &run_dir)?);
            }
            0 | 1 => {
                let _ = std::fs::remove_file(&tmp);
                run_exit = Some(1);
                let reason = explore_json["report"]["reason"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                notes.push(if code == 0 {
                    "explore reached the goal but the test it wrote didn't pass its trust gates"
                        .into()
                } else {
                    format!(
                        "explore did not verify the goal ({}): {reason}",
                        explore_json["report"]["outcome"]
                            .as_str()
                            .unwrap_or("failed")
                    )
                });
                for r in explore_json["gates"]["reasons"]
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    if let Some(r) = r.as_str() {
                        notes.push(format!("gate: {r}"));
                    }
                }
            }
            other => {
                let _ = std::fs::remove_file(&tmp);
                run_exit = Some(other);
            }
        }
    }
    let verdict = match run_exit.unwrap_or(1) {
        0 => Verdict::Passed,
        1 => Verdict::Failed,
        c => Verdict::Error(c),
    };
    let stats = bump_stats(&a.cache, !a.no_cache, hit)?;
    let manifest = write_bundle(
        plan,
        &key,
        verdict,
        hit,
        model_calls,
        &stats,
        &explore_json,
        &notes,
    )?;
    Ok((verdict, manifest))
}

/// `atalanta run` of the saved flow, with evidence kept for the bundle.
fn run_flow(plan: &Plan, flow: &Path, dir: &Path) -> anyhow::Result<i32> {
    let a = plan.args;
    let mut cmd = Command::new(plan.program);
    cmd.envs(plan.child_env.iter().cloned());
    cmd.arg("run")
        .arg(flow)
        .args(["--retries", "0", "--video", "always", "--trace", "always"])
        .args(["--report", "json,junit,html"])
        .arg("--artifacts-dir")
        .arg(dir);
    if let Some(d) = &a.device {
        cmd.args(["--device", d]);
    }
    if let Some(p) = &a.platform {
        cmd.args(["--platform", p]);
    }
    for e in &a.env {
        cmd.args(["--env", e]);
    }
    let status = cmd
        .status()
        .map_err(|e| anyhow::anyhow!("running {}: {e}", plan.program.display()))?;
    Ok(status.code().unwrap_or(5))
}

/// `atalanta explore --json` writing `out`; its exit code and JSON.
fn explore(plan: &Plan, out: &Path) -> anyhow::Result<(i32, Value)> {
    let a = plan.args;
    let mut cmd = Command::new(plan.program);
    cmd.envs(plan.child_env.iter().cloned());
    cmd.arg("explore")
        .arg(&a.description)
        .args(["--app-id", &a.app_id, "--json"])
        .arg("--out")
        .arg(out);
    if let Some(d) = &a.device {
        cmd.args(["--device", d]);
    }
    if let Some(p) = &a.platform {
        cmd.args(["--platform", p]);
    }
    if let Some(f) = &a.after {
        cmd.args(["--after", f]);
    }
    if let Some(m) = &a.model {
        cmd.args(["--model", m]);
    }
    if let Some(sc) = &a.script {
        cmd.args(["--script", sc]);
    }
    for e in &a.env {
        // explore's typed values: NAME=VALUE as data.
        cmd.args(["--data", e]);
    }
    cmd.stderr(std::process::Stdio::inherit());
    let output = cmd
        .output()
        .map_err(|e| anyhow::anyhow!("running {}: {e}", plan.program.display()))?;
    let v = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    Ok((output.status.code().unwrap_or(5), v))
}

/// Lookups and hits so far, this one counted.
fn bump_stats(cache: &Path, looked: bool, hit: bool) -> anyhow::Result<Value> {
    let path = cache.join("stats.json");
    let mut s: Value = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| json!({ "lookups": 0, "hits": 0 }));
    if looked {
        s["lookups"] = json!(s["lookups"].as_u64().unwrap_or(0) + 1);
        if hit {
            s["hits"] = json!(s["hits"].as_u64().unwrap_or(0) + 1);
        }
    }
    std::fs::write(&path, serde_json::to_vec_pretty(&s)?)?;
    let (l, h) = (
        s["lookups"].as_u64().unwrap_or(0),
        s["hits"].as_u64().unwrap_or(0),
    );
    Ok(
        json!({ "lookups": l, "hits": h, "hitRate": if l == 0 { 0.0 } else { h as f64 / l as f64 } }),
    )
}

/// Files under `dir` with one of `exts`, relative to `base`, sorted.
fn files(dir: &Path, base: &Path, exts: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p
                .extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| exts.contains(&x))
            {
                out.push(
                    p.strip_prefix(base)
                        .unwrap_or(&p)
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    out.sort();
    out
}

#[allow(clippy::too_many_arguments)]
fn write_bundle(
    plan: &Plan,
    key: &str,
    verdict: Verdict,
    hit: bool,
    model_calls: u64,
    stats: &Value,
    explore: &Value,
    notes: &[String],
) -> anyhow::Result<Value> {
    let a = plan.args;
    let dir = &a.bundle;
    let results: Value = std::fs::read(dir.join("run/results.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    let videos = files(&dir.join("run"), dir, &["mp4", "webm", "mov"]);
    let shots = files(&dir.join("run"), dir, &["png", "jpg", "jpeg"]);
    let verdict_s = match verdict {
        Verdict::Passed => "passed",
        Verdict::Failed => "failed",
        Verdict::Error(_) => "error",
    };
    // failures.md: each failure with the evidence it rests on.
    let mut failures = String::from("# Failures\n\n");
    let mut n = 0;
    for t in results["tests"].as_array().into_iter().flatten() {
        if t["status"] != "failed" {
            continue;
        }
        n += 1;
        let step = t["steps"]
            .as_array()
            .and_then(|s| s.iter().find(|s| s["status"] == "failed"));
        failures.push_str(&format!(
            "## {}\n\n- **Error:** {}\n",
            t["test_name"].as_str().unwrap_or("test"),
            t["error_message"].as_str().unwrap_or("(none given)")
        ));
        if let Some(s) = step {
            failures.push_str(&format!(
                "- **Step:** {} `{}`\n",
                s["step_index"].as_u64().map(|i| i + 1).unwrap_or(0),
                s["action_name"].as_str().unwrap_or("")
            ));
        }
        failures.push_str("- **Evidence:** `run/results.json`, `run/report.html`");
        for v in &videos {
            failures.push_str(&format!(", `{v}`"));
        }
        failures.push_str("\n\n");
    }
    for note in notes {
        n += 1;
        failures.push_str(&format!("- {note}\n"));
    }
    if n == 0 {
        failures.push_str("None.\n");
    }
    std::fs::write(dir.join("failures.md"), &failures)?;
    let manifest = json!({
        "schema": 1,
        "description": a.description,
        "appId": a.app_id,
        "platform": plan.platform_label,
        "device": a.device,
        "verdict": verdict_s,
        "exitCode": match verdict { Verdict::Passed => 0, Verdict::Failed => 1, Verdict::Error(c) => c },
        "cache": { "key": key, "hit": hit, "stats": stats },
        "modelCalls": model_calls,
        "explore": if explore.is_null() { Value::Null } else { json!({
            "outcome": explore["report"]["outcome"],
            "reason": explore["report"]["reason"],
            "gates": explore["gates"],
        }) },
        "evidence": {
            "results": "run/results.json",
            "junit": "run/junit.xml",
            "report": "run/report.html",
            "videos": videos,
            "screenshots": shots,
            "failures": "failures.md",
        },
    });
    std::fs::write(
        dir.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    let icon = match verdict {
        Verdict::Passed => "✅",
        Verdict::Failed => "❌",
        Verdict::Error(_) => "⚠️",
    };
    let comment = format!(
        "### {icon} Atalanta verify-change: {verdict_s}\n\n> {}\n\n| | |\n|---|---|\n| App | `{}` ({}) |\n| Saved flow | {} |\n| Model calls | {model_calls} |\n| Evidence | {} video(s), {} screenshot(s) in the run artifacts |\n\n{}",
        a.description,
        if a.app_id.chars().count() > 60 {
            format!("{}…", a.app_id.chars().take(60).collect::<String>())
        } else {
            a.app_id.clone()
        },
        plan.platform_label,
        if hit { "replayed (cache hit, no model)" } else { "explored and saved" },
        manifest["evidence"]["videos"].as_array().map_or(0, Vec::len),
        manifest["evidence"]["screenshots"].as_array().map_or(0, Vec::len),
        if n == 0 { String::new() } else { failures.trim_start_matches("# Failures\n\n").to_string() }
    );
    std::fs::write(dir.join("comment.md"), comment)?;
    Ok(manifest)
}

pub async fn execute(args: VerifyChangeArgs) -> anyhow::Result<i32> {
    let program = std::env::current_exe()?;
    let platform_label = args
        .platform
        .clone()
        .or_else(|| {
            args.device.as_deref().map(|d| {
                atalanta_common::Platform::of_device_id(d)
                    .to_string()
                    .to_lowercase()
            })
        })
        .unwrap_or_else(|| "default".into());
    let plan = Plan {
        program: &program,
        args: &args,
        platform_label,
        child_env: Vec::new(),
    };
    let (verdict, manifest) = tokio::task::block_in_place(|| verify(&plan))?;
    let stats = &manifest["cache"]["stats"];
    println!(
        "\n{} verify-change {}: {}",
        "=>".cyan().bold(),
        match verdict {
            Verdict::Passed => "passed".green().bold(),
            Verdict::Failed => "failed".red().bold(),
            Verdict::Error(_) => "error".yellow().bold(),
        },
        args.description
    );
    println!(
        "   {} · {} model call(s) · cache hit rate {}/{}",
        if manifest["cache"]["hit"] == true {
            "saved flow replayed with no model"
        } else {
            "explored"
        },
        manifest["modelCalls"],
        stats["hits"],
        stats["lookups"]
    );
    println!(
        "   evidence: {}   (attach: gh pr comment --body-file {})",
        args.bundle.display(),
        args.bundle.join("comment.md").display()
    );
    Ok(match verdict {
        Verdict::Passed => 0,
        Verdict::Failed => 1,
        Verdict::Error(c) => c,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    #[command(no_binary_name = true)]
    struct P {
        #[command(flatten)]
        args: VerifyChangeArgs,
    }

    #[test]
    fn the_key_covers_app_goal_platform_and_precondition() {
        let k = cache_key("com.app", "The cart  shows the discount", "android", None);
        assert_eq!(
            k,
            cache_key("com.app", "the cart shows the discount", "android", None)
        );
        assert_ne!(
            k,
            cache_key("com.app", "the cart shows the discount", "ios", None)
        );
        assert_ne!(
            k,
            cache_key(
                "com.app",
                "the cart shows the discount",
                "android",
                Some("login")
            )
        );
        assert_ne!(
            k,
            cache_key("com.other", "the cart shows the discount", "android", None)
        );
    }

    /// A fake `atalanta`: `explore` writes a flow and answers with gates;
    /// `run` passes or fails per $FAKE_RUN and leaves a video. It logs its
    /// argv to $FAKE_LOG.
    fn fake(dir: &Path) -> PathBuf {
        let p = dir.join("atalanta");
        std::fs::write(
            &p,
            r#"#!/bin/sh
echo "$*" >> "$FAKE_LOG"
cmd=$1; shift
if [ "$cmd" = explore ]; then
  while [ $# -gt 0 ]; do [ "$1" = --out ] && out=$2; shift; done
  printf 'appId: com.app\ntests:\n  - name: t\n    steps:\n      - launchApp: {}\n' > "$out"
  printf '{"report":{"outcome":"success","reason":"done","modelCalls":7},"replay":{"modelCalls":0},"gates":{"trusted":%s,"reasons":["wrong path: still passed"]}}' "${FAKE_TRUSTED:-true}"
  exit "${FAKE_EXPLORE:-0}"
fi
if [ "$cmd" = run ]; then
  while [ $# -gt 0 ]; do [ "$1" = --artifacts-dir ] && art=$2; shift; done
  mkdir -p "$art/traces/t"; : > "$art/traces/t/video.mp4"
  if [ "${FAKE_RUN:-0}" = 0 ]; then st=passed; err=null; else st=failed; err='"element not found: Discount"'; fi
  printf '{"total":1,"passed":0,"failed":0,"skipped":0,"retried":0,"duration":{"secs":1,"nanos":0},"tests":[{"test_name":"t","status":"%s","duration":{"secs":1,"nanos":0},"steps":[{"step_index":2,"action_name":"assertVisible","status":"failed"}],"retries":0,"error_message":%s,"screenshots":[]}],"shard_index":null,"shard_total":null}' "$st" "$err" > "$art/results.json"
  exit "${FAKE_RUN:-0}"
fi
"#,
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn plan<'a>(bin: &'a Path, args: &'a VerifyChangeArgs, env: &[(&str, &str)]) -> Plan<'a> {
        Plan {
            program: bin,
            args,
            platform_label: "android".into(),
            child_env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn a_miss_explores_saves_and_the_next_run_is_a_hit_with_no_model() {
        let d = tempfile::tempdir().unwrap();
        let bin = fake(d.path());
        let log = d.path().join("log");
        let bundle = d.path().join("bundle");
        let cache = d.path().join("cache");
        let p = P::try_parse_from([
            "the cart shows the discount",
            "--app-id",
            "com.app",
            "--bundle",
            bundle.to_str().unwrap(),
            "--cache",
            cache.to_str().unwrap(),
        ])
        .unwrap();
        let log_s = log.to_str().unwrap().to_string();

        // 1. Miss: explore (7 model calls), save, replay for evidence.
        let (v, m) = verify(&plan(&bin, &p.args, &[("FAKE_LOG", &log_s)])).unwrap();
        assert_eq!(v, Verdict::Passed, "{m}");
        assert_eq!(m["cache"]["hit"], false);
        assert_eq!(m["modelCalls"], 7);
        assert_eq!(m["evidence"]["videos"][0], "run/traces/t/video.mp4");
        assert!(bundle.join("comment.md").is_file() && bundle.join("failures.md").is_file());

        // 2. Hit: no explore, no model.
        let (v, m) = verify(&plan(&bin, &p.args, &[("FAKE_LOG", &log_s)])).unwrap();
        assert_eq!(v, Verdict::Passed);
        assert_eq!(m["cache"]["hit"], true);
        assert_eq!(m["modelCalls"], 0);
        assert_eq!(m["cache"]["stats"]["hits"], 1);
        assert_eq!(m["cache"]["stats"]["lookups"], 2);
        let calls = std::fs::read_to_string(&log).unwrap();
        assert_eq!(
            calls.lines().filter(|l| l.starts_with("explore")).count(),
            1,
            "{calls}"
        );

        // 3. The saved flow fails and explore can't reach the goal: failed,
        // with the failing step and the reason in failures.md.
        let (v, m) = verify(&plan(
            &bin,
            &p.args,
            &[
                ("FAKE_LOG", &log_s),
                ("FAKE_RUN", "1"),
                ("FAKE_EXPLORE", "1"),
            ],
        ))
        .unwrap();
        assert_eq!(v, Verdict::Failed, "{m}");
        let failures = std::fs::read_to_string(bundle.join("failures.md")).unwrap();
        assert!(
            failures.contains("element not found: Discount"),
            "{failures}"
        );
        assert!(
            failures.contains("explore did not verify the goal"),
            "{failures}"
        );
    }

    #[test]
    fn an_untrusted_flow_is_never_saved() {
        let d = tempfile::tempdir().unwrap();
        let bin = fake(d.path());
        let log = d.path().join("log");
        let cache = d.path().join("cache");
        let p = P::try_parse_from([
            "buy the monthly plan",
            "--app-id",
            "com.app",
            "--bundle",
            d.path().join("b").to_str().unwrap(),
            "--cache",
            cache.to_str().unwrap(),
        ])
        .unwrap();
        let (v, m) = verify(&plan(
            &bin,
            &p.args,
            &[
                ("FAKE_LOG", log.to_str().unwrap()),
                ("FAKE_TRUSTED", "false"),
            ],
        ))
        .unwrap();
        assert_eq!(v, Verdict::Failed, "{m}");
        let saved: Vec<_> = std::fs::read_dir(&cache)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "yaml"))
            .collect();
        assert!(saved.is_empty(), "an untrusted flow was saved");
        let failures = std::fs::read_to_string(d.path().join("b/failures.md")).unwrap();
        assert!(
            failures.contains("trust gates") && failures.contains("wrong path"),
            "{failures}"
        );
    }
}
