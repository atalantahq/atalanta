use std::path::PathBuf;
use std::time::{Duration, Instant};

use velocity_common::{
    Action, Direction, Platform, PlatformDriver, Result, Selector, Step, StepResult, StepStatus,
    SuiteConfig, SyncMode, TestCase, TestResult, TestStatus, VelocityError,
};

use crate::dialog_handler::DialogHandler;
use crate::native_sync::SyncManager;
use crate::profiler::ResourceProfiler;
use crate::selector::SelectorEngine;

pub struct TestExecutor<'a> {
    driver: &'a dyn PlatformDriver,
    sync_manager: SyncManager,
    selector_engine: SelectorEngine,
    dialog_handler: DialogHandler,
    config: SuiteConfig,
    profiler: Option<ResourceProfiler>,
}

impl<'a> TestExecutor<'a> {
    pub fn new(driver: &'a dyn PlatformDriver, config: SuiteConfig, app_id: &str) -> Self {
        let platform = config.platform.unwrap_or(Platform::Ios);
        let sync_manager = SyncManager::new(&config.sync, platform);
        let selector_engine = SelectorEngine::with_healing(config.healing.clone());
        let dialog_handler = DialogHandler::new(config.dialog.clone());
        let profiler = if config.performance.enabled {
            Some(ResourceProfiler::new(app_id.to_string()))
        } else {
            None
        };
        Self {
            driver,
            sync_manager,
            selector_engine,
            dialog_handler,
            config,
            profiler,
        }
    }

    pub async fn execute_test(
        &mut self,
        test: &TestCase,
        device_id: &str,
        app_id: &str,
    ) -> Result<TestResult> {
        let test_start = Instant::now();
        let mut step_results = Vec::with_capacity(test.steps.len());
        let mut screenshots = Vec::new();
        let mut test_error: Option<String> = None;

        for (i, step) in test.steps.iter().enumerate() {
            let step_result = self.execute_step(step, device_id, app_id, i).await;

            match step_result {
                Ok(result) => {
                    if result.status == StepStatus::Failed {
                        if let Some(ref path) = result.screenshot {
                            screenshots.push(path.clone());
                        }
                        test_error = result.error_message.clone();
                        step_results.push(result);
                        // Mark remaining steps as skipped
                        for j in (i + 1)..test.steps.len() {
                            step_results.push(StepResult {
                                step_index: j,
                                action_name: action_name(&test.steps[j].action),
                                status: StepStatus::Skipped,
                                duration: Duration::ZERO,
                                screenshot: None,
                                error_message: None,
                                resource_delta: None,
                            });
                        }
                        break;
                    }
                    step_results.push(result);
                }
                Err(e) => {
                    let error_msg = e.to_string();

                    // Take failure screenshot if configured
                    let screenshot = if self.config.artifacts.on_failure {
                        self.take_failure_screenshot(device_id, &test.name, i).await
                    } else {
                        None
                    };
                    if let Some(ref path) = screenshot {
                        screenshots.push(path.clone());
                    }

                    test_error = Some(error_msg.clone());
                    step_results.push(StepResult {
                        step_index: i,
                        action_name: action_name(&step.action),
                        status: StepStatus::Failed,
                        duration: test_start.elapsed(),
                        screenshot,
                        error_message: Some(error_msg),
                        resource_delta: None,
                    });

                    // Mark remaining steps as skipped
                    for j in (i + 1)..test.steps.len() {
                        step_results.push(StepResult {
                            step_index: j,
                            action_name: action_name(&test.steps[j].action),
                            status: StepStatus::Skipped,
                            duration: Duration::ZERO,
                            screenshot: None,
                            error_message: None,
                            resource_delta: None,
                        });
                    }
                    break;
                }
            }
        }

        let all_passed = step_results.iter().all(|r| r.status == StepStatus::Passed);
        let status = if all_passed {
            TestStatus::Passed
        } else {
            TestStatus::Failed
        };

        let resource_peak = self.profiler.as_ref().and_then(|p| p.peak()).cloned();

        Ok(TestResult {
            test_name: test.name.clone(),
            status,
            duration: test_start.elapsed(),
            steps: step_results,
            retries: 0,
            error_message: test_error,
            screenshots,
            resource_peak,
        })
    }

    pub async fn execute_step(
        &mut self,
        step: &Step,
        device_id: &str,
        app_id: &str,
        step_index: usize,
    ) -> Result<StepResult> {
        let step_start = Instant::now();
        let name = action_name(&step.action);

        // Pre-action sync (skip for wait, screenshot, and app lifecycle actions)
        let skip_sync = matches!(
            step.action,
            Action::Wait { .. }
                | Action::Screenshot { .. }
                | Action::LaunchApp { .. }
                | Action::StopApp { .. }
                | Action::Goto { .. }
                | Action::GoBack
                | Action::Refresh
        );
        if !skip_sync {
            // Dismiss any system dialogs before waiting for idle
            self.dialog_handler
                .dismiss_if_present(self.driver, device_id)
                .await;

            // Collect pre-action metrics concurrently with sync (zero added latency)
            let sync_mgr = &mut self.sync_manager;
            let driver = self.driver;
            let pkg = self.profiler.as_ref().map(|p| p.package().to_string());

            let snap_fut = async {
                match &pkg {
                    Some(pkg) => driver.collect_resource_metrics(device_id, pkg).await.ok(),
                    None => None,
                }
            };

            let (sync_result, raw_before) = tokio::join!(
                sync_mgr.wait_for_idle(driver, device_id),
                snap_fut
            );
            sync_result?;

            if let (Some(profiler), Some(raw)) = (self.profiler.as_mut(), raw_before) {
                profiler.record_before(ResourceProfiler::snapshot_from_raw(raw));
            }
        }

        let result = self.execute_action(&step.action, device_id, app_id).await;

        // Post-action sync for actions that modify UI state
        let mutating = matches!(
            step.action,
            Action::Goto { .. }
                | Action::GoBack
                | Action::Refresh
                | Action::Tap { .. }
                | Action::DoubleTap { .. }
                | Action::LongPress { .. }
                | Action::InputText { .. }
                | Action::ClearText { .. }
                | Action::Swipe { .. }
                | Action::ScrollUntilVisible { .. }
                | Action::PressKey { .. }
                | Action::LaunchApp { .. }
                | Action::StopApp { .. }
                | Action::TapXY { .. }
                | Action::InputRaw { .. }
        );

        let mut resource_delta = None;

        match result {
            Ok(()) => {
                if mutating {
                    self.selector_engine.invalidate_cache();
                    self.sync_manager.invalidate();

                    // Collect post-action metrics concurrently with sync
                    let sync_mgr = &mut self.sync_manager;
                    let driver = self.driver;
                    let pkg = self.profiler.as_ref().map(|p| p.package().to_string());

                    let snap_fut = async {
                        match &pkg {
                            Some(pkg) => driver.collect_resource_metrics(device_id, pkg).await.ok(),
                            None => None,
                        }
                    };

                    let (sync_result, raw_after) = tokio::join!(
                        sync_mgr.wait_for_idle(driver, device_id),
                        snap_fut
                    );
                    // Best-effort: ignore sync timeout
                    let _ = sync_result;

                    if let (Some(profiler), Some(raw)) = (self.profiler.as_mut(), raw_after) {
                        resource_delta = profiler.record_after(ResourceProfiler::snapshot_from_raw(raw));
                    }

                    // Check for system dialogs after the action.
                    // Skip for coordinate-based actions (TapXY, InputRaw) — these target
                    // canvas/WebView elements that never trigger system permission dialogs.
                    let can_trigger_dialog = !matches!(
                        step.action,
                        Action::TapXY { .. } | Action::InputRaw { .. }
                    );
                    if can_trigger_dialog {
                        self.dialog_handler
                            .dismiss_if_present(self.driver, device_id)
                            .await;
                    }
                }
                Ok(StepResult {
                    step_index,
                    action_name: name,
                    status: StepStatus::Passed,
                    duration: step_start.elapsed(),
                    screenshot: None,
                    error_message: None,
                    resource_delta,
                })
            }
            Err(e) => {
                let screenshot = if self.config.artifacts.on_failure {
                    self.take_failure_screenshot(device_id, &name, step_index)
                        .await
                } else {
                    None
                };
                Ok(StepResult {
                    step_index,
                    action_name: name,
                    status: StepStatus::Failed,
                    duration: step_start.elapsed(),
                    screenshot,
                    error_message: Some(e.to_string()),
                    resource_delta: None,
                })
            }
        }
    }

    /// When sync mode is None, find_element retries with implicit wait
    /// instead of failing immediately. This eliminates the need for explicit
    /// `wait` steps — the test proceeds as soon as the element appears.
    async fn find_element_smart(
        &mut self,
        device_id: &str,
        selector: &Selector,
        timeout_ms: u64,
    ) -> Result<velocity_common::Element> {
        // Fast path: if sync mode is not None, do a single lookup (sync engine handles waiting)
        if self.config.sync.mode != SyncMode::None {
            return self
                .selector_engine
                .find_element(self.driver, device_id, selector)
                .await;
        }

        // Implicit wait: retry until element found or timeout.
        // First attempt uses the existing cache (filled by post-action dialog check).
        // Only subsequent retries invalidate to get a fresh hierarchy.
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let poll_interval = Duration::from_millis(300);
        let mut last_err = None;
        let mut first = true;

        loop {
            if !first {
                self.selector_engine.invalidate_cache();
            }
            first = false;
            match self
                .selector_engine
                .find_element(self.driver, device_id, selector)
                .await
            {
                Ok(el) => return Ok(el),
                Err(e) => {
                    last_err = Some(e);
                    if Instant::now() + poll_interval > deadline {
                        break;
                    }
                    tokio::time::sleep(poll_interval).await;
                }
            }
        }

        Err(last_err.unwrap_or_else(|| VelocityError::ElementNotFound {
            selector: format!("{selector}"),
            timeout_ms,
            screenshot: None,
            hierarchy_snapshot: None,
        }))
    }

    async fn execute_action(
        &mut self,
        action: &Action,
        device_id: &str,
        app_id: &str,
    ) -> Result<()> {
        match action {
            Action::LaunchApp {
                app_id: override_id,
                clear_state,
            } => {
                let id = if override_id.is_empty() {
                    app_id
                } else {
                    override_id
                };
                self.driver.launch_app(device_id, id, *clear_state).await
            }
            Action::StopApp {
                app_id: override_id,
            } => {
                let id = if override_id.is_empty() {
                    app_id
                } else {
                    override_id
                };
                self.driver.stop_app(device_id, id).await
            }
            Action::Tap { selector } => {
                let timeout = self.config.sync.timeout_ms;
                let element = self.find_element_smart(device_id, selector, timeout).await?;
                self.driver.tap(device_id, &element).await
            }
            Action::DoubleTap { selector } => {
                let timeout = self.config.sync.timeout_ms;
                let element = self.find_element_smart(device_id, selector, timeout).await?;
                self.driver.double_tap(device_id, &element).await
            }
            Action::LongPress {
                selector,
                duration_ms,
            } => {
                let timeout = self.config.sync.timeout_ms;
                let element = self.find_element_smart(device_id, selector, timeout).await?;
                let ms = duration_ms.unwrap_or(1000);
                self.driver.long_press(device_id, &element, ms).await
            }
            Action::InputText { selector, text } => {
                let timeout = self.config.sync.timeout_ms;
                let element = self.find_element_smart(device_id, selector, timeout).await?;
                self.driver.input_text(device_id, &element, text).await
            }
            Action::ClearText { selector } => {
                let timeout = self.config.sync.timeout_ms;
                let element = self.find_element_smart(device_id, selector, timeout).await?;
                self.driver.clear_text(device_id, &element).await
            }
            Action::AssertVisible { selector } => {
                let timeout = self.config.sync.timeout_ms;
                let element = self.find_element_smart(device_id, selector, timeout).await?;
                let visible = self.driver.is_element_visible(device_id, &element).await?;
                if !visible {
                    return Err(VelocityError::AssertionFailed {
                        expected: "visible".to_string(),
                        actual: "not visible".to_string(),
                        selector: format!("{selector}"),
                        screenshot: None,
                    });
                }
                Ok(())
            }
            Action::AssertNotVisible { selector } => {
                // No implicit wait for "not visible" — check once immediately
                let result = self
                    .selector_engine
                    .find_element(self.driver, device_id, selector)
                    .await;
                match result {
                    Err(_) => Ok(()), // Element not found = not visible, pass
                    Ok(element) => {
                        let visible = self.driver.is_element_visible(device_id, &element).await?;
                        if visible {
                            return Err(VelocityError::AssertionFailed {
                                expected: "not visible".to_string(),
                                actual: "visible".to_string(),
                                selector: format!("{selector}"),
                                screenshot: None,
                            });
                        }
                        Ok(())
                    }
                }
            }
            Action::AssertText { selector, expected } => {
                let timeout = self.config.sync.timeout_ms;
                let element = self.find_element_smart(device_id, selector, timeout).await?;
                let actual = self.driver.get_element_text(device_id, &element).await?;
                if actual != *expected {
                    return Err(VelocityError::AssertionFailed {
                        expected: expected.clone(),
                        actual,
                        selector: format!("{selector}"),
                        screenshot: None,
                    });
                }
                Ok(())
            }
            Action::ScrollUntilVisible {
                selector,
                direction,
                max_scrolls,
            } => {
                for _ in 0..*max_scrolls {
                    let found = self
                        .selector_engine
                        .find_element(self.driver, device_id, selector)
                        .await;
                    if let Ok(element) = found {
                        if self
                            .driver
                            .is_element_visible(device_id, &element)
                            .await
                            .unwrap_or(false)
                        {
                            return Ok(());
                        }
                    }
                    self.selector_engine.invalidate_cache();
                    self.driver.swipe(device_id, *direction).await?;
                    let _ = self.sync_manager.wait_for_idle(self.driver, device_id).await;
                }
                Err(VelocityError::ElementNotFound {
                    selector: format!("{selector}"),
                    timeout_ms: 0,
                    screenshot: None,
                    hierarchy_snapshot: None,
                })
            }
            Action::Swipe {
                direction,
                from,
                to,
            } => {
                if let (Some(from), Some(to)) = (from, to) {
                    self.driver.swipe_coords(device_id, *from, *to).await
                } else if let Some(dir) = direction {
                    self.driver.swipe(device_id, *dir).await
                } else {
                    self.driver.swipe(device_id, Direction::Down).await
                }
            }
            Action::Screenshot { filename } => {
                let data = self.driver.screenshot(device_id).await?;
                let fname = filename
                    .clone()
                    .unwrap_or_else(|| format!("screenshot_{}.png", chrono_millis()));
                let path = PathBuf::from(&self.config.artifacts.output_dir).join(&fname);
                // Best-effort write; don't fail the step if artifacts dir doesn't exist
                let _ = std::fs::create_dir_all(path.parent().unwrap_or(&PathBuf::from(".")));
                let _ = std::fs::write(&path, &data);
                Ok(())
            }
            Action::PressKey { key } => self.driver.press_key(device_id, key.clone()).await,
            Action::Wait { ms } => {
                tokio::time::sleep(Duration::from_millis(*ms)).await;
                Ok(())
            }
            Action::RunFlow { .. } => {
                // Flows should be resolved before execution
                Err(VelocityError::Config(
                    "runFlow encountered during execution; flows must be resolved first"
                        .to_string(),
                ))
            }
            Action::TapXY { x, y } => {
                self.driver.tap_xy(device_id, *x, *y).await
            }
            Action::InputRaw { text } => {
                self.driver.input_raw(device_id, text).await
            }
            Action::Goto { url } => self.driver.navigate(device_id, url).await,
            Action::GoBack => self.driver.go_back(device_id).await,
            Action::Refresh => self.driver.refresh_page(device_id).await,
        }
    }

    async fn take_failure_screenshot(
        &self,
        device_id: &str,
        context: &str,
        step_index: usize,
    ) -> Option<PathBuf> {
        let data = self.driver.screenshot(device_id).await.ok()?;
        let sanitized = context.replace(|c: char| !c.is_alphanumeric(), "_");
        let fname = format!("failure_{sanitized}_step{step_index}.png");
        let path = PathBuf::from(&self.config.artifacts.output_dir).join(&fname);
        let _ = std::fs::create_dir_all(path.parent()?);
        std::fs::write(&path, &data).ok()?;
        Some(path)
    }
}

fn action_name(action: &Action) -> String {
    match action {
        Action::LaunchApp { .. } => "launchApp".to_string(),
        Action::StopApp { .. } => "stopApp".to_string(),
        Action::Tap { .. } => "tap".to_string(),
        Action::DoubleTap { .. } => "doubleTap".to_string(),
        Action::LongPress { .. } => "longPress".to_string(),
        Action::InputText { .. } => "inputText".to_string(),
        Action::ClearText { .. } => "clearText".to_string(),
        Action::AssertVisible { .. } => "assertVisible".to_string(),
        Action::AssertNotVisible { .. } => "assertNotVisible".to_string(),
        Action::AssertText { .. } => "assertText".to_string(),
        Action::ScrollUntilVisible { .. } => "scrollUntilVisible".to_string(),
        Action::Swipe { .. } => "swipe".to_string(),
        Action::Screenshot { .. } => "screenshot".to_string(),
        Action::PressKey { .. } => "pressKey".to_string(),
        Action::Wait { .. } => "wait".to_string(),
        Action::RunFlow { .. } => "runFlow".to_string(),
        Action::TapXY { x, y } => format!("tapXY({x},{y})"),
        Action::InputRaw { .. } => "inputRaw".to_string(),
        Action::Goto { url } => format!("goto({url})"),
        Action::GoBack => "goBack".to_string(),
        Action::Refresh => "refresh".to_string(),
    }
}

fn chrono_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
