use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use velocity_common::{Element, PlatformDriver, Result, Selector, VelocityError};

fn flatten_elements<'a>(
    root: &'a Element,
    out: &mut Vec<Value>,
    nodes: &mut Vec<&'a Element>,
    filter: Option<&str>,
) {
    let matches_filter = filter.is_none_or(|f| {
        let f_lower = f.to_lowercase();
        root.label
            .as_deref()
            .is_some_and(|l| l.to_lowercase().contains(&f_lower))
            || root
                .text
                .as_deref()
                .is_some_and(|t| t.to_lowercase().contains(&f_lower))
            || root.element_type.to_lowercase().contains(&f_lower)
    });

    if matches_filter {
        let mut entry = serde_json::json!({
            "platform_id": root.platform_id,
            "label": root.label,
            "text": root.text,
            "element_type": root.element_type,
            "bounds": root.bounds,
            "enabled": root.enabled,
            "visible": root.visible
        });
        // S1/S2: what a `role:` selector and the state filters see.
        if let Some(role) = velocity_common::role::role_of(root) {
            entry["role"] = role.into();
        }
        if let Some(c) = root.checked {
            entry["checked"] = c.into();
        }
        if root.selected == Some(true) {
            entry["selected"] = true.into();
        }
        if root.focused == Some(true) {
            entry["focused"] = true.into();
        }
        out.push(entry);
        nodes.push(root);
    }

    for child in &root.children {
        flatten_elements(child, out, nodes, filter);
    }
}

/// `e3 Button "Sign in" label="Log in" id=login @(10,20 300x48) role=button`;
/// a disabled, checked or selected element says so.
fn compact_line(r: &str, e: &Element) -> String {
    let mut line = format!("{r} {}", e.element_type);
    if let Some(t) = e.text.as_deref().filter(|t| !t.is_empty()) {
        line.push_str(&format!(" {t:?}"));
    }
    if let Some(l) = e.label.as_deref().filter(|l| !l.is_empty()) {
        if e.text.as_deref() != Some(l) {
            line.push_str(&format!(" label={l:?}"));
        }
    }
    if let Some(id) = e.platform_id.rsplit_once(":id/").map(|(_, s)| s) {
        line.push_str(&format!(" id={id}"));
    }
    let b = &e.bounds;
    line.push_str(&format!(" @({},{} {}x{})", b.x, b.y, b.width, b.height));
    // S1: the role a `role:` selector matches, after the box so the
    // listing's familiar prefix stays as it was.
    if let Some(role) = velocity_common::role::role_of(e) {
        line.push_str(&format!(" role={role}"));
    }
    if !e.enabled {
        line.push_str(" disabled");
    }
    match e.checked {
        Some(true) => line.push_str(" checked"),
        Some(false) => line.push_str(" unchecked"),
        None => {}
    }
    if e.selected == Some(true) {
        line.push_str(" selected");
    }
    line
}

/// Each element that a selector built from its id or text can find gets a
/// `ref` the other tools accept as `{"ref": "eN"}` (see `crate::refs`).
pub async fn list_elements(
    driver: &Arc<dyn PlatformDriver>,
    device_id: &str,
    args: &Value,
    refs: &mut crate::refs::Refs,
) -> Result<Value> {
    let filter = args.get("filter").and_then(|v| v.as_str());
    let hierarchy = driver.get_hierarchy(device_id).await?;
    let screen = crate::refs::screen_rect(driver, device_id).await;
    let mut elements = Vec::new();
    let mut nodes = Vec::new();
    flatten_elements(&hierarchy, &mut elements, &mut nodes, filter);
    refs.start_listing();
    let mut lines = Vec::new();
    for (json, node) in elements.iter_mut().zip(nodes) {
        if let Some(r) = refs.mint(&hierarchy, node, screen.as_ref()) {
            lines.push(compact_line(&r, node));
            json["ref"] = Value::String(r);
        }
    }
    // `format: "text"`: one line per element an agent can act on, a fraction
    // of the JSON's tokens (P-7).
    if args.get("format").and_then(Value::as_str) == Some("text") {
        return Ok(serde_json::json!({
            "count": lines.len(),
            "elements": lines.join("\n")
        }));
    }
    Ok(serde_json::json!({
        "count": elements.len(),
        "elements": elements
    }))
}

pub async fn get_element(
    driver: &Arc<dyn PlatformDriver>,
    device_id: &str,
    args: &Value,
) -> Result<Value> {
    let selector: Selector = super::interaction::parse_selector(args)?;

    let element = super::interaction::find_live(driver, device_id, &selector).await?;
    serde_json::to_value(&element)
        .map_err(|e| VelocityError::Internal(anyhow::anyhow!("Serialization error: {e}")))
}

pub async fn assert_visible(
    driver: &Arc<dyn PlatformDriver>,
    device_id: &str,
    args: &Value,
) -> Result<Value> {
    let selector: Selector = super::interaction::parse_selector(args)?;

    // `timeout_ms` (default 0: check now) waits for it first, as
    // `velocity assert --timeout` asks; the check below then reports it.
    if u64_arg(args, "timeout_ms")?.unwrap_or(0) > 0 {
        match wait_for_element(driver, device_id, args).await {
            Ok(_) => {}
            Err(e) if e.kind() == velocity_common::ErrorKind::ElementNotFound => {}
            Err(e) => return Err(e),
        }
    }

    let element = super::interaction::find_live(driver, device_id, &selector).await?;
    let visible = driver.is_element_visible(device_id, &element).await?;

    if !visible {
        return Err(VelocityError::AssertionFailed {
            expected: "visible".to_string(),
            actual: "not visible".to_string(),
            selector: selector.to_string(),
            screenshot: None,
        });
    }

    Ok(serde_json::json!({
        "assertion": "visible",
        "selector": selector.to_string(),
        "visible": true,
        "element": {
            "label": element.label,
            "text": element.text,
            "bounds": element.bounds
        }
    }))
}

// --- get_screen_summary ---

const INTERACTIVE_TYPES: &[&str] = &[
    "Button",
    "TextField",
    "EditText",
    "Switch",
    "Toggle",
    "Link",
    "Input",
    "Slider",
    "Checkbox",
    "Radio",
    "SecureTextField",
    "SearchField",
    "TextInput",
];

const NAV_TYPES: &[&str] = &[
    "TabBar",
    "NavigationBar",
    "Toolbar",
    "BottomNavigation",
    "BottomBar",
    "TabView",
];

const HEADER_TYPES: &[&str] = &["NavigationBar", "Header", "NavBar"];

#[derive(Default)]
struct ScreenSummary {
    screen_title: Option<String>,
    screen_text: Vec<String>,
    interactive: Vec<Value>,
    navigation: Vec<String>,
    total: usize,
    visible: usize,
    interactive_count: usize,
    text_fields: usize,
    buttons: usize,
}

fn type_matches_any(element_type: &str, patterns: &[&str]) -> bool {
    let lower = element_type.to_lowercase();
    patterns.iter().any(|p| lower.contains(&p.to_lowercase()))
}

fn element_display_text(el: &Element) -> Option<String> {
    el.label
        .as_deref()
        .or(el.text.as_deref())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

fn summarize_tree(root: &Element, summary: &mut ScreenSummary) {
    summary.total += 1;

    if root.visible {
        summary.visible += 1;
    }

    // Extract screen title from first header-type element
    if summary.screen_title.is_none() && type_matches_any(&root.element_type, HEADER_TYPES) {
        summary.screen_title = element_display_text(root).or_else(|| {
            // Try first child with text
            root.children.iter().find_map(element_display_text)
        });
    }

    // Collect navigation items
    if type_matches_any(&root.element_type, NAV_TYPES) {
        for child in &root.children {
            if let Some(text) = element_display_text(child) {
                if !summary.navigation.contains(&text) {
                    summary.navigation.push(text);
                }
            }
        }
    }

    // Collect visible text
    if root.visible {
        if let Some(text) = &root.text {
            if !text.is_empty() && !summary.screen_text.contains(text) {
                summary.screen_text.push(text.clone());
            }
        }
        if let Some(label) = &root.label {
            if !label.is_empty() && !summary.screen_text.contains(label) {
                summary.screen_text.push(label.clone());
            }
        }
    }

    // Collect interactive elements
    if type_matches_any(&root.element_type, INTERACTIVE_TYPES) {
        summary.interactive_count += 1;
        let lower = root.element_type.to_lowercase();
        if lower.contains("button") {
            summary.buttons += 1;
        }
        if lower.contains("text") || lower.contains("input") || lower.contains("search") {
            summary.text_fields += 1;
        }
        summary.interactive.push(serde_json::json!({
            "type": root.element_type,
            "label": element_display_text(root).unwrap_or_default(),
            "enabled": root.enabled
        }));
    }

    for child in &root.children {
        summarize_tree(child, summary);
    }
}

pub async fn get_screen_summary(
    driver: &Arc<dyn PlatformDriver>,
    device_id: &str,
    _args: &Value,
) -> Result<Value> {
    let hierarchy = driver.get_hierarchy(device_id).await?;
    let mut summary = ScreenSummary::default();
    summarize_tree(&hierarchy, &mut summary);

    Ok(serde_json::json!({
        "screen_title": summary.screen_title,
        "screen_text": summary.screen_text,
        "interactive": summary.interactive,
        "navigation": summary.navigation,
        "counts": {
            "total": summary.total,
            "visible": summary.visible,
            "interactive": summary.interactive_count,
            "text_fields": summary.text_fields,
            "buttons": summary.buttons
        }
    }))
}

// --- wait_for_element ---

pub async fn wait_for_element(
    driver: &Arc<dyn PlatformDriver>,
    device_id: &str,
    args: &Value,
) -> Result<Value> {
    let selector: Selector = super::interaction::parse_selector(args)?;

    let timeout_ms = u64_arg(args, "timeout_ms")?.unwrap_or(10_000).min(30_000);

    // Bounded both ways: a huge interval slept past the timeout and wedged
    // the server (VEL-083).
    let poll_interval_ms = u64_arg(args, "poll_interval_ms")?
        .unwrap_or(500)
        .clamp(100, 5_000);

    let start = Instant::now();
    let timeout = Duration::from_millis(timeout_ms);
    let poll_interval = Duration::from_millis(poll_interval_ms);

    loop {
        match super::interaction::find_live(driver, device_id, &selector).await {
            Ok(element) => {
                let visible = driver.is_element_visible(device_id, &element).await?;
                if visible {
                    let waited_ms = start.elapsed().as_millis() as u64;
                    return Ok(serde_json::json!({
                        "found": true,
                        "waited_ms": waited_ms,
                        "element": {
                            "label": element.label,
                            "text": element.text,
                            "bounds": element.bounds,
                            "enabled": element.enabled,
                            "visible": true
                        }
                    }));
                }
            }
            Err(e) if e.kind() == velocity_common::ErrorKind::ElementNotFound => {
                // Expected — element not yet present, keep polling
            }
            Err(e) => return Err(e), // Real driver error — propagate
        }

        let elapsed = start.elapsed();
        if elapsed >= timeout {
            return Err(VelocityError::ElementNotFound {
                selector: selector.to_string(),
                timeout_ms,
                screenshot: None,
                hierarchy_snapshot: None,
            });
        }

        tokio::time::sleep(poll_interval.min(timeout - elapsed)).await;
    }
}

/// An optional non-negative integer argument. A value of the wrong type is
/// an error, not silently the default (VEL-245).
fn u64_arg(args: &Value, name: &str) -> Result<Option<u64>> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_u64().map(Some).ok_or_else(|| {
            VelocityError::Config(format!("'{name}' must be a non-negative integer, got {v}"))
        }),
    }
}

#[cfg(test)]
mod wait_tests {
    use super::*;

    /// A headless device showing an empty screen.
    async fn driver() -> Arc<dyn PlatformDriver> {
        let d: Arc<dyn PlatformDriver> = Arc::new(velocity_headless::HeadlessDriver::new(
            velocity_common::Platform::Android,
            Default::default(),
        ));
        d.launch_app("headless", "app", false).await.unwrap();
        d
    }

    // VEL-083: a huge poll interval can't sleep past the timeout.
    #[tokio::test]
    async fn huge_poll_interval_is_clamped_to_the_timeout() {
        let args = serde_json::json!({
            "selector": { "text": "never-there" },
            "timeout_ms": 100,
            "poll_interval_ms": 10_000_000_000u64
        });
        let start = Instant::now();
        let r = tokio::time::timeout(
            Duration::from_secs(8),
            wait_for_element(&driver().await, "headless", &args),
        )
        .await;
        assert!(matches!(r, Ok(Err(_))), "{r:?}");
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    /// A device whose screen changes after its first read (a page
    /// transition), behind a tree cache as the Android and iOS drivers keep
    /// one: a lookup is served from the cache until it is invalidated.
    #[derive(Default)]
    struct CachingDevice {
        reads: std::sync::Mutex<u32>,
        cached: std::sync::Mutex<Option<Element>>,
    }

    impl CachingDevice {
        fn read(&self) -> Element {
            let mut reads = self.reads.lock().unwrap();
            *reads += 1;
            let text = if *reads == 1 {
                "Network & internet"
            } else {
                "Airplane mode"
            };
            let bounds = velocity_common::Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 40,
            };
            Element {
                children: vec![Element {
                    text: Some(text.into()),
                    bounds,
                    visible: true,
                    enabled: true,
                    ..Default::default()
                }],
                bounds,
                visible: true,
                ..Default::default()
            }
        }
    }

    #[async_trait::async_trait]
    impl PlatformDriver for CachingDevice {
        async fn find_element(&self, _: &str, selector: &Selector) -> Result<Element> {
            let tree = self
                .cached
                .lock()
                .unwrap()
                .get_or_insert_with(|| self.read())
                .clone();
            tree.children
                .into_iter()
                .find(|e| matches!(selector, Selector::Text(t) if e.text.as_ref() == Some(t)))
                .ok_or_else(|| VelocityError::ElementNotFound {
                    selector: selector.to_string(),
                    timeout_ms: 0,
                    screenshot: None,
                    hierarchy_snapshot: None,
                })
        }
        async fn invalidate_hierarchy_cache(&self) {
            *self.cached.lock().unwrap() = None;
        }
        async fn is_element_visible(&self, _: &str, _: &Element) -> Result<bool> {
            Ok(true)
        }
        async fn list_devices(&self) -> Result<Vec<velocity_common::DeviceInfo>> {
            unreachable!()
        }
        async fn boot_device(&self, _: &str) -> Result<()> {
            unreachable!()
        }
        async fn shutdown_device(&self, _: &str) -> Result<()> {
            unreachable!()
        }
        async fn install_app(&self, _: &str, _: &str) -> Result<()> {
            unreachable!()
        }
        async fn launch_app(&self, _: &str, _: &str, _: bool) -> Result<()> {
            unreachable!()
        }
        async fn stop_app(&self, _: &str, _: &str) -> Result<()> {
            unreachable!()
        }
        async fn find_elements(&self, _: &str, _: &Selector) -> Result<Vec<Element>> {
            unreachable!()
        }
        async fn get_hierarchy(&self, _: &str) -> Result<Element> {
            unreachable!()
        }
        async fn tap(&self, _: &str, _: &Element) -> Result<()> {
            unreachable!()
        }
        async fn double_tap(&self, _: &str, _: &Element) -> Result<()> {
            unreachable!()
        }
        async fn long_press(&self, _: &str, _: &Element, _: u64) -> Result<()> {
            unreachable!()
        }
        async fn input_text(&self, _: &str, _: &Element, _: &str) -> Result<()> {
            unreachable!()
        }
        async fn clear_text(&self, _: &str, _: &Element) -> Result<()> {
            unreachable!()
        }
        async fn swipe(&self, _: &str, _: velocity_common::Direction) -> Result<()> {
            unreachable!()
        }
        async fn swipe_coords(&self, _: &str, _: (i32, i32), _: (i32, i32)) -> Result<()> {
            unreachable!()
        }
        async fn press_key(&self, _: &str, _: velocity_common::Key) -> Result<()> {
            unreachable!()
        }
        async fn screenshot(&self, _: &str) -> Result<Vec<u8>> {
            unreachable!()
        }
        async fn screen_size(&self, _: &str) -> Result<(i32, i32)> {
            unreachable!()
        }
        async fn get_element_text(&self, _: &str, _: &Element) -> Result<String> {
            unreachable!()
        }
    }

    // DV-1 (emulator-5554, 2026-10-25): after a tap, wait_for_element kept
    // matching against the tree read mid-transition for the driver's 2 s
    // cache life, and assert_visible / get_element / tap read it too.
    #[tokio::test]
    async fn lookups_read_the_live_screen_not_the_driver_cache() {
        let device = Arc::new(CachingDevice::default());
        // Prime the cache with the pre-transition screen.
        let _ = device.find_element("d", &Selector::Text("x".into())).await;
        let d: Arc<dyn PlatformDriver> = device;
        let args = serde_json::json!({
            "selector": { "text": "Airplane mode" },
            "timeout_ms": 1000,
            "poll_interval_ms": 100
        });
        let found = wait_for_element(&d, "d", &args).await.unwrap();
        assert_eq!(found["waited_ms"], 0, "{found}");
        assert_visible(&d, "d", &args).await.unwrap();
        get_element(&d, "d", &args).await.unwrap();
    }

    // SUNMI tablet, 2026-11-21: `velocity assert --timeout 3s` failed at
    // once ("after 0ms"); assert_visible ignored timeout_ms.
    #[tokio::test]
    async fn assert_visible_waits_for_its_timeout_and_checks_now_without_one() {
        let d = driver().await;
        let start = Instant::now();
        let none = serde_json::json!({ "selector": { "text": "never-there" } });
        assert!(assert_visible(&d, "headless", &none).await.is_err());
        assert!(start.elapsed() < Duration::from_millis(300));

        let start = Instant::now();
        let waits = serde_json::json!({
            "selector": { "text": "never-there" },
            "timeout_ms": 600,
            "poll_interval_ms": 100
        });
        let e = assert_visible(&d, "headless", &waits).await.unwrap_err();
        assert!(start.elapsed() >= Duration::from_millis(600), "{e}");
    }

    // VEL-245: a wrong-typed number is reported, not silently the default.
    #[tokio::test]
    async fn wrong_typed_timeout_is_an_error() {
        let args = serde_json::json!({
            "selector": { "text": "x" },
            "timeout_ms": "5000"
        });
        let e = wait_for_element(&driver().await, "headless", &args)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("timeout_ms"), "{e}");
    }
}
