use std::collections::HashSet;

use velocity_common::{Action, Result, Selector, Step, TestSuite, VelocityError};

/// Row 3.2: every `exec` step runs a program `config.exec.allow` lists.
fn check_exec_allowed(suite: &TestSuite) -> Result<()> {
    let steps = suite
        .tests
        .iter()
        .map(|t| (t.name.as_str(), &t.steps))
        .chain(suite.flows.iter().map(|f| (f.id.as_str(), &f.steps)));
    for (name, steps) in steps {
        for (i, step) in steps.iter().enumerate() {
            if let Action::Exec(e) = &step.action {
                if let Some(why) = e.refused_by(suite.config.exec.as_ref()) {
                    return Err(at_source(
                        step,
                        VelocityError::Config(format!("{name} step {}: {why}", i + 1)),
                    ));
                }
            }
        }
    }
    Ok(())
}

pub fn validate_suite(suite: &TestSuite) -> Result<()> {
    validate_suite_at(suite, &std::collections::HashMap::new())
}

/// [`validate_suite`], with each test's `- name:` line (from
/// `parse_suite_with_positions`) so test-level errors (`dependsOn`) say
/// where the test is, as step errors do (VEL-218).
pub fn validate_suite_at(
    suite: &TestSuite,
    tests: &std::collections::HashMap<String, velocity_common::SourcePos>,
) -> Result<()> {
    validate_structure_at(suite, tests)?;
    check_exec_allowed(suite)?;
    if let Some(platform) = suite.config.platform {
        check_platform_selectors(suite, platform)?;
    }
    check_capabilities(suite)
}

/// [`validate_suite_at`] without [`check_platform_selectors`] and
/// [`check_capabilities`]: `velocity run` checks those once `--platform`
/// and `--driver` have been applied, so a `driver: headless` suite run
/// with `--driver device` isn't refused for what only headless can't do
/// (review-s9 M2), nor a `platform: android` suite with web-only
/// selectors run with `--platform web` (review f5-f6 MINOR-7).
pub fn validate_structure_at(
    suite: &TestSuite,
    tests: &std::collections::HashMap<String, velocity_common::SourcePos>,
) -> Result<()> {
    // Results, history, the cache and `dependsOn` are keyed by test name,
    // and `resolve_flows` kept the last of two flows with one id: both
    // validated (VEL-195).
    let mut names = HashSet::new();
    for test in &suite.tests {
        if !names.insert(test.name.as_str()) {
            return Err(VelocityError::Config(format!(
                "two tests are named '{}'; test names must be unique (results and dependsOn use them)",
                test.name
            )));
        }
    }
    // D-W3: an unknown device profile lists the known ones.
    if let Some(device) = suite.config.web.as_ref().and_then(|w| w.device.as_deref()) {
        velocity_common::web_devices::lookup(device)?;
    }
    let mut flow_ids = HashSet::new();
    for flow in &suite.flows {
        if !flow_ids.insert(flow.id.as_str()) {
            return Err(VelocityError::Config(format!(
                "two flows have the id '{}'; flow ids must be unique",
                flow.id
            )));
        }
    }

    for test in &suite.tests {
        for (i, step) in test.steps.iter().enumerate() {
            validate_step(step, &test.name, i, &flow_ids).map_err(|e| at_source(step, e))?;
        }
    }

    for flow in &suite.flows {
        for (i, step) in flow.steps.iter().enumerate() {
            validate_step(step, &flow.id, i, &flow_ids).map_err(|e| at_source(step, e))?;
        }
    }

    // `appId` may be left out only where nothing launches an app or opens
    // a page.
    if suite.app_id.trim().is_empty() && !crate::http_only(suite) {
        return Err(VelocityError::Config(
            "the suite has no `appId`; only a suite of http steps (with wait/setConfig) runs without one"
                .into(),
        ));
    }

    validate_dependencies(suite).map_err(|(test, e)| match tests.get(test) {
        Some(pos) => VelocityError::AtSource {
            pos: pos.clone(),
            step: format!("test '{test}' dependsOn"),
            inner: Box::new(e),
        },
        None => e,
    })
}

/// The config error for `framework: react_native` with `driver: headless`
/// (D-44/45: the RN headless sidecar, a stub that rendered nothing, was
/// removed). React Native runs as a device profile with the native probe.
pub const RN_NO_HEADLESS: &str = "React Native has no headless renderer; run on a device or \
     simulator (drop `driver: headless` / `--driver headless`, or set `framework: native` \
     to use the static layout renderer)";

/// Steps the suite's platform and driver can never run (VEL-219): a
/// configuration error before the run, instead of one mid-run.
pub fn check_capabilities(suite: &TestSuite) -> Result<()> {
    use velocity_common::config::{DriverMode, Framework};
    if suite.config.driver == Some(DriverMode::Headless)
        && suite.config.framework == Some(Framework::ReactNative)
    {
        return Err(VelocityError::Config(RN_NO_HEADLESS.to_string()));
    }
    for (name, i, step, can) in capabilities(suite) {
        if let Can::Never(why) = can {
            return Err(at_source(
                step,
                VelocityError::Config(format!("test '{name}' step {i}: {why}")),
            ));
        }
    }
    check_keys(suite)
}

/// The keys the headless driver accepts (as no-ops): the ones it had
/// before D-9. The editing keys are refused.
pub const HEADLESS_KEYS: [velocity_common::Key; 5] = [
    velocity_common::Key::Back,
    velocity_common::Key::Home,
    velocity_common::Key::Enter,
    velocity_common::Key::VolumeUp,
    velocity_common::Key::VolumeDown,
];

/// A `pressKey` the platform's device driver can't send (D-9): iOS has no
/// escape, forward delete or arrow keys through WebDriverAgent, a browser
/// no volume buttons. They were skipped silently on the web.
fn check_keys(suite: &TestSuite) -> Result<()> {
    use velocity_common::config::{DriverMode, Framework};
    let tests = suite.tests.iter().map(|t| (t.name.as_str(), &t.steps));
    let flows = suite.flows.iter().map(|f| (f.id.as_str(), &f.steps));
    if suite.config.driver == Some(DriverMode::Headless) {
        // The headless renderer has no keyboard: the D-9 editing keys did
        // nothing and the step passed (review-d6 m7). The older keys keep
        // their no-op, which suites rely on.
        for (name, steps) in tests.chain(flows) {
            for (i, step) in steps.iter().enumerate() {
                if let Action::PressKey { key } = &step.action {
                    if !HEADLESS_KEYS.contains(key) {
                        return Err(at_source(
                            step,
                            VelocityError::Config(format!(
                                "test '{name}' step {i}: pressKey `{}` can't be sent by the headless driver (it has no keyboard); use inputText, or run on a device or the web",
                                key.name()
                            )),
                        ));
                    }
                }
            }
        }
        return Ok(());
    }
    let Some(platform) = suite.config.platform else {
        return Ok(());
    };
    if matches!(
        suite.config.framework,
        Some(Framework::ReactNative | Framework::Flutter)
    ) {
        return Ok(());
    }
    for (name, steps) in tests.chain(flows) {
        for (i, step) in steps.iter().enumerate() {
            if let Action::PressKey { key } = &step.action {
                if !key.supported_on(platform) {
                    let can: Vec<&str> = velocity_common::Key::ALL
                        .iter()
                        .filter(|k| k.supported_on(platform))
                        .map(|k| k.name())
                        .collect();
                    return Err(at_source(
                        step,
                        VelocityError::Config(format!(
                            "test '{name}' step {i}: pressKey `{}` can't be sent on {platform} (it can send: {})",
                            key.name(),
                            can.join(", ")
                        )),
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Things that are allowed but almost certainly not what was meant. The
/// CLI prints them with `validate` and `run`; they never fail a suite.
pub fn suite_warnings(suite: &TestSuite) -> Vec<String> {
    let mut out = Vec::new();
    if suite.tests.is_empty() {
        out.push("the suite has no tests".to_string());
    }
    if suite.config.react_native.is_some() {
        out.push(
            "`reactNative:` is ignored: the React Native headless sidecar was removed \
             (React Native runs on a device or simulator); delete the block"
                .to_string(),
        );
    }
    for test in &suite.tests {
        // Passes without checking anything (VEL-195).
        if test.steps.is_empty() {
            out.push(format!(
                "test '{}' has no steps (it always passes)",
                test.name
            ));
        }
    }
    for (name, i, step, can) in capabilities(suite) {
        if let Can::Maybe(why) = can {
            out.push(format!("{}'{name}' step {i}: {why}", at_prefix(step)));
        }
    }
    let tests = suite.tests.iter().map(|t| (t.name.as_str(), &t.steps));
    let flows = suite.flows.iter().map(|f| (f.id.as_str(), &f.steps));
    let web = suite.config.platform == Some(velocity_common::Platform::Web);
    for (name, steps) in tests.chain(flows) {
        for (i, step) in steps.iter().enumerate() {
            // D-W4: one release of notice that the web flipped, a swipe
            // with no direction (down) included (review-d6 m4).
            if let Action::Swipe {
                direction,
                from: None,
                to: None,
                within: None,
            } = &step.action
            {
                if web {
                    let d = format!(
                        "{:?}",
                        direction.unwrap_or(velocity_common::Direction::Down)
                    )
                    .to_lowercase();
                    let what = match direction {
                        Some(_) => format!("`swipe: {d}`"),
                        None => "a `swipe` with no `direction` (down)".to_string(),
                    };
                    out.push(format!(
                        "{}'{name}' step {i}: on the web {what} is now the finger's direction, as on devices (up scrolls the page down); before, it scrolled the page {d}",
                        at_prefix(step),
                    ));
                    continue;
                }
            }
            if let Action::Swipe {
                direction: None,
                from: None,
                to: None,
                ..
            } = &step.action
            {
                out.push(format!(
                    "{}'{name}' step {i}: swipe has no `direction`; it swipes down",
                    at_prefix(step)
                ));
            }
            // D-9: `back` after typing deleted a character on the web; it
            // is history back now (review-d6 m1).
            if web
                && i > 0
                && matches!(
                    step.action,
                    Action::PressKey {
                        key: velocity_common::Key::Back
                    }
                )
                && matches!(
                    steps[i - 1].action,
                    Action::InputText { .. } | Action::InputRaw { .. }
                )
            {
                out.push(format!(
                    "{}'{name}' step {i}: on the web `back` is the browser's history back, not a deleted character; use `pressKey: backspace` to delete what was typed",
                    at_prefix(step)
                ));
            }
        }
    }
    out
}

/// Whether the suite's platform and driver can run a step.
#[derive(Debug, PartialEq)]
enum Can {
    Yes,
    /// It may not work there: a warning.
    Maybe(&'static str),
    /// No driver for that platform implements it: an error.
    Never(&'static str),
}

/// What the suite's `platform`, `driver` and `framework` say about each
/// step that only some drivers can run. These validated, then failed at
/// the step as configuration errors after the earlier steps ran
/// (VEL-219). goBack/refresh, css/xpath and assertSystemState off Android
/// are [`check_platform_selectors`]'s.
fn capabilities(suite: &TestSuite) -> Vec<(&str, usize, &Step, Can)> {
    use velocity_common::config::{DriverMode, Framework};
    use velocity_common::Platform;
    let platform = suite.config.platform;
    let headless = suite.config.driver == Some(DriverMode::Headless);
    let bridge = matches!(
        suite.config.framework,
        Some(Framework::ReactNative | Framework::Flutter)
    );
    let device = matches!(platform, Some(Platform::Android | Platform::Ios));
    let can = |action: &Action| -> Can {
        match action {
            Action::MockRequest { .. } if headless || device => {
                Can::Never("mockRequest works only on the web")
            }
            Action::ClearMocks if headless || device => {
                Can::Maybe("clearMocks does nothing off the web")
            }
            Action::TapXY { .. } | Action::InputRaw { .. } | Action::Goto { .. } if headless => {
                Can::Never("the headless driver can't run tapXY, inputRaw or openLink/goto")
            }
            Action::SaveState { .. } | Action::RestoreState { .. }
                if headless || platform == Some(Platform::Web) =>
            {
                Can::Never("saveState/restoreState work only on an Android or iOS device")
            }
            Action::AssertSystemState { .. } if headless => {
                Can::Never("the headless driver can't read system state (assertSystemState)")
            }
            Action::TapXY { .. }
            | Action::InputRaw { .. }
            | Action::Goto { .. }
            | Action::SaveState { .. }
            | Action::RestoreState { .. }
                if bridge =>
            {
                Can::Maybe("the React Native/Flutter driver may not support this step")
            }
            _ => Can::Yes,
        }
    };
    let tests = suite.tests.iter().map(|t| (t.name.as_str(), &t.steps));
    let flows = suite.flows.iter().map(|f| (f.id.as_str(), &f.steps));
    let mut out = Vec::new();
    for (name, steps) in tests.chain(flows) {
        for (i, step) in steps.iter().enumerate() {
            let c = can(&step.action);
            if c != Can::Yes {
                out.push((name, i, step, c));
            }
        }
    }
    out
}

/// `line N: ` for a step the parser placed, else nothing.
fn at_prefix(step: &Step) -> String {
    step.source
        .as_ref()
        .map(|p| format!("line {}: ", p.line))
        .unwrap_or_default()
}

/// CSS and XPath selectors are resolved by the browser: on a device they
/// never match, so a step would wait out its timeout and report "not
/// found". Refuse them up front for `platform`.
pub fn check_platform_selectors(
    suite: &TestSuite,
    platform: velocity_common::Platform,
) -> Result<()> {
    // Device state checks read Android services (media sessions, telephony).
    if platform != velocity_common::Platform::Android {
        let tests = suite.tests.iter().map(|t| (t.name.as_str(), &t.steps));
        let flows = suite.flows.iter().map(|f| (f.id.as_str(), &f.steps));
        for (name, steps) in tests.chain(flows) {
            for (i, step) in steps.iter().enumerate() {
                if matches!(
                    step.action,
                    velocity_common::Action::AssertSystemState { .. }
                ) {
                    return Err(at_source(
                        step,
                        VelocityError::Config(format!(
                            "test '{name}' step {i}: `assertSystemState` works only on Android"
                        )),
                    ));
                }
            }
        }
    }
    if platform == velocity_common::Platform::Web {
        return Ok(());
    }
    let tests = suite.tests.iter().map(|t| (t.name.as_str(), &t.steps));
    let flows = suite.flows.iter().map(|f| (f.id.as_str(), &f.steps));
    for (name, steps) in tests.chain(flows) {
        for (i, step) in steps.iter().enumerate() {
            // Browser navigation has no device equivalent in the drivers: it
            // used to pass validation and fail mid-run with "go_back() not
            // supported on this platform".
            let browser_only = match &step.action {
                velocity_common::Action::GoBack => {
                    Some(("goBack", "use `pressKey: { key: back }`"))
                }
                velocity_common::Action::Refresh => Some(("refresh", "relaunch with `launchApp`")),
                _ => None,
            };
            if let Some((action, instead)) = browser_only {
                return Err(at_source(
                    step,
                    VelocityError::Config(format!(
                        "test '{name}' step {i}: `{action}` works only on the web; on {platform} {instead}"
                    )),
                ));
            }
            for sel in crate::selector_lint::step_selectors(&step.action) {
                if let Some(kind) = web_only(sel) {
                    return Err(at_source(
                        step,
                        VelocityError::InvalidSelector {
                            test_name: name.to_string(),
                            step_index: i,
                            reason: format!(
                                "{kind} selectors work only on the web; on {platform} they never match"
                            ),
                        },
                    ));
                }
            }
        }
    }
    Ok(())
}

/// `css` / `xpath` if `sel` is, or contains, a web-only selector.
fn web_only(sel: &Selector) -> Option<&'static str> {
    match sel {
        Selector::Css(_) => Some("css"),
        Selector::XPath(_) => Some("xpath"),
        Selector::Index { selector, .. } => web_only(selector),
        Selector::Compound(all) => all.iter().find_map(web_only),
        Selector::Relative { target, anchor, .. } => web_only(target).or_else(|| web_only(anchor)),
        _ => None,
    }
}

/// Point a step's error at its line in the suite, when the parser knew it.
fn at_source(step: &Step, e: VelocityError) -> VelocityError {
    match &step.source {
        Some(pos) => VelocityError::AtSource {
            pos: pos.clone(),
            step: crate::executor::step_label(&step.action),
            inner: Box::new(e),
        },
        None => e,
    }
}

/// `dependsOn` must name tests in this suite and must not form a cycle.
/// The error comes with the test it is about.
fn validate_dependencies(suite: &TestSuite) -> std::result::Result<(), (&str, VelocityError)> {
    let names: HashSet<&str> = suite.tests.iter().map(|t| t.name.as_str()).collect();
    for test in &suite.tests {
        for dep in &test.depends_on {
            if !names.contains(dep.as_str()) {
                return Err((
                    test.name.as_str(),
                    VelocityError::Config(format!(
                        "test '{}' depends on '{dep}', which is not a test in this suite",
                        test.name
                    )),
                ));
            }
        }
    }
    // Depth-first search for a back edge.
    let deps: std::collections::HashMap<&str, &[String]> = suite
        .tests
        .iter()
        .map(|t| (t.name.as_str(), t.depends_on.as_slice()))
        .collect();
    fn visit<'a>(
        name: &'a str,
        deps: &std::collections::HashMap<&'a str, &'a [String]>,
        path: &mut Vec<&'a str>,
        done: &mut HashSet<&'a str>,
    ) -> std::result::Result<(), String> {
        if done.contains(name) {
            return Ok(());
        }
        if let Some(i) = path.iter().position(|n| *n == name) {
            let mut cycle: Vec<&str> = path[i..].to_vec();
            cycle.push(name);
            return Err(cycle.join(" -> "));
        }
        path.push(name);
        for dep in deps.get(name).copied().unwrap_or_default() {
            visit(dep, deps, path, done)?;
        }
        path.pop();
        done.insert(name);
        Ok(())
    }
    let mut done = HashSet::new();
    for test in &suite.tests {
        visit(&test.name, &deps, &mut Vec::new(), &mut done).map_err(|cycle| {
            (
                test.name.as_str(),
                VelocityError::Config(format!("dependsOn cycle: {cycle}")),
            )
        })?;
    }
    Ok(())
}

fn validate_step(
    step: &Step,
    context_name: &str,
    step_index: usize,
    flow_ids: &HashSet<&str>,
) -> Result<()> {
    match &step.action {
        Action::MockRequest { rule } => {
            let bodies = [
                rule.json.is_some(),
                rule.body.is_some(),
                rule.file.is_some(),
            ]
            .iter()
            .filter(|b| **b)
            .count();
            if rule.url.is_empty() || bodies > 1 || (rule.block && bodies > 0) {
                return Err(VelocityError::Config(format!(
                    "{context_name} step {step_index}: mockRequest needs a url and at most one of json/body/file (none with block)"
                )));
            }
        }
        Action::ClearMocks => {}
        // The allow-list needs the suite's config: checked in
        // `check_exec_allowed`.
        Action::Exec(_) => {}
        Action::Http(req) => {
            velocity_api::validate(req).map_err(|e| {
                VelocityError::Config(format!("{context_name} step {step_index}: http: {e}"))
            })?;
        }
        Action::SetConfig { values } => {
            for (k, v) in values {
                if let Err(e) = crate::executor::check_set_config(k, v) {
                    return Err(VelocityError::Config(format!(
                        "{context_name} step {step_index}: {e}"
                    )));
                }
            }
        }
        Action::AssertSystemState {
            media_playing,
            media_title,
            in_call,
        } => {
            if media_playing.is_none() && media_title.is_none() && in_call.is_none() {
                return Err(VelocityError::Config(format!(
                    "{context_name} step {step_index}: assertSystemState needs mediaPlaying, mediaTitle or inCall"
                )));
            }
        }
        Action::Tap { selector }
        | Action::DoubleTap { selector }
        | Action::ClearText { selector }
        | Action::AssertVisible { selector }
        | Action::AssertNotVisible { selector } => {
            validate_selector(selector, context_name, step_index)?;
        }
        Action::LongPress { selector, .. } => {
            validate_selector(selector, context_name, step_index)?;
        }
        Action::Drag { from, to, .. } => {
            validate_selector(from, context_name, step_index)?;
            validate_selector(to, context_name, step_index)?;
        }
        Action::InputText { selector, .. } => {
            validate_selector(selector, context_name, step_index)?;
        }
        Action::AssertText { selector, .. } => {
            validate_selector(selector, context_name, step_index)?;
        }
        Action::ScrollUntilVisible {
            selector,
            max_scrolls,
            within,
            ..
        } => {
            validate_selector(selector, context_name, step_index)?;
            if let Some(w) = within {
                validate_selector(w, context_name, step_index)?;
            }
            // Zero scrolls can only fail when the element is off screen
            // (VEL-195).
            if *max_scrolls == 0 {
                return Err(VelocityError::Config(format!(
                    "{context_name} step {step_index}: scrollUntilVisible maxScrolls must be at least 1 (use assertVisible to check without scrolling)"
                )));
            }
        }
        Action::Swipe {
            from, to, within, ..
        } => {
            if let Some(w) = within {
                validate_selector(w, context_name, step_index)?;
            }
            // `from` alone (or `to` alone) was dropped and the step swiped
            // down instead (VEL-195).
            if from.is_some() != to.is_some() {
                return Err(VelocityError::Config(format!(
                    "{context_name} step {step_index}: swipe needs both `from` and `to` (or a `direction`)"
                )));
            }
        }
        Action::RunFlow { flow_id } => {
            if !flow_ids.contains(flow_id.as_str()) {
                return Err(VelocityError::UnknownFlowRef {
                    flow_id: flow_id.clone(),
                    test_name: context_name.to_string(),
                });
            }
        }
        // Not selectors: these said "Invalid selector in step 0" (VEL-218).
        Action::Natural { intent, .. } => {
            if intent.trim().is_empty() {
                return Err(VelocityError::Config(format!(
                    "{context_name} step {step_index}: natural-language step has an empty instruction"
                )));
            }
        }
        Action::AssertScreenshot {
            name,
            threshold,
            mask,
            tolerance,
            comparator,
            crop,
            ..
        } => {
            // `tolerance` is the strict rule's; perceptual has its own.
            if tolerance.is_some() && *comparator == Some(velocity_common::Comparator::Perceptual) {
                return Err(VelocityError::Config(format!(
                    "{context_name} step {step_index}: assertScreenshot tolerance applies to \
                     comparator: strict only (perceptual uses a fixed YIQ threshold of 0.1)"
                )));
            }
            if let Some(velocity_common::ScreenshotCrop::Element(sel)) = crop {
                validate_selector(sel, context_name, step_index)?;
            }
            if name.trim().is_empty() {
                return Err(VelocityError::Config(format!(
                    "{context_name} step {step_index}: assertScreenshot needs a baseline name"
                )));
            }
            if threshold.is_some_and(|t| !(0.0..=1.0).contains(&t)) {
                return Err(VelocityError::Config(format!(
                    "{context_name} step {step_index}: assertScreenshot threshold must be between 0 and 1"
                )));
            }
            for sel in mask {
                validate_selector(sel, context_name, step_index)?;
            }
        }
        Action::SaveState { .. } | Action::RestoreState { .. } => {}
        Action::LaunchApp { .. }
        | Action::StopApp { .. }
        | Action::Screenshot { .. }
        | Action::PressKey { .. }
        | Action::Wait { .. }
        | Action::TapXY { .. }
        | Action::InputRaw { .. }
        | Action::Goto { .. }
        | Action::GoBack
        | Action::Refresh => {}
    }
    Ok(())
}

/// `s` with each `${...}` reference replaced by a plain word, so a pattern
/// that takes a value from the environment can be checked before it has one.
/// `s` with each `${VAR}` replaced by a word, as `textMatches` /
/// `idMatches` patterns are checked (and `velocity migrate` checks them).
pub fn without_env_refs(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        let Some(len) = rest[start..].find('}') else {
            break;
        };
        out.push_str(&rest[..start]);
        out.push('x');
        rest = &rest[start + len + 1..];
    }
    out.push_str(rest);
    out
}

fn validate_selector(selector: &Selector, test_name: &str, step_index: usize) -> Result<()> {
    match selector {
        Selector::Id(v)
        | Selector::Text(v)
        | Selector::TextContains(v)
        | Selector::AccessibilityId(v)
        | Selector::ClassName(v) => {
            if v.trim().is_empty() {
                return Err(VelocityError::InvalidSelector {
                    test_name: test_name.to_string(),
                    step_index,
                    reason: "selector value is empty".to_string(),
                });
            }
        }
        // D-11: an invalid pattern is a validation error, not a lookup
        // that never matches. `${VAR}` references are checked as a word.
        Selector::TextMatches(v) | Selector::IdMatches(v) => {
            let kind = if matches!(selector, Selector::TextMatches(_)) {
                "textMatches"
            } else {
                "idMatches"
            };
            if let Err(why) = velocity_common::selector_match::compile_pattern(&without_env_refs(v))
            {
                return Err(VelocityError::InvalidSelector {
                    test_name: test_name.to_string(),
                    step_index,
                    reason: format!("{kind} {v:?} is not a valid regular expression: {why}"),
                });
            }
        }
        Selector::Index {
            selector: inner, ..
        } => {
            validate_selector(inner, test_name, step_index)?;
        }
        Selector::Compound(selectors) => {
            if selectors.is_empty() {
                return Err(VelocityError::InvalidSelector {
                    test_name: test_name.to_string(),
                    step_index,
                    reason: "compound selector has no children".to_string(),
                });
            }
            // A state filter part has nothing of its own to check; the
            // whole compound is checked below.
            for s in selectors.iter().filter(|s| !s.is_state_filter()) {
                validate_selector(s, test_name, step_index)?;
            }
        }
        Selector::Css(v) | Selector::XPath(v) => {
            if v.trim().is_empty() {
                return Err(VelocityError::InvalidSelector {
                    test_name: test_name.to_string(),
                    step_index,
                    reason: "css/xpath selector value is empty".to_string(),
                });
            }
        }
        Selector::Relative { target, anchor, .. } => {
            validate_selector(target, test_name, step_index)?;
            validate_selector(anchor, test_name, step_index)?;
        }
        Selector::Role(v) => {
            if let Some(reason) = role_problem(v) {
                return Err(VelocityError::InvalidSelector {
                    test_name: test_name.to_string(),
                    step_index,
                    reason,
                });
            }
        }
        Selector::Name(v) => {
            if v.trim().is_empty() {
                return Err(VelocityError::InvalidSelector {
                    test_name: test_name.to_string(),
                    step_index,
                    reason: "name selector value is empty".to_string(),
                });
            }
        }
        Selector::Checked(_)
        | Selector::Focused(_)
        | Selector::Selected(_)
        | Selector::Enabled(_) => {}
    }
    if only_state_filters(selector) {
        return Err(VelocityError::InvalidSelector {
            test_name: test_name.to_string(),
            step_index,
            reason: "checked, focused, selected and enabled narrow a selector; add id, text, role or name".to_string(),
        });
    }
    Ok(())
}

/// A selector made of state filters alone (S2): it would match every
/// enabled (or checked …) element on screen.
fn only_state_filters(selector: &Selector) -> bool {
    match selector {
        Selector::Compound(parts) => {
            !parts.is_empty() && parts.iter().all(Selector::is_state_filter)
        }
        s => s.is_state_filter(),
    }
}

/// Why `role` isn't a role a `role:` selector can match, if it isn't:
/// empty, or not one of Playwright's role names (with the nearest one).
/// A `${VAR}` reference is checked when it runs.
pub fn role_problem(role: &str) -> Option<String> {
    let r = role.trim();
    if r.is_empty() {
        return Some("role selector value is empty".to_string());
    }
    if r.contains("${") || velocity_common::role::is_role(&r.to_ascii_lowercase()) {
        return None;
    }
    let near = velocity_common::role::ROLES
        .iter()
        .min_by_key(|c| crate::config_keys::distance(c, &r.to_ascii_lowercase()))
        .copied()
        .unwrap_or("button");
    Some(format!(
        "unknown role {r:?} (did you mean {near:?}?); roles are Playwright's: button, checkbox, textbox, link, heading, …"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use velocity_common::{Flow, SuiteConfig, TestCase};

    /// SUM-4: `web.settle` parses with its three timings; a typo in it is
    /// refused rather than ignored.
    #[test]
    fn web_settle_parses_and_refuses_unknown_keys() {
        let yaml = |settle: &str| {
            format!(
                "appId: x\nconfig:\n  platform: web\n  web:\n    settle: {settle}\ntests:\n  - name: t\n    steps:\n      - wait: {{ ms: 1 }}\n"
            )
        };
        let suite =
            crate::parse_suite_from_str(&yaml("{ reactionMs: 40, quietMs: 60, capMs: 900 }"))
                .unwrap();
        let ws = suite.config.web.unwrap().settle.unwrap();
        assert_eq!(
            (ws.reaction_ms, ws.quiet_ms, ws.cap_ms),
            (Some(40), Some(60), Some(900))
        );
        assert!(crate::parse_suite_from_str(&yaml("{ reactMs: 40 }")).is_err());
    }

    /// D-W3: `web.device` names a known profile; an unknown one is a
    /// config error that lists the known names.
    #[test]
    fn an_unknown_web_device_is_a_config_error() {
        let suite = |device: &str| {
            crate::parse_suite_from_str(&format!(
                "appId: x\nconfig:\n  platform: web\n  web:\n    device: \"{device}\"\ntests:\n  - name: t\n    steps:\n      - wait: {{ ms: 1 }}\n"
            ))
            .unwrap()
        };
        validate_suite(&suite("Pixel 7")).unwrap();
        validate_suite(&suite("iphone 15")).unwrap();
        let e = validate_suite(&suite("Nokia 3310")).unwrap_err();
        assert!(matches!(e, VelocityError::Config(_)), "{e}");
        assert!(e.to_string().contains("Pixel 7"), "{e}");
    }

    /// D-W4: a web suite's `swipe: <direction>` gets a one-release note
    /// that the direction is the finger's now; a device suite's doesn't.
    #[test]
    fn a_web_swipe_direction_is_flagged_for_one_release() {
        let suite = |platform: &str| {
            crate::parse_suite_from_str(&format!(
                "appId: x\nconfig:\n  platform: {platform}\ntests:\n  - name: t\n    steps:\n      - swipe: {{ direction: up }}\n"
            ))
            .unwrap()
        };
        let w = suite_warnings(&suite("web"));
        assert!(w.iter().any(|w| w.contains("finger's direction")), "{w:?}");
        assert!(suite_warnings(&suite("android")).is_empty());
    }

    /// review-d6 m4: a web swipe with no direction (down) flipped too; it
    /// got only the "it swipes down" warning, which misleads on the web.
    #[test]
    fn a_web_swipe_without_direction_gets_the_flip_note() {
        let suite = |platform: &str| {
            crate::parse_suite_from_str(&format!(
                "appId: x\nconfig:\n  platform: {platform}\ntests:\n  - name: t\n    steps:\n      - swipe: {{}}\n"
            ))
            .unwrap()
        };
        let w = suite_warnings(&suite("web"));
        assert!(
            w.iter()
                .any(|w| w.contains("no `direction` (down) is now the finger's direction")),
            "{w:?}"
        );
        assert!(!w.iter().any(|w| w.contains("it swipes down")), "{w:?}");
        let w = suite_warnings(&suite("android"));
        assert!(w.iter().any(|w| w.contains("it swipes down")), "{w:?}");
    }

    /// review-d6 m1: on the web `back` right after typing is history back
    /// now (D-9), not a deleted character: warned, web only.
    #[test]
    fn web_back_right_after_typing_is_warned_about() {
        let suite = |platform: &str, steps: &str| {
            crate::parse_suite_from_str(&format!(
                "appId: x\nconfig:\n  platform: {platform}\ntests:\n  - name: t\n    steps:\n{steps}"
            ))
            .unwrap()
        };
        let typed = "      - inputText: { selector: { id: q }, text: abc }\n      - back\n";
        let w = suite_warnings(&suite("web", typed));
        assert!(
            w.iter().any(|w| w.contains("step 1: on the web `back`")),
            "{w:?}"
        );
        assert!(suite_warnings(&suite("android", typed)).is_empty());
        let later = "      - inputText: { selector: { id: q }, text: abc }\n      - wait: { ms: 1 }\n      - back\n";
        assert!(suite_warnings(&suite("web", later)).is_empty());
    }

    /// review-d6 m7: the headless driver has no keyboard. The D-9 editing
    /// keys passed silently; now they are config errors. The older keys
    /// still validate.
    #[test]
    fn headless_refuses_the_editing_keys() {
        let suite = |key: &str| {
            crate::parse_suite_from_str(&format!(
                "appId: x\nconfig:\n  platform: android\n  driver: headless\ntests:\n  - name: t\n    steps:\n      - pressKey: {{ key: {key} }}\n"
            ))
            .unwrap()
        };
        for key in ["backspace", "tab", "escape", "delete", "arrowUp"] {
            let e = validate_suite(&suite(key)).unwrap_err();
            let e = e.to_string();
            assert!(e.contains("Configuration error"), "{key}: {e}");
            assert!(e.contains("headless"), "{e}");
        }
        for key in ["back", "home", "enter", "volumeUp"] {
            validate_suite(&suite(key)).unwrap();
        }
    }

    /// D-9: a key the platform's driver can't send is a config error
    /// before the run; the same key on a platform that has it validates.
    #[test]
    fn a_key_the_platform_cannot_send_is_a_config_error() {
        let suite = |platform: &str, key: &str| {
            crate::parse_suite_from_str(&format!(
                "appId: x\nconfig:\n  platform: {platform}\ntests:\n  - name: t\n    steps:\n      - pressKey: {{ key: {key} }}\n"
            ))
            .unwrap()
        };
        let e = validate_suite(&suite("ios", "arrowUp"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("arrowUp") && e.contains("ios"), "{e}");
        let e = validate_suite(&suite("web", "volumeUp"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("volumeUp"), "{e}");
        for (p, k) in [
            ("android", "arrowUp"),
            ("android", "escape"),
            ("ios", "backspace"),
            ("ios", "tab"),
            ("web", "escape"),
            ("web", "back"),
        ] {
            validate_suite(&suite(p, k)).unwrap_or_else(|e| panic!("{p} {k}: {e}"));
        }
    }

    fn make_suite(flows: Vec<Flow>, tests: Vec<TestCase>) -> TestSuite {
        TestSuite {
            app_id: "com.test".to_string(),
            config: SuiteConfig::default(),
            flows,
            tests,
        }
    }

    fn step(action: Action) -> Step {
        Step {
            action,
            timeout_ms: None,
            settle: None,
            source: None,
            modifiers: Default::default(),
        }
    }

    #[test]
    fn valid_suite_passes() {
        let suite = make_suite(
            vec![Flow {
                id: "login".to_string(),
                steps: vec![step(Action::Tap {
                    selector: Selector::Id("btn".to_string()),
                })],
            }],
            vec![TestCase {
                name: "test1".to_string(),
                tags: vec![],
                isolated: false,
                depends_on: vec![],
                steps: vec![
                    step(Action::RunFlow {
                        flow_id: "login".to_string(),
                    }),
                    step(Action::AssertVisible {
                        selector: Selector::Text("Welcome".to_string()),
                    }),
                ],
                description: None,
                verify: None,
            }],
        );
        assert!(validate_suite(&suite).is_ok());
    }

    #[test]
    fn unknown_flow_ref_fails() {
        let suite = make_suite(
            vec![],
            vec![TestCase {
                name: "test1".to_string(),
                tags: vec![],
                isolated: false,
                depends_on: vec![],
                steps: vec![step(Action::RunFlow {
                    flow_id: "nonexistent".to_string(),
                })],
                description: None,
                verify: None,
            }],
        );
        let err = validate_suite(&suite).unwrap_err();
        match err {
            VelocityError::UnknownFlowRef { flow_id, .. } => {
                assert_eq!(flow_id, "nonexistent");
            }
            other => panic!("expected UnknownFlowRef, got {:?}", other),
        }
    }

    #[test]
    fn empty_selector_fails() {
        let suite = make_suite(
            vec![],
            vec![TestCase {
                name: "test1".to_string(),
                tags: vec![],
                isolated: false,
                depends_on: vec![],
                steps: vec![step(Action::Tap {
                    selector: Selector::Id("".to_string()),
                })],
                description: None,
                verify: None,
            }],
        );
        let err = validate_suite(&suite).unwrap_err();
        match err {
            VelocityError::InvalidSelector { reason, .. } => {
                assert!(reason.contains("empty"));
            }
            other => panic!("expected InvalidSelector, got {:?}", other),
        }
    }

    /// S1: an unknown role is a validation error naming the nearest one;
    /// a `${VAR}` role is checked when it runs. S2: a compound of state
    /// filters alone (as MCP could build) is refused.
    #[test]
    fn unknown_roles_and_bare_state_filters_fail() {
        let bad = |sel: Selector| {
            let suite = make_suite(
                vec![],
                vec![TestCase {
                    name: "t".to_string(),
                    tags: vec![],
                    isolated: false,
                    depends_on: vec![],
                    steps: vec![step(Action::Tap { selector: sel })],
                    description: None,
                    verify: None,
                }],
            );
            validate_suite(&suite).err().map(|e| e.to_string())
        };
        let msg = bad(Selector::Role("buton".into())).unwrap();
        assert!(msg.contains("did you mean \"button\""), "{msg}");
        assert!(
            bad(Selector::Role("Button".into())).is_none(),
            "case-insensitive"
        );
        assert!(bad(Selector::Role("${ROLE}".into())).is_none());
        assert!(bad(Selector::Name("  ".into())).is_some());
        assert!(bad(Selector::Compound(vec![
            Selector::Checked(true),
            Selector::Enabled(true)
        ]))
        .is_some());
        assert!(bad(Selector::Compound(vec![
            Selector::Role("checkbox".into()),
            Selector::Checked(true)
        ]))
        .is_none());
    }

    #[test]
    fn empty_compound_selector_fails() {
        let suite = make_suite(
            vec![],
            vec![TestCase {
                name: "test1".to_string(),
                tags: vec![],
                isolated: false,
                depends_on: vec![],
                steps: vec![step(Action::Tap {
                    selector: Selector::Compound(vec![]),
                })],
                description: None,
                verify: None,
            }],
        );
        let err = validate_suite(&suite).unwrap_err();
        match err {
            VelocityError::InvalidSelector { reason, .. } => {
                assert!(reason.contains("compound"));
            }
            other => panic!("expected InvalidSelector, got {:?}", other),
        }
    }

    #[test]
    fn flow_steps_are_validated() {
        let suite = make_suite(
            vec![Flow {
                id: "bad_flow".to_string(),
                steps: vec![step(Action::Tap {
                    selector: Selector::Id("  ".to_string()),
                })],
            }],
            vec![TestCase {
                name: "test1".to_string(),
                tags: vec![],
                isolated: false,
                depends_on: vec![],
                steps: vec![step(Action::Wait { ms: 100 })],
                description: None,
                verify: None,
            }],
        );
        assert!(validate_suite(&suite).is_err());
    }

    #[test]
    fn non_selector_actions_pass() {
        let suite = make_suite(
            vec![],
            vec![TestCase {
                name: "test1".to_string(),
                tags: vec![],
                isolated: false,
                depends_on: vec![],
                steps: vec![
                    step(Action::LaunchApp {
                        app_id: "com.test".to_string(),
                        clear_state: false,
                    }),
                    step(Action::Wait { ms: 500 }),
                    step(Action::Screenshot { filename: None }),
                    step(Action::PressKey {
                        key: velocity_common::Key::Back,
                    }),
                ],
                description: None,
                verify: None,
            }],
        );
        assert!(validate_suite(&suite).is_ok());
    }

    fn suite_with(deps: &[(&str, &[&str])]) -> TestSuite {
        TestSuite {
            app_id: "a".into(),
            config: Default::default(),
            flows: vec![],
            tests: deps
                .iter()
                .map(|(name, d)| TestCase {
                    name: (*name).into(),
                    tags: vec![],
                    isolated: false,
                    depends_on: d.iter().map(|s| (*s).to_string()).collect(),
                    steps: vec![],
                    description: None,
                    verify: None,
                })
                .collect(),
        }
    }

    #[test]
    fn dependencies_must_exist() {
        let err = validate_suite(&suite_with(&[("a", &["ghost"])])).unwrap_err();
        assert!(err.to_string().contains("ghost"));
    }

    #[test]
    fn dependency_cycles_are_rejected() {
        let err = validate_suite(&suite_with(&[("a", &["b"]), ("b", &["c"]), ("c", &["a"])]))
            .unwrap_err();
        assert!(err.to_string().contains("cycle"), "{err}");
        assert!(validate_suite(&suite_with(&[
            ("a", &[]),
            ("b", &["a"]),
            ("c", &["a", "b"])
        ]))
        .is_ok());
    }

    #[test]
    fn web_selectors_are_refused_on_devices() {
        let suite = |platform: &str| {
            let platform = if platform.is_empty() {
                String::new()
            } else {
                format!("config:\n  platform: {platform}\n")
            };
            crate::parser::parse_suite_from_str(&format!(
                "appId: a\n{platform}tests:\n  - name: t\n    steps:\n      - tap: {{ text: \"Pay\", below: {{ xpath: \"//x[2]\" }} }}\n"
            ))
            .unwrap()
        };
        let err = validate_suite(&suite("android")).unwrap_err().to_string();
        assert!(
            err.contains("xpath selectors work only on the web"),
            "{err}"
        );
        assert!(validate_suite(&suite("web")).is_ok());
        // No platform: decided at run time (`check_platform_selectors`).
        assert!(validate_suite(&suite("")).is_ok());
        assert!(check_platform_selectors(&suite(""), velocity_common::Platform::Ios).is_err());
    }

    /// VEL-195: these all validated.
    #[test]
    fn duplicates_and_nonsensical_scroll_and_swipe_are_refused() {
        let check = |body: &str| {
            let yaml = format!("appId: x\n{body}");
            let suite = crate::parse_suite_from_str(&yaml).unwrap();
            validate_suite(&suite).map_err(|e| e.to_string())
        };
        let err = check(
            "tests:\n  - name: same\n    steps: [goBack]\n  - name: same\n    steps: [goBack]\n",
        )
        .unwrap_err();
        assert!(err.contains("two tests are named 'same'"), "{err}");
        let err = check("flows:\n  - id: f\n    steps: [goBack]\n  - id: f\n    steps: [refresh]\ntests:\n  - name: t\n    steps: [{ runFlow: f }]\n").unwrap_err();
        assert!(err.contains("two flows have the id 'f'"), "{err}");
        let err = check("tests:\n  - name: t\n    steps:\n      - scrollUntilVisible: { selector: { text: a }, maxScrolls: 0 }\n").unwrap_err();
        assert!(err.contains("maxScrolls"), "{err}");
        let err =
            check("tests:\n  - name: t\n    steps:\n      - swipe: { from: { x: 1, y: 2 } }\n")
                .unwrap_err();
        assert!(err.contains("both `from` and `to`"), "{err}");
        let err = check("tests:\n  - name: t\n    steps:\n      - swipe: { direction: up, within: { text: \"\" } }\n").unwrap_err();
        assert!(err.contains("empty"), "{err}");
        // Still fine.
        check("tests:\n  - name: t\n    steps:\n      - swipe: { from: { x: 1, y: 2 }, to: { x: 1, y: 9 } }\n      - scrollUntilVisible: { selector: { text: a }, maxScrolls: 1 }\n").unwrap();
    }

    #[test]
    fn empty_tests_and_directionless_swipes_are_warned_about() {
        let suite = crate::parse_suite_from_str(
            "appId: x\ntests:\n  - name: empty\n    steps: []\n  - name: s\n    steps:\n      - swipe: {}\n",
        )
        .unwrap();
        assert!(validate_suite(&suite).is_ok());
        let w = suite_warnings(&suite);
        assert!(
            w.iter().any(|w| w.contains("'empty' has no steps")),
            "{w:?}"
        );
        assert!(
            w.iter()
                .any(|w| w.contains("line 7: 's' step 0: swipe has no `direction`")),
            "{w:?}"
        );
        assert_eq!(w.len(), 2, "{w:?}");
        let none = crate::parse_suite_from_str("appId: x\ntests: []\n").unwrap();
        assert_eq!(suite_warnings(&none), ["the suite has no tests"]);
    }

    /// D-44/45: React Native has no headless renderer any more. Headless RN
    /// is a config error; RN on a device validates; a leftover
    /// `reactNative:` block parses and is warned about.
    #[test]
    fn react_native_headless_is_a_config_error() {
        let suite = |cfg: &str| {
            crate::parse_suite_from_str(&format!(
                "appId: x\nconfig:\n{cfg}tests:\n  - name: t\n    steps: [back]\n"
            ))
            .unwrap()
        };
        let headless =
            suite("  platform: android\n  driver: headless\n  framework: react_native\n");
        let err = validate_suite(&headless).unwrap_err().to_string();
        assert!(
            err.contains("React Native has no headless renderer"),
            "{err}"
        );
        assert!(err.contains("device or simulator"), "{err}");
        let device = suite("  platform: android\n  framework: react_native\n");
        validate_suite(&device).unwrap();
        assert!(suite_warnings(&device).is_empty());
        let leftover =
            suite("  framework: rn\n  reactNative:\n    bundlePath: a.bundle\n    port: 19500\n");
        validate_suite(&leftover).unwrap();
        let w = suite_warnings(&leftover);
        assert!(
            w.iter().any(|w| w.contains("`reactNative:` is ignored")),
            "{w:?}"
        );
    }

    /// VEL-219: steps the platform or driver can never run fail validation;
    /// ones it may not run are warnings.
    #[test]
    fn run_checks_capabilities_after_the_cli_overrides() {
        // Review-s9 M2: `driver: headless` with `tapXY`, run with
        // `--driver device`. The structural check alone passes; the
        // capability check follows the driver the run actually uses.
        let mut suite = crate::parse_suite_from_str(
            "appId: x\nconfig:\n  platform: android\n  driver: headless\ntests:\n  - name: t\n    steps:\n      - tapXY: { x: 1, y: 2 }\n",
        )
        .unwrap();
        let none = std::collections::HashMap::new();
        assert!(validate_suite_at(&suite, &none).is_err());
        validate_structure_at(&suite, &none).unwrap();
        suite.config.driver = Some(velocity_common::config::DriverMode::Device);
        check_capabilities(&suite).unwrap();
    }

    /// Review f5-f6 MINOR-7: the platform-selector check follows
    /// `--platform` too; the structural check leaves it to the run.
    #[test]
    fn run_checks_platform_selectors_after_the_platform_override() {
        let suite = crate::parse_suite_from_str(
            "appId: x\nconfig:\n  platform: android\ntests:\n  - name: t\n    steps:\n      - tap: { xpath: \"//x[2]\" }\n",
        )
        .unwrap();
        let none = std::collections::HashMap::new();
        assert!(validate_suite_at(&suite, &none).is_err());
        validate_structure_at(&suite, &none).unwrap();
        check_platform_selectors(&suite, velocity_common::Platform::Web).unwrap();
        assert!(check_platform_selectors(&suite, velocity_common::Platform::Android).is_err());
    }

    #[test]
    fn steps_the_platform_or_driver_cannot_run_are_caught_up_front() {
        let suite = |config: &str, step: &str| {
            crate::parse_suite_from_str(&format!(
                "appId: x\nconfig:\n{config}\ntests:\n  - name: t\n    steps:\n      - {step}\n"
            ))
            .unwrap()
        };
        let never = [
            (
                "  platform: android",
                "mockRequest: { url: \"*/api\", status: 500 }",
            ),
            (
                "  platform: ios",
                "mockRequest: { url: \"*/api\", status: 500 }",
            ),
            (
                "  platform: android\n  driver: headless",
                "tapXY: { x: 1, y: 2 }",
            ),
            (
                "  platform: android\n  driver: headless",
                "inputRaw: { text: a }",
            ),
            (
                "  platform: android\n  driver: headless",
                "openLink: app://x",
            ),
            ("  platform: android\n  driver: headless", "saveState: s"),
            (
                "  platform: android\n  driver: headless",
                "assertSystemState: { inCall: false }",
            ),
            ("  platform: web", "restoreState: s"),
        ];
        for (config, step) in never {
            let err = validate_suite(&suite(config, step));
            assert!(err.is_err(), "{config} / {step} validated");
        }
        let fine = [
            (
                "  platform: web",
                "mockRequest: { url: \"*/api\", status: 500 }",
            ),
            ("  platform: android", "tapXY: { x: 1, y: 2 }"),
            ("  platform: ios", "openLink: app://x"),
            ("  platform: android", "saveState: s"),
            (
                "  platform: android",
                "assertSystemState: { inCall: false }",
            ),
            (
                "  sync: { mode: auto }",
                "mockRequest: { url: \"*/api\", status: 500 }",
            ),
        ];
        for (config, step) in fine {
            let s = suite(config, step);
            validate_suite(&s).unwrap_or_else(|e| panic!("{config} / {step}: {e}"));
            assert!(
                suite_warnings(&s).is_empty(),
                "{config} / {step}: {:?}",
                suite_warnings(&s)
            );
        }
        for (config, step) in [
            ("  platform: android", "clearMocks"),
            (
                "  platform: android\n  framework: flutter",
                "tapXY: { x: 1, y: 2 }",
            ),
            ("  platform: ios\n  framework: rn", "saveState: s"),
        ] {
            let s = suite(config, step);
            validate_suite(&s).unwrap_or_else(|e| panic!("{config} / {step}: {e}"));
            assert_eq!(suite_warnings(&s).len(), 1, "{config} / {step}");
        }
    }
}
