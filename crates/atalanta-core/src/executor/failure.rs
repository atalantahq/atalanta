// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

//! Crashes and failure evidence: crash checks, failure screenshots, the pause on failure.

use super::*;

impl<'a> TestExecutor<'a> {
    /// I3 (D-50): the crash the driver already knows of for the suite's
    /// app, sticky for the rest of the test. No device round trip (the
    /// driver answers from what it has seen), so it is asked before every
    /// step and on every attempt of an element wait. `None` once the test
    /// stopped the app on purpose.
    pub(super) async fn pending_crash(&mut self, device_id: &str) -> Option<AtalantaError> {
        if self.app_stopped {
            return None;
        }
        let app_id = self.app_id.clone();
        if self.sticky_crash.is_none() {
            let latched = self.driver.pending_crash(device_id, &app_id).await?;
            // The driver's crash report says why, when it has one.
            let detail = match self.driver.crash_report(device_id, &app_id).await {
                Some(d) => d,
                None => latched,
            };
            self.sticky_crash = Some(detail);
        }
        self.sticky_crash
            .clone()
            .map(|detail| AtalantaError::AppCrashed { app_id, detail })
    }

    /// The suite's app crashed (the driver's liveness says so), unless the
    /// test stopped it on purpose. Read at most once between actions: a run
    /// of negative assertions pays for one read (an `simctl` call on iOS),
    /// not one each.
    pub(super) async fn check_alive(&mut self, device_id: &str) -> Option<AtalantaError> {
        if self.app_stopped {
            return None;
        }
        let app_id = self.app_id.clone();
        if self.liveness_seen.is_none() {
            self.liveness_seen = Some(self.driver.app_liveness(device_id, &app_id).await);
        }
        match &self.liveness_seen {
            Some(atalanta_common::Liveness::Dead(detail)) => Some(AtalantaError::AppCrashed {
                app_id,
                detail: detail.clone(),
            }),
            _ => None,
        }
    }

    /// `stepScreenshots`: the screen after step `index`, held in memory.
    pub(super) async fn hold_step_screenshot(
        &mut self,
        device_id: &str,
        index: usize,
        action: &str,
    ) {
        let Ok(data) = self.driver.screenshot(device_id).await else {
            return;
        };
        let clean = |s: &str| s.replace(|c: char| !c.is_alphanumeric(), "_");
        let attempt = if self.retry_attempt > 0 {
            format!("_retry{}", self.retry_attempt)
        } else {
            String::new()
        };
        let path = PathBuf::from(&self.config.artifacts.output_dir)
            .join("steps")
            .join(format!("{}{attempt}", clean(&self.current_test)))
            .join(format!("step{index:02}_{}.png", clean(action)));
        self.held_shots.push((path, data));
    }

    /// The crash reports `save_crash_logs` copied for the running test.
    pub(super) fn crash_log_files(&self) -> Vec<PathBuf> {
        let clean = |s: &str| s.replace(|c: char| !c.is_alphanumeric(), "_");
        let attempt = if self.retry_attempt > 0 {
            format!("_retry{}", self.retry_attempt)
        } else {
            String::new()
        };
        let dir = PathBuf::from(&self.config.artifacts.output_dir)
            .join("crashlogs")
            .join(format!("{}{attempt}", clean(&self.current_test)));
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map(|d| {
                d.filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| p.is_file())
                    .collect()
            })
            .unwrap_or_default();
        files.sort();
        files
    }

    /// `--pause-on-failure`: write what there is so far (screenshot,
    /// video and logs, `trace.html`), say where, and wait for Enter, `r`
    /// or `q`. The device is left exactly as the failure left it.
    pub(super) async fn pause_at_failure(
        &mut self,
        device_id: &str,
        test: &str,
        failed: &StepResult,
    ) -> PauseChoice {
        let _one_at_a_time = PAUSE_PROMPT.lock().await;
        let i = failed.step_index;
        let shot = match &failed.screenshot {
            Some(p) => Some(p.clone()),
            None => self.take_failure_screenshot(device_id, test, i).await,
        };
        let root = PathBuf::from(&self.config.artifacts.output_dir);
        let dir = crate::trace::trace_dir(&root, test);
        let suffix = self.trace.file_suffix(self.retry_attempt);
        let mut files = Vec::new();
        if let Some(session) = self.evidence.as_mut() {
            let out = atalanta_common::EvidenceOut {
                dir: &dir,
                suffix: &suffix,
                keep_video: self.config.artifacts.video.on(),
                keep_logs: self.config.artifacts.logs.on(),
            };
            if let Ok(ev) =
                tokio::time::timeout(EVIDENCE_FINISH_BUDGET, session.checkpoint(out)).await
            {
                use atalanta_common::AttachmentKind as K;
                let rel = |k: &[K]| -> Vec<String> {
                    ev.attachments
                        .iter()
                        .filter(|a| k.contains(&a.kind))
                        .map(|a| crate::trace_html::relative(&a.path, &dir))
                        .collect()
                };
                let evidence = crate::trace::TraceEvidence {
                    video: rel(&[K::Video]).into_iter().next(),
                    video_offset_ms: ev.video_offset_ms,
                    screencast: rel(&[K::Screencast]).into_iter().next(),
                    logs: rel(&[K::AppLog, K::DeviceLog]),
                    log_lines: None,
                    notes: ev.notes.clone(),
                };
                files = ev.attachments.iter().map(|a| a.path.clone()).collect();
                self.trace.set_evidence(evidence, ev.log_lines);
            }
        }
        let html = self.trace.snapshot_html(
            &root,
            test,
            device_id,
            self.config.platform,
            self.retry_attempt,
        );
        let mut msg = format!(
            "\n=> Paused at step {} of '{test}' on {device_id}: {}\n",
            i + 1,
            atalanta_common::redact::redact(failed.error_message.as_deref().unwrap_or("failed"))
        );
        if let Some(p) = &shot {
            msg.push_str(&format!("   screenshot: {}\n", p.display()));
        }
        if let Some(p) = &html {
            msg.push_str(&format!("   trace:      {}\n", p.display()));
        }
        for f in &files {
            msg.push_str(&format!("   evidence:   {}\n", f.display()));
        }
        msg.push_str(
            "   The device is left as it is: inspect it live with `atalanta inspect` in another \
             terminal.\n   [Enter] continue  [r] retry the step  [q] abort the run: ",
        );
        eprint!("{msg}");
        let answer = tokio::task::spawn_blocking(|| {
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).map(|_| line)
        })
        .await;
        match answer {
            Ok(Ok(line)) => parse_pause_answer(&line),
            _ => PauseChoice::Continue,
        }
    }

    /// D-50: the crash reports of a crash into the test's artifacts
    /// (`crashlogs/<test>/`), copied off a physical iPhone with devicectl.
    pub(super) async fn save_crash_logs(&self, device_id: &str, app_id: &str) {
        let clean = |s: &str| s.replace(|c: char| !c.is_alphanumeric(), "_");
        let attempt = if self.retry_attempt > 0 {
            format!("_retry{}", self.retry_attempt)
        } else {
            String::new()
        };
        let dir = PathBuf::from(&self.config.artifacts.output_dir)
            .join("crashlogs")
            .join(format!("{}{attempt}", clean(&self.current_test)));
        let copy = self.driver.copy_crash_logs(device_id, app_id, &dir);
        match tokio::time::timeout(Duration::from_secs(150), copy).await {
            Ok(files) if !files.is_empty() => {
                tracing::info!("crash report(s) saved to {}", dir.display());
            }
            Ok(_) => {}
            Err(_) => tracing::warn!("copying the crash reports timed out"),
        }
    }

    pub(super) async fn take_failure_screenshot(
        &self,
        device_id: &str,
        context: &str,
        step_index: usize,
    ) -> Option<PathBuf> {
        // Its own small budget: after a watchdog timeout the device is
        // likely the thing that hung, and this ran outside the watchdog
        // (one hung step kept a run going 12.5 min; VEL-351).
        let data =
            match tokio::time::timeout(FAILURE_ARTIFACT_BUDGET, self.driver.screenshot(device_id))
                .await
            {
                Ok(Ok(png)) => png,
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, step_index, "failure screenshot not taken");
                    return None;
                }
                Err(_) => {
                    tracing::warn!(
                        step_index,
                        "failure screenshot not taken: the device didn't answer in time"
                    );
                    return None;
                }
            };
        let clean = |s: &str| s.replace(|c: char| !c.is_alphanumeric(), "_");
        // Named by test (and retry) as well as step: `failure_tap_step2.png`
        // from one test overwrote another test's, so a suite with several
        // failures kept the evidence of only the last one.
        let test = clean(&self.current_test);
        let attempt = if self.retry_attempt > 0 {
            format!("_retry{}", self.retry_attempt)
        } else {
            String::new()
        };
        let fname = if test.is_empty() || context == self.current_test {
            format!("failure_{}_step{step_index}{attempt}.png", clean(context))
        } else {
            format!(
                "failure_{test}__{}_step{step_index}{attempt}.png",
                clean(context)
            )
        };
        let path = PathBuf::from(&self.config.artifacts.output_dir).join(&fname);
        write_artifact(&path, data).await.then_some(path)
    }
}
