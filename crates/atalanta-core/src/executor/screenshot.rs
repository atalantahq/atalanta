//! Screenshot assertions: baselines, crops, masks and the still frame.

use super::*;

impl<'a> TestExecutor<'a> {
    /// `assertScreenshot`: mask the status bar (or freeze it, D-V4) and
    /// any `mask` elements, cut to `crop` if set, and compare with the
    /// baseline in
    /// `<suite dir>/velocity-baselines/<platform>[-headless]/<W>x<H>/<name>.png`
    /// (`W`x`H`: the device's full capture, D-V3).
    ///
    /// Playwright's capture: a first frame that matches the baseline passes
    /// at once; otherwise frames are taken until two in a row match each
    /// other under the step's own rule, threshold and cluster limit, and
    /// the still one is compared. A screen that never holds still fails
    /// with the previous frame, the actual one and their diff, and is never
    /// written as a baseline (B5/B6). A missing baseline is written and the
    /// step fails, on a laptop and in CI alike, unless the run is
    /// `--update-baselines[=missing]` (D-V1).
    pub(super) async fn assert_screenshot(
        &mut self,
        device_id: &str,
        name: &str,
        step: ScreenshotStep<'_>,
    ) -> Result<()> {
        use crate::visual::{compare_images_rule, VisualConfig, VisualEngine};
        use velocity_common::BaselineUpdate;
        let threshold = step.threshold.unwrap_or(0.98);
        let cluster = step.cluster_px.unwrap_or(crate::visual::DEFAULT_CLUSTER_PX);
        let rule = step.rule;
        let frozen = self.freeze_status_bar(device_id).await;
        let first_full = self.capture_png(device_id).await?;
        let (full_w, full_h) = crate::visual::png_size(&first_full).unwrap_or((0, 0));
        let crop = match step.crop {
            Some(c) => Some(self.screenshot_crop(device_id, &first_full, c).await?),
            None => None,
        };
        let full_masks = self
            .screenshot_masks_with(device_id, &first_full, step.mask, !frozen)
            .await?;
        let masks = match &crop {
            Some(c) => crate::visual::crop_masks(&full_masks, full_w, full_h, c),
            None => full_masks,
        };
        let first = cut(&first_full, crop.as_ref())?;
        let size = crate::visual::png_size(&first);

        let place = self.baseline_place(name, (full_w, full_h), size);
        // Two names that clean to the same file would share (and overwrite)
        // one baseline (VEL-201). New names never do (they carry a hash);
        // a legacy file two names fall back to is an error.
        let key = place.read.clone().unwrap_or_else(|| place.write.clone());
        // Lower-cased: "Home" and "home" are one file on macOS and Windows.
        let key = key.to_string_lossy().to_lowercase();
        // Names that differ only in case are one file on macOS and Windows
        // but two on Linux: refuse them everywhere, so a suite recorded on
        // one OS can't silently share or split baselines on another.
        if let Some(other) = self
            .baseline_names
            .values()
            .find(|o| *o != name && o.to_lowercase() == name.to_lowercase())
        {
            return Err(VelocityError::Config(format!(
                "assertScreenshot names {other:?} and {name:?} differ only in case: both map \
                 to one baseline file on macOS and Windows; rename one"
            )));
        }
        match self.baseline_names.get(&key) {
            Some(other) if other != name => {
                return Err(VelocityError::Config(format!(
                    "assertScreenshot names {other:?} and {name:?} both map to baseline \
                     file {key}; rename one"
                )));
            }
            Some(_) => {}
            None => {
                self.baseline_names.insert(key, name.to_string());
            }
        }
        if let (Some(read), true) = (&place.read, place.legacy) {
            let note = format!(
                "assertScreenshot({name}): using baseline {} from an older layout (no \
                 <W>x<H> folder); copy it to {} (or re-record with --update-baselines)",
                read.display(),
                place.write.display()
            );
            if !self.warnings.contains(&note) {
                self.warnings.push(note);
            }
        }
        // review-fixes #3: a size folder with no baseline for the name is
        // a missing baseline for that size, even when another `<W>x<H>`
        // folder holds the name (a rotation, or a second device): `=missing`
        // records it, so a farm can add a device and a first parallel run
        // can record two sizes. A plain run fails (as for any missing
        // baseline) without writing, and says how to record the size: a
        // rotated screen must not be committed by accident.
        if let (None, Some((bw, bh)), BaselineUpdate::Off) =
            (&place.read, place.other_size, self.update_baselines)
        {
            return Err(VelocityError::AssertionFailed {
                expected: format!("screen matches baseline {name}"),
                actual: format!(
                    "size mismatch: baseline is {bw}x{bh}, capture is {full_w}x{full_h}: no \
                     baseline at this size yet (a rotated screen or another device); record \
                     it with --update-baselines=missing"
                ),
                selector: name.to_string(),
                screenshot: None,
            });
        }
        // The same size folder holds a file of another pixel size (a crop
        // whose element changed size, a file copied in by hand): a real
        // mismatch. Only `--update-baselines` (changed) replaces it.
        if let (Some(read), Some((bw, bh)), Some((cw, ch)), true) = (
            &place.read,
            place.wrong_size,
            size,
            self.update_baselines != BaselineUpdate::Changed,
        ) {
            return Err(VelocityError::AssertionFailed {
                expected: format!("screen matches baseline {name}"),
                actual: format!(
                    "size mismatch: baseline {} is {bw}x{bh}, capture is {cw}x{ch}; \
                     re-record it with --update-baselines (on this device only)",
                    read.display()
                ),
                selector: name.to_string(),
                screenshot: None,
            });
        }
        let file = place
            .write
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
        let diffs_dir = PathBuf::from(&self.config.artifacts.output_dir).join("diffs");
        let baseline = place.read.as_ref().and_then(|p| std::fs::read(p).ok());
        // An earlier run's review record for this baseline (under the
        // current file name, or the legacy one it read) is stale once a
        // comparison passes or the baseline is rewritten: accepting it
        // would put that run's old frame back over the good baseline.
        let read_name = place
            .read
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|f| f.to_string_lossy().into_owned());
        // Only records of this baseline (this size folder and platform):
        // another device's failure of the same file name stays for review.
        let review_paths: Vec<PathBuf> = std::iter::once(place.write.clone())
            .chain(place.read.clone())
            .collect();
        let clear_review = |diffs: &std::path::Path| {
            let paths: Vec<&std::path::Path> = review_paths.iter().map(PathBuf::as_path).collect();
            crate::baselines::clear_record(diffs, &file, &paths);
            if let Some(r) = read_name.as_deref().filter(|r| *r != file) {
                crate::baselines::clear_record(diffs, r, &paths);
            }
        };

        // `--update-baselines` (changed) writes; `=missing` and plain runs
        // compare what exists.
        let checking = self.update_baselines != BaselineUpdate::Changed;
        // A baseline that isn't an image can't match anything: say which
        // file, rather than "invalid PNG" after waiting for a still screen
        // (review-s8 m-f). Update mode just replaces it.
        if let (true, Some(read), Some(b)) = (checking, &place.read, &baseline) {
            if crate::visual::png_size(b).is_none() {
                return Err(VelocityError::AssertionFailed {
                    expected: format!("baseline {} to be a PNG image", read.display()),
                    actual: "a file that doesn't decode as one; re-record it with \
                             --update-baselines"
                        .into(),
                    selector: name.to_string(),
                    screenshot: None,
                });
            }
        }
        // Masks over most of the screen (or of the crop) leave nothing to
        // compare. Every frame-to-frame comparison failed on that, so the
        // step waited for a still screen it could never see and advised
        // masking more (review-s8 m-f): report the coverage itself, at once.
        if let Err(e) = compare_images_rule(&first, &first, &masks, threshold, cluster, rule) {
            return Err(match e {
                VelocityError::AssertionFailed {
                    expected,
                    actual,
                    screenshot,
                    ..
                } => VelocityError::AssertionFailed {
                    expected,
                    actual,
                    selector: name.to_string(),
                    screenshot,
                },
                e => e,
            });
        }
        let same = |a: &[u8], b: &[u8]| {
            compare_images_rule(a, b, &masks, threshold, cluster, rule).is_ok_and(|r| r.passed)
        };
        // A baseline this process created moments ago (an earlier attempt
        // of this test) is not one anybody reviewed: a retry must not pass
        // on it (D-V1).
        let unreviewed = place
            .read
            .as_ref()
            .is_some_and(|p| self.created_baselines.contains(p));
        if checking && !unreviewed && baseline.as_deref().is_some_and(|b| same(b, &first)) {
            clear_review(&diffs_dir);
            return Ok(());
        }

        let wait = self
            .step_timeout_ms
            .map(Duration::from_millis)
            .unwrap_or_else(screenshot_stable_wait);
        let expected = |f: &[u8]| checking && baseline.as_deref().is_some_and(|b| same(b, f));
        let crop_ref = crop.as_ref();
        let (previous, frame, still) = self
            .still_frame(device_id, first, wait, &same, &expected, crop_ref)
            .await?;
        if !still {
            let waited = format!("{:.1} s", wait.as_secs_f64());
            if checking && !unreviewed && baseline.as_deref().is_some_and(|b| same(b, &frame)) {
                self.warnings.push(format!(
                    "assertScreenshot({name}): the screen never held still for {waited}; \
                     its last frame matches the baseline"
                ));
                clear_review(&diffs_dir);
                return Ok(());
            }
            // Its images are now an unstable screen's: nothing to accept.
            // The record is keyed on the file read (a legacy name too).
            clear_review(&diffs_dir);
            let engine = VisualEngine::new(VisualConfig {
                diffs_dir,
                ..VisualConfig::default()
            });
            let diff = crate::visual::generate_diff_overlay_rule(&previous, &frame, &masks, rule);
            let shot = engine.save_frames(
                &file,
                &[("previous", &previous), ("actual", &frame)],
                diff.as_deref(),
            )?;
            let kept = if checking && baseline.is_some() {
                ""
            } else {
                "; not saved as the baseline"
            };
            return Err(VelocityError::AssertionFailed {
                expected: format!("a still screen to compare with baseline {name}"),
                actual: format!(
                    "the screen never held still for {waited}{kept} (mask what moves, \
                     or raise threshold/clusterPx)"
                ),
                selector: name.to_string(),
                screenshot: shot,
            });
        }

        let write_dir = place
            .write
            .parent()
            .map(|d| d.to_path_buf())
            .unwrap_or_default();
        if !checking {
            clear_review(&diffs_dir);
            VisualEngine::new(VisualConfig {
                threshold,
                rule,
                cluster_px: cluster,
                baselines_dir: write_dir,
                diffs_dir,
                update_baselines: true,
            })
            .assert_screenshot(&file, &frame, &masks)?;
            self.created_baselines.forget(&place.write);
            return Ok(());
        }
        let path = place.write.display().to_string();
        if unreviewed {
            return Err(baseline_created(name, &path));
        }
        let Some(read) = place.read.filter(|_| baseline.is_some()) else {
            // D-V1: written, then failed everywhere (no CI sniffing), so a
            // new assertion never passes without a reviewed baseline.
            // `--update-baselines=missing` writes it and passes.
            VisualEngine::new(VisualConfig {
                update_baselines: true,
                baselines_dir: write_dir,
                ..VisualConfig::default()
            })
            .assert_screenshot(&file, &frame, &[])?;
            clear_review(&diffs_dir);
            if self.update_baselines == BaselineUpdate::Missing {
                self.warnings.push(format!(
                    "assertScreenshot: created baseline {path}; review and commit it"
                ));
                return Ok(());
            }
            self.created_baselines.note(&place.write);
            return Err(baseline_created(name, &path));
        };
        let read_file = read
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
        let engine = VisualEngine::new(VisualConfig {
            threshold,
            rule,
            baselines_dir: read.parent().map(|d| d.to_path_buf()).unwrap_or_default(),
            diffs_dir: diffs_dir.clone(),
            update_baselines: false,
            cluster_px: cluster,
        });
        let r = engine.assert_screenshot(&read_file, &frame, &masks)?;
        if r.passed {
            clear_review(&diffs_dir);
            return Ok(());
        }
        let at = r
            .largest_cluster_bounds
            .map(|b| format!(" at ({}, {}) {}×{}", b.x, b.y, b.width, b.height))
            .unwrap_or_default();
        let actual = format!(
            "{:.2}% similar; largest change {} px{at}",
            r.similarity * 100.0,
            r.largest_cluster
        );
        // U1: what `velocity baselines` and the inspector's review page
        // need to accept this frame. The images are named after the file
        // read (a legacy one keeps its old name); accepting writes the
        // current layout's path at this capture size.
        let record = crate::baselines::ReviewRecord {
            name: name.to_string(),
            baseline: place.write.clone(),
            detail: actual.clone(),
            baseline_hash: None,
        };
        if let Err(e) = crate::baselines::record_failure(&diffs_dir, &read_file, &record) {
            tracing::warn!("assertScreenshot({name}): couldn't record the failure for review: {e}");
        }
        Err(VelocityError::AssertionFailed {
            expected: format!("screen matches baseline {name}"),
            actual,
            selector: name.to_string(),
            screenshot: r.diff_image_path,
        })
    }

    /// `screenshots.freezeStatusBar` (D-V4): ask the driver once per
    /// device to freeze the status bar; whether it took.
    pub(super) async fn freeze_status_bar(&mut self, device_id: &str) -> bool {
        if !self.config.screenshots.freeze_status_bar || !has_status_bar(&self.config) {
            return false;
        }
        if let Some(done) = self.status_bar_frozen.get(device_id) {
            return *done;
        }
        let done = self.driver.freeze_status_bar(device_id).await;
        if !done {
            self.warnings.push(format!(
                "screenshots.freezeStatusBar: {device_id}'s status bar couldn't be frozen; \
                 it is masked instead"
            ));
        }
        self.status_bar_frozen.insert(device_id.to_string(), done);
        done
    }

    /// Where `crop:` cuts the frame, in screenshot pixels (VT-12): the
    /// element's bounds or the rect, in screen coordinates, scaled to the
    /// capture and cut to it. An element that isn't on screen, or a rect
    /// off the screen, fails the step.
    pub(super) async fn screenshot_crop(
        &mut self,
        device_id: &str,
        png: &[u8],
        crop: &velocity_common::ScreenshotCrop,
    ) -> Result<crate::visual::MaskRegion> {
        let bounds = match crop {
            velocity_common::ScreenshotCrop::Rect(r) => *r,
            velocity_common::ScreenshotCrop::Element(sel) => {
                // Resolved as an action's target is, so the driver records
                // its match count for the ambiguity check below.
                let mut found = self.driver.find_element(device_id, sel).await.ok();
                if found.is_none() {
                    let tree = self.driver.get_hierarchy(device_id).await?;
                    found = velocity_common::selector_match::find_all_ref(&tree, sel, None)
                        .into_iter()
                        .next()
                        .cloned();
                }
                let found = found.ok_or_else(|| VelocityError::AssertionFailed {
                    expected: format!("crop target {sel} on screen"),
                    actual: "not found".into(),
                    selector: sel.to_string(),
                    screenshot: None,
                })?;
                // Several matches: the first may not be the one meant, and
                // the region compared would change with it. Warn as an
                // action does, or fail under selectors.strict (review-d5 m-2).
                self.note_if_ambiguous(device_id, sel, &found).await?;
                found.bounds
            }
        };
        let (sw, sh) = self.driver.screen_size(device_id).await?;
        let (pw, ph) = crate::visual::png_size(png).unwrap_or((sw as u32, sh as u32));
        let (fx, fy) = (pw as f64 / sw.max(1) as f64, ph as f64 / sh.max(1) as f64);
        let screen = velocity_common::Rect {
            x: 0,
            y: 0,
            width: sw,
            height: sh,
        };
        mask_region(&bounds, &screen, (fx, fy))
            .and_then(|r| crate::visual::clip_region(&r, pw, ph))
            .ok_or_else(|| VelocityError::AssertionFailed {
                expected: format!(
                    "crop ({}, {}) {}x{} on the {sw}x{sh} screen",
                    bounds.x, bounds.y, bounds.width, bounds.height
                ),
                actual: "nothing of it is on screen".into(),
                selector: "crop".into(),
                screenshot: None,
            })
    }

    /// A screenshot that decodes as a PNG. A corrupt capture (a truncated
    /// transfer) is taken again, twice; then it is a device failure (exit
    /// 3, an infrastructure failure the runner may retry elsewhere), not a
    /// configuration error (VEL-203) nor an app failure (review-s8 m-c).
    pub(super) async fn capture_png(&mut self, device_id: &str) -> Result<Vec<u8>> {
        let mut last = 0;
        for _ in 0..3 {
            let png = self.driver.screenshot_for_compare(device_id).await?;
            if crate::visual::png_size(&png).is_some() {
                return Ok(png);
            }
            last = png.len();
        }
        Err(VelocityError::ScreenCaptureFailed {
            device_id: device_id.to_string(),
            detail: format!("3 captures in a row were not PNG images (last: {last} bytes)"),
        })
    }

    /// Capture until two frames in a row are `same`, or one is `expected`
    /// (it matches the baseline), for up to `wait`: (previous frame, last
    /// frame, whether it got there). Polls at 100, 250, 500 ms, then every
    /// second, as Playwright does.
    pub(super) async fn still_frame(
        &mut self,
        device_id: &str,
        first: Vec<u8>,
        wait: Duration,
        same: &(dyn Fn(&[u8], &[u8]) -> bool + Sync),
        expected: &(dyn Fn(&[u8]) -> bool + Sync),
        crop: Option<&crate::visual::MaskRegion>,
    ) -> Result<(Vec<u8>, Vec<u8>, bool)> {
        const PAUSES: [u64; 3] = [100, 250, 500];
        let deadline = Instant::now() + wait;
        let mut previous: Option<Vec<u8>> = None;
        let mut frame = first;
        for n in 0.. {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            let pause = Duration::from_millis(PAUSES.get(n).copied().unwrap_or(1000));
            tokio::time::sleep(pause.min(left)).await;
            let next = cut(&self.capture_png(device_id).await?, crop)?;
            if expected(&next) || same(&frame, &next) {
                return Ok((frame, next, true));
            }
            previous = Some(std::mem::replace(&mut frame, next));
        }
        Ok((previous.unwrap_or_else(|| frame.clone()), frame, false))
    }

    /// Where `assertScreenshot(name)` keeps its baseline: written to
    /// `<suite dir>/velocity-baselines/<platform>[-headless]/<W>x<H>/<file>`
    /// (`-headless` so headless renders never meet device baselines, B1;
    /// `W`x`H` the full capture, so devices and orientations get their own,
    /// D-V3). Read from there, else (with a warning) from where older
    /// versions kept it: no size folder, the name's old file name, the
    /// shared platform folder for headless runs, or the folder the run
    /// started in (B2). A legacy file is used only when it is `size`, the
    /// size of what is compared.
    pub(super) fn baseline_place(
        &self,
        name: &str,
        (w, h): (u32, u32),
        size: Option<(u32, u32)>,
    ) -> BaselinePlace {
        let platform = self
            .config
            .platform
            .map(|p| p.to_string())
            .unwrap_or_else(|| "device".into());
        let headless =
            self.config.driver.unwrap_or_default() == velocity_common::DriverMode::Headless;
        let space = crate::baselines::baseline_space(&platform, headless);
        let suite_dir = self.config.suite_dir.clone().unwrap_or_default();
        let root = suite_dir.join(crate::baselines::BASELINES_DIR);
        let write = crate::baselines::baseline_path(&suite_dir, &space, (w, h), name);
        let file = baseline_file(name);
        let sized = write
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_default();
        let mut files = vec![file];
        let old = legacy_baseline_file(name);
        if !files.contains(&old) {
            files.push(old);
        }
        let current = files.iter().map(|f| sized.join(f)).find(|p| p.is_file());
        if current.is_some() {
            let wrong_size = current
                .as_ref()
                .and_then(|p| std::fs::read(p).ok())
                .and_then(|b| crate::visual::png_size(&b))
                .filter(|s| size.is_some_and(|want| want != *s));
            return BaselinePlace {
                legacy: current.as_ref().is_some_and(|r| *r != write),
                read: current,
                write,
                other_size: None,
                wrong_size,
            };
        }
        let mut dirs = vec![root.join(&space)];
        if headless {
            dirs.push(root.join(&platform));
        }
        let cwd = PathBuf::from("velocity-baselines");
        if cwd != root {
            dirs.push(cwd.join(&space));
            if headless {
                dirs.push(cwd.join(&platform));
            }
        }
        let fits = |p: &PathBuf| {
            size.is_none()
                || std::fs::read(p)
                    .ok()
                    .and_then(|b| crate::visual::png_size(&b))
                    .is_none_or(|s| Some(s) == size)
        };
        let read = dirs
            .iter()
            .flat_map(|d| files.iter().map(move |f| d.join(f)))
            .find(|p| p.is_file() && fits(p));
        // review-d5 m-3: the same name at another capture size.
        let other_size = if read.is_none() {
            let mut sizes: Vec<(u32, u32)> = std::fs::read_dir(root.join(&space))
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let (sw, sh) = name.split_once('x')?;
                    let size = (sw.parse().ok()?, sh.parse().ok()?);
                    (size != (w, h) && files.iter().any(|f| e.path().join(f).is_file()))
                        .then_some(size)
                })
                .collect();
            sizes.sort_unstable();
            sizes.first().copied()
        } else {
            None
        };
        BaselinePlace {
            legacy: read.is_some(),
            read,
            write,
            other_size,
            wrong_size: None,
        }
    }

    /// What `assertScreenshot` leaves out of the comparison: the system
    /// bars the driver reports (Android's SystemUI windows, iOS's status
    /// bar), a fixed guess only when it can't say, and the `mask` elements,
    /// each cut to the screen.
    #[cfg(test)]
    pub(super) async fn screenshot_masks(
        &mut self,
        device_id: &str,
        png: &[u8],
        mask: &[Selector],
    ) -> Result<Vec<crate::visual::MaskPreset>> {
        self.screenshot_masks_with(device_id, png, mask, true).await
    }

    /// [`Self::screenshot_masks`]; `bars: false` leaves the system bars in
    /// (their content is frozen, D-V4).
    pub(super) async fn screenshot_masks_with(
        &mut self,
        device_id: &str,
        png: &[u8],
        mask: &[Selector],
        bars: bool,
    ) -> Result<Vec<crate::visual::MaskPreset>> {
        use crate::visual::{MaskPreset, MaskRegion};
        // The system bars the driver reports (Android's SystemUI windows,
        // iOS's status bar); a fixed guess only when it can't say.
        let bars = if bars && has_status_bar(&self.config) {
            self.driver.system_bars(device_id).await
        } else {
            Some(Vec::new())
        };
        let mut masks = Vec::new();
        let guess = bars.is_none();
        if guess {
            masks.push(MaskPreset::StatusBar);
        }
        let ios = guess && self.config.platform == Some(velocity_common::Platform::Ios);
        let mut regions = bars.unwrap_or_default();
        // Each mask resolves through the driver, as actions do (web
        // `css:`/`xpath:` included, VEL-223), and every match is masked: a
        // list of rows with a timestamp each (B4). The shared matcher over
        // the tree covers drivers whose own lookup finds nothing.
        let mut tree = None;
        for sel in mask {
            let mut found: Vec<velocity_common::Rect> = self
                .driver
                .find_elements(device_id, sel)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|e| e.bounds)
                .collect();
            if found.is_empty() {
                if tree.is_none() {
                    tree = Some(self.driver.get_hierarchy(device_id).await?);
                }
                if let Some(t) = &tree {
                    found = velocity_common::selector_match::find_all_ref(t, sel, None)
                        .into_iter()
                        .map(|e| e.bounds)
                        .collect();
                }
            }
            if found.is_empty() {
                return Err(VelocityError::AssertionFailed {
                    expected: format!("mask target {sel} on screen"),
                    actual: "not found".into(),
                    selector: sel.to_string(),
                    screenshot: None,
                });
            }
            regions.extend(found);
        }
        if !regions.is_empty() || ios {
            let (sw, sh) = self.driver.screen_size(device_id).await?;
            let (pw, ph) = crate::visual::png_size(png).unwrap_or((sw as u32, sh as u32));
            // Tree coordinates are points on iOS, pixels on Android.
            let (fx, fy) = (pw as f64 / sw.max(1) as f64, ph as f64 / sh.max(1) as f64);
            if ios {
                // The status bar is up to 62 pt tall (Dynamic Island
                // iPhones): 186 px at 3x, more than the preset's 4%.
                masks.push(MaskPreset::Custom(MaskRegion {
                    x: 0,
                    y: 0,
                    width: pw,
                    height: (IOS_STATUS_BAR_PT * fy).ceil() as u32,
                }));
            }
            let screen = velocity_common::Rect {
                x: 0,
                y: 0,
                width: sw,
                height: sh,
            };
            masks.extend(
                regions
                    .iter()
                    .filter_map(|b| mask_region(b, &screen, (fx, fy)))
                    .map(MaskPreset::Custom),
            );
        }
        Ok(masks)
    }
}

/// Where an `assertScreenshot` baseline is written, and where it was found
/// (`legacy`: somewhere older versions kept it).
pub(super) struct BaselinePlace {
    pub(super) write: PathBuf,
    pub(super) read: Option<PathBuf>,
    pub(super) legacy: bool,
    /// Nothing to read at this capture size, but the name has a baseline
    /// in another `<W>x<H>` folder (a rotation or a device swap, D-V3):
    /// that size, which is a size mismatch, not a missing baseline.
    pub(super) other_size: Option<(u32, u32)>,
    /// `read` is in this size's folder but its pixel size isn't what is
    /// compared: that pixel size.
    pub(super) wrong_size: Option<(u32, u32)>,
}

/// One `assertScreenshot`'s settings, as the step gives them.
pub(super) struct ScreenshotStep<'a> {
    pub(super) threshold: Option<f64>,
    pub(super) cluster_px: Option<usize>,
    pub(super) mask: &'a [Selector],
    pub(super) rule: crate::visual::PixelRule,
    pub(super) crop: Option<&'a velocity_common::ScreenshotCrop>,
}

/// `png` cut to `crop` (the whole frame without one).
pub(super) fn cut(png: &[u8], crop: Option<&crate::visual::MaskRegion>) -> Result<Vec<u8>> {
    let Some(c) = crop else {
        return Ok(png.to_vec());
    };
    crate::visual::crop_png(png, c).ok_or_else(|| VelocityError::AssertionFailed {
        expected: format!(
            "the crop {}x{} at ({}, {}) on screen",
            c.width, c.height, c.x, c.y
        ),
        actual: "the capture no longer contains it (the screen size changed)".into(),
        selector: "crop".into(),
        screenshot: None,
    })
}

/// The failure for a baseline that was missing and has just been written.
pub(super) fn baseline_created(name: &str, path: &str) -> VelocityError {
    VelocityError::AssertionFailed {
        expected: format!("baseline {path} for {name}"),
        actual: "missing; created it from this screen. Review and commit it, then re-run \
                 (or run with --update-baselines=missing to accept new baselines)"
            .into(),
        selector: name.to_string(),
        screenshot: Some(PathBuf::from(path)),
    }
}

/// How long `assertScreenshot` waits for the screen to hold still when the
/// step sets no `timeoutMs`.
pub(super) fn screenshot_stable_wait() -> Duration {
    velocity_common::settings::ms("VELOCITY_SCREENSHOT_STABLE_MS", 5000)
}

/// The file name older versions gave `name` (lossy: `a b` and `a/b` were
/// both `a_b.png`), still read so existing baselines keep matching.
pub(super) fn legacy_baseline_file(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        + ".png"
}

/// Only a device screen has a status bar (clock, battery) to mask. A web
/// viewport or a headless render starts with the app's own header; masking
/// its top 4% let a wrong price or a broken nav bar pass assertScreenshot.
pub(super) fn has_status_bar(config: &SuiteConfig) -> bool {
    config.driver.unwrap_or_default() == velocity_common::DriverMode::Device
        && config.platform != Some(velocity_common::Platform::Web)
}

/// Whether this run is on a CI server: the shared detector, reading the
/// daemon client's environment when there is one (review MINOR 9; it read
/// the daemon's own process environment).
pub(super) fn in_ci_env() -> bool {
    velocity_common::env::in_ci()
}

/// Whether a CI variable's value says "CI": set, and not empty, `0` or
/// `false` (m7: create-react-app users set `CI=false` locally, which
/// turned on report-mode healing and refused missing baselines).
#[cfg(test)]
pub(super) fn ci_flag(value: &std::ffi::OsStr) -> bool {
    velocity_common::env::ci_flag(&value.to_string_lossy())
}

/// The part of `b` (tree coordinates) on `screen`, scaled to screenshot
/// pixels by `(fx, fy)`. A partly off-screen element used to keep its full
/// size from a clamped origin and so masked what lay past it (VEL-063).
pub(super) fn mask_region(
    b: &velocity_common::Rect,
    screen: &velocity_common::Rect,
    (fx, fy): (f64, f64),
) -> Option<crate::visual::MaskRegion> {
    let (x0, y0) = (b.x.max(screen.x), b.y.max(screen.y));
    let x1 =
        (i64::from(b.x) + i64::from(b.width)).min(i64::from(screen.x) + i64::from(screen.width));
    let y1 =
        (i64::from(b.y) + i64::from(b.height)).min(i64::from(screen.y) + i64::from(screen.height));
    let (w, h) = (x1 - i64::from(x0), y1 - i64::from(y0));
    if w <= 0 || h <= 0 {
        return None;
    }
    Some(crate::visual::MaskRegion {
        x: (f64::from(x0) * fx) as u32,
        y: (f64::from(y0) * fy) as u32,
        width: (w as f64 * fx).ceil() as u32,
        height: (h as f64 * fy).ceil() as u32,
    })
}

/// Tallest iOS status bar, in points (Dynamic Island iPhones).
pub(super) const IOS_STATUS_BAR_PT: f64 = 62.0;
