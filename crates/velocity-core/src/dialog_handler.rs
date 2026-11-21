use std::collections::HashSet;

use tracing::{debug, info, warn};
use velocity_common::{DialogConfig, Element, PlatformDriver, Selector};

/// Known system dialog button labels that should be auto-dismissed.
/// Organized by platform convention and priority (prefer "Allow" over "Don't Allow").
const IOS_DISMISS_LABELS: &[&str] = &[
    // Location
    "Allow While Using App",
    "Allow Once",
    // Notifications
    "Allow",
    // "Open in <app>?" the first time a custom URL scheme is opened
    "Open",
    // Contacts, Photos, Camera, Microphone
    "OK",
    "Allow Full Access",
    // Generic
    "Continue",
    "Accept",
];

const IOS_ALERT_INDICATORS: &[&str] = &[
    // Common iOS system alert title patterns (element types)
    "Alert",
    "XCUIElementTypeAlert",
];

const ANDROID_DISMISS_LABELS: &[&str] = &[
    // Permissions
    "While using the app",
    "Only this time",
    "Allow",
    "ALLOW",
    // Generic
    "OK",
    "ACCEPT",
    "Continue",
    "CONTINUE",
    "GOT IT",
];

/// `platform_id` prefix the iOS driver gives the buttons of a SpringBoard
/// alert it merged into the app's tree (see velocity-ios `system_alert`).
const IOS_SYSTEM_ALERT_BUTTON: &str = "velocity-system-alert-button:";

/// Wording of iOS system prompts (permissions, "Open in …?"), for an alert
/// that reached the tree some other way: "“Shop” Would Like to Access…",
/// "Allow “Shop” to use your location?", "Open in “Shop”?". "Would like
/// to" counts only right after the quoted app name: a bare one matched the
/// app's own "We would like to send you offers [No thanks][Continue]"
/// (review 09 #7).
const IOS_SYSTEM_ALERT_PHRASES: &[&str] = &["\u{201d} would like to", "\" would like to"];
const IOS_SYSTEM_ALERT_STARTS: &[&str] = &["allow ", "open in \"", "open in \u{201c}"];
/// What follows "Allow …" in a permission prompt. A bare "to access" also
/// matched the app's own "Sign in to access your account" (C-m2).
const IOS_ALLOW_PURPOSES: &[&str] = &[
    " to access",
    " to use your",
    " to track your activity",
    " to find and connect",
    " to send you",
];

// `android:id/alertTitle` is not here: the app's own AlertDialogs carry it
// too, and confirming those ("Delete account?" → OK) is the test's call. It
// counts only in a dialog of a system package (see `foreign_alert`).
const ANDROID_ALERT_INDICATORS: &[&str] = &[
    "PermissionDialogActivity",
    "GrantPermissionsActivity",
    "com.android.permissioncontroller",
    "grant_dialog",
    "grant_singleton",
];

/// The permission controller's grant buttons by resource id, most wanted
/// first (the order of [`ANDROID_DISMISS_LABELS`]): found whatever the
/// device's language, where the English labels find nothing on a de-DE
/// device ("Bei Nutzung der App") and the test failed on its next step
/// (review 09 #7).
const ANDROID_PERMISSION_BUTTON_IDS: &[&str] = &[
    ":id/permission_allow_foreground_only_button",
    ":id/permission_allow_one_time_button",
    ":id/permission_allow_button",
];

/// Handles automatic detection and dismissal of system dialogs
/// (permission prompts, alerts) that block test execution.
pub struct DialogHandler {
    config: DialogConfig,
    all_dismiss_labels: Vec<String>,
    /// The app under test (its own AlertDialogs are left to the test).
    app_id: String,
}

impl DialogHandler {
    pub fn new(config: DialogConfig) -> Self {
        let mut all_dismiss_labels = Vec::new();
        let mut seen = HashSet::new();

        for label in IOS_DISMISS_LABELS
            .iter()
            .chain(ANDROID_DISMISS_LABELS.iter())
        {
            if seen.insert(*label) {
                all_dismiss_labels.push((*label).to_string());
            }
        }

        for label in &config.custom_dismiss_labels {
            if seen.insert(label.as_str()) {
                all_dismiss_labels.push(label.clone());
            }
        }
        Self {
            config,
            all_dismiss_labels,
            app_id: String::new(),
        }
    }

    /// Tell the handler which app is under test: an Android AlertDialog
    /// whose ids name another package (Play services' location prompt) is
    /// a system dialog; one of this app's is not.
    pub fn for_app(mut self, app_id: &str) -> Self {
        self.app_id = app_id.to_string();
        self
    }

    /// Check the current hierarchy for system dialogs and dismiss them.
    /// Returns the number of dialogs dismissed.
    pub async fn dismiss_if_present(&self, driver: &dyn PlatformDriver, device_id: &str) -> u32 {
        self.dismiss_unless_target(driver, device_id, None).await
    }

    /// Like [`Self::dismiss_if_present`], but leave a dialog alone when
    /// `target` (the step's own selector) matches something in it: the step
    /// is about to answer that dialog itself.
    pub async fn dismiss_unless_target(
        &self,
        driver: &dyn PlatformDriver,
        device_id: &str,
        target: Option<&Selector>,
    ) -> u32 {
        if !self.config.enabled {
            return 0;
        }

        let mut dismissed = 0;

        for _ in 0..self.config.max_dismissals {
            let hierarchy = match driver.get_hierarchy_cached(device_id).await {
                Ok(h) => h,
                Err(_) => break,
            };

            // A crash / ANR dialog ("<app> keeps stopping", "<app> isn't
            // responding"). Close it unless it belongs to the app under test
            // or the step itself answers it.
            if let Some(close) = self.find_crash_close(&hierarchy) {
                // The step's own selector answers this dialog: leave it alone.
                if target.is_some_and(|t| {
                    self.find_crash_dialog(&hierarchy).is_some_and(|d| {
                        velocity_common::selector_match::find_ref(d, t, None).is_some()
                    })
                }) {
                    debug!("crash dialog is the step's own target; not dismissing it");
                    break;
                }
                let title = self.find_crash_title_text(&hierarchy).unwrap_or_default();
                let ours = self.app_id.is_empty()
                    || title.contains(self.app_id.as_str())
                    || driver.crash_report(device_id, &self.app_id).await.is_some();
                if ours {
                    break;
                }
                info!(title = %title, "closed another app's crash dialog");
                match driver.tap(device_id, close).await {
                    Ok(()) => {
                        dismissed += 1;
                        // Brief pause to let the dialog animate away
                        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    }
                    Err(e) => {
                        warn!(error = %e, "failed to dismiss another app's crash dialog");
                        break;
                    }
                }
                continue;
            }

            // Check if there's a system dialog present
            let Some(dialog) = self.find_dialog_element(&hierarchy) else {
                break;
            };
            if target.is_some_and(|t| {
                velocity_common::selector_match::find_ref(dialog, t, None).is_some()
            }) {
                debug!("system dialog is the step's own target; not dismissing it");
                break;
            }

            // Find a dismiss button and tap it
            if let Some(button) = self.find_dismiss_button(&hierarchy) {
                info!(
                    label = button.label.as_deref().unwrap_or(""),
                    text = button.text.as_deref().unwrap_or(""),
                    "auto-dismissing system dialog"
                );
                match driver.tap(device_id, &button).await {
                    Ok(()) => {
                        dismissed += 1;
                        // Brief pause to let the dialog animate away
                        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    }
                    Err(e) => {
                        warn!(error = %e, "failed to dismiss system dialog");
                        break;
                    }
                }
            } else {
                debug!("system dialog detected but no dismiss button found");
                break;
            }
        }

        if dismissed > 0 {
            info!(count = dismissed, "dismissed system dialog(s)");
        }

        dismissed
    }

    /// Check if the hierarchy contains a system dialog/alert.
    #[cfg(test)]
    fn has_system_dialog(&self, root: &Element) -> bool {
        self.find_dialog_element(root).is_some()
    }

    /// Recursively search for a system dialog element.
    fn find_dialog_element<'a>(&self, root: &'a Element) -> Option<&'a Element> {
        self.find_dialog_in(root, root)
    }

    fn find_dialog_in<'a>(&self, element: &'a Element, root: &Element) -> Option<&'a Element> {
        // Check element type against known alert indicators
        // iOS: only an element whose *type* is an alert. Substring-matching
        // "Alert" against ids treated app content like `SuccessAlertBanner` as
        // a system dialog and tapped its OK/Allow button.
        // And only a system one: the app's own UIAlertController is an
        // alert too, and "Continue" on "Delete account?" was confirmed
        // before the step could tap Cancel.
        let ios_alert = IOS_ALERT_INDICATORS
            .iter()
            .any(|indicator| element.element_type == *indicator)
            && is_ios_system_alert(element, root);
        // Android: system dialog ids/packages are specific enough to match
        // as substrings of the resource-id or class.
        let android_alert = ANDROID_ALERT_INDICATORS.iter().any(|indicator| {
            element.platform_id.contains(indicator) || element.element_type.contains(indicator)
        }) || self.foreign_alert(element);
        let is_alert = ios_alert || android_alert;

        if is_alert {
            return Some(element);
        }

        // Recurse into children
        for child in &element.children {
            if let Some(found) = self.find_dialog_in(child, root) {
                return Some(found);
            }
        }

        None
    }

    /// A crash / ANR dialog's `android:id/aerr_close` button (an "Close app"/
    /// "Wait"-style dialog of another package). Returns the button to tap.
    fn find_crash_close<'a>(&self, root: &'a Element) -> Option<&'a Element> {
        Self::crash_close_in(root)
    }

    fn crash_close_in(el: &Element) -> Option<&Element> {
        if el.platform_id == "android:id/aerr_close" {
            return Some(el);
        }
        el.children.iter().find_map(|c| Self::crash_close_in(c))
    }

    /// The innermost dialog node holding both `android:id/alertTitle` and a
    /// crash button (`aerr_close`/`aerr_wait`).
    fn find_crash_dialog<'a>(&self, root: &'a Element) -> Option<&'a Element> {
        fn any(el: &Element, f: &dyn Fn(&Element) -> bool) -> bool {
            f(el) || el.children.iter().any(|c| any(c, f))
        }
        let title = |e: &Element| e.platform_id == "android:id/alertTitle";
        let crash_button = |e: &Element| {
            e.platform_id == "android:id/aerr_close" || e.platform_id == "android:id/aerr_wait"
        };
        let both = |e: &Element| any(e, &title) && any(e, &crash_button);
        if !both(root) {
            return None;
        }
        // Innermost first: the screen around the dialog holds both too, and
        // matching the step's selector against the whole screen took any
        // step's target for the dialog's.
        root.children
            .iter()
            .find_map(|c| self.find_crash_dialog(c))
            .or(Some(root))
    }

    /// The `android:id/alertTitle` text of a crash dialog, for logging.
    fn find_crash_title_text(&self, root: &Element) -> Option<String> {
        if root.platform_id == "android:id/alertTitle" {
            return root.text.clone().or(root.label.clone());
        }
        root.children
            .iter()
            .find_map(|c| self.find_crash_title_text(c))
    }

    /// An Android framework AlertDialog (it holds `android:id/alertTitle`)
    /// shown by a system package: some id in it names a package that is
    /// neither the framework's nor the app's (C-m3: dropping alertTitle
    /// altogether stopped answering Play services' prompts).
    fn foreign_alert(&self, element: &Element) -> bool {
        fn any(el: &Element, f: &dyn Fn(&Element) -> bool) -> bool {
            f(el) || el.children.iter().any(|c| any(c, f))
        }
        // The dialog: the innermost node holding both its title and a button
        // (not the screen around it, which may hold another app's ids).
        let title = |e: &Element| e.platform_id == "android:id/alertTitle";
        let button = |e: &Element| e.platform_id.starts_with("android:id/button");
        let both = |e: &Element| any(e, &title) && any(e, &button);
        if !both(element) || element.children.iter().any(both) {
            return false;
        }
        any(element, &|e| match e.platform_id.split_once(":id/") {
            Some((pkg, _)) => is_system_package(pkg) && !self.is_app_package(pkg),
            None => false,
        })
    }

    /// Whether `pkg` (from a resource id) is the app's own. Resource ids
    /// carry the Gradle *namespace*, not the applicationId: a debug or
    /// flavoured build (`com.shop.debug`) shows ids `com.shop:id/…`, and its
    /// own dialogs were answered as another package's (M-4).
    fn is_app_package(&self, pkg: &str) -> bool {
        let app = self.app_id.as_str();
        !app.is_empty()
            && (pkg == app
                || app
                    .strip_prefix(pkg)
                    .is_some_and(|rest| rest.starts_with('.'))
                || pkg
                    .strip_prefix(app)
                    .is_some_and(|rest| rest.starts_with('.')))
    }

    /// Find a tappable dismiss button within a dialog subtree.
    fn find_dismiss_button(&self, root: &Element) -> Option<Element> {
        // First, find the dialog subtree
        let dialog = self.find_dialog_element(root)?;

        // Search within the dialog for a matching button
        let mut candidates = Vec::new();
        self.collect_dismiss_buttons(dialog, &mut candidates);

        // Sort by priority: first match in dismiss_labels list wins
        candidates.sort_by_key(|(priority, _)| *priority);
        candidates
            .into_iter()
            .next()
            .map(|(_, el)| el)
            .or_else(|| permission_button_by_id(dialog))
    }

    /// Collect all buttons within an element tree that match dismiss labels.
    fn collect_dismiss_buttons(&self, element: &Element, out: &mut Vec<(usize, Element)>) {
        // Android Material dialogs often render action buttons as TextViews.
        // A TextView is only accepted when its label exactly matches a known
        // dismiss label (checked below), so this cannot tap arbitrary text.
        let is_button = element.element_type.contains("Button")
            || element.element_type.contains("button")
            || element.element_type.contains("TextView");

        if is_button && element.visible && element.enabled {
            let label_text = element
                .label
                .as_deref()
                .or(element.text.as_deref())
                .unwrap_or("");

            for (priority, dismiss_label) in self.all_dismiss_labels.iter().enumerate() {
                if label_text.eq_ignore_ascii_case(dismiss_label) {
                    out.push((priority, element.clone()));
                    break;
                }
            }
        }

        for child in &element.children {
            self.collect_dismiss_buttons(child, out);
        }
    }
}

/// A visible, enabled permission-controller grant button, by id
/// ([`ANDROID_PERMISSION_BUTTON_IDS`]), in `dialog`.
fn permission_button_by_id(dialog: &Element) -> Option<Element> {
    fn find<'a>(el: &'a Element, suffix: &str) -> Option<&'a Element> {
        if el.visible
            && el.enabled
            && el.platform_id.ends_with(suffix)
            && el
                .platform_id
                .split_once(":id/")
                .is_some_and(|(pkg, _)| pkg.ends_with(".permissioncontroller"))
        {
            return Some(el);
        }
        el.children.iter().find_map(|c| find(c, suffix))
    }
    ANDROID_PERMISSION_BUTTON_IDS
        .iter()
        .find_map(|id| find(dialog, id))
        .cloned()
}

/// Whether a dialog of `pkg` is the system's to show: one of the known
/// system packages, exactly. Any other package is not enough: a library
/// module's namespace (`com.stripe:id/…`) or the app's own under another
/// namespace than its applicationId is the app's dialog, and the test's to
/// answer. A `com.android.*` / `com.google.android.*` prefix used to count
/// too, but an AOSP-derived or rebranded app keeps such a namespace under
/// another applicationId, and its own "Discard changes? [Cancel][OK]" was
/// answered OK before the step ran (review 09 #7).
fn is_system_package(pkg: &str) -> bool {
    SYSTEM_PACKAGES.contains(&pkg)
}

/// Packages whose dialogs are the system's, not the app's.
const SYSTEM_PACKAGES: &[&str] = &[
    "com.google.android.gms",
    "com.google.android.gsf",
    "com.android.vending",
    "com.android.systemui",
    "com.android.settings",
    "com.google.android.permissioncontroller",
    "com.android.permissioncontroller",
    "com.android.packageinstaller",
    "com.google.android.packageinstaller",
];

/// A SpringBoard alert the driver merged in (and not a copy of an in-app
/// alert also in the app's tree), or one worded like a system prompt.
fn is_ios_system_alert(alert: &Element, root: &Element) -> bool {
    fn any(el: &Element, f: &dyn Fn(&Element) -> bool) -> bool {
        f(el) || el.children.iter().any(|c| any(c, f))
    }
    let merged = alert
        .children
        .iter()
        .any(|c| c.platform_id.starts_with(IOS_SYSTEM_ALERT_BUTTON));
    if merged {
        let in_app_copy = any(root, &|e| {
            !std::ptr::eq(e, alert)
                && IOS_ALERT_INDICATORS.contains(&e.element_type.as_str())
                && e.label.is_some()
                && e.label == alert.label
        });
        return !in_app_copy;
    }
    any(alert, &|e| {
        [e.label.as_deref(), e.text.as_deref()]
            .into_iter()
            .flatten()
            .any(|t| {
                let t = t.to_lowercase();
                IOS_SYSTEM_ALERT_PHRASES.iter().any(|p| t.contains(p))
                    || (IOS_SYSTEM_ALERT_STARTS.iter().any(|p| t.starts_with(p))
                        && (!t.starts_with("allow ")
                            || IOS_ALLOW_PURPOSES.iter().any(|p| t.contains(p))))
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use velocity_common::{DeviceInfo, Direction, Element, Key, Rect, Result, VelocityError};

    fn make_button(label: &str, element_type: &str) -> Element {
        Element {
            hint: None,
            platform_id: String::new(),
            label: Some(label.to_string()),
            text: Some(label.to_string()),
            element_type: element_type.to_string(),
            bounds: Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 44,
            },
            enabled: true,
            visible: true,
            children: vec![],
            ..Default::default()
        }
    }

    fn make_ios_alert(buttons: Vec<Element>) -> Element {
        Element {
            hint: None,
            platform_id: "alert".to_string(),
            label: Some("Allow App to access your location?".to_string()),
            text: None,
            element_type: "XCUIElementTypeAlert".to_string(),
            bounds: Rect {
                x: 50,
                y: 300,
                width: 300,
                height: 200,
            },
            enabled: true,
            visible: true,
            children: buttons,
            ..Default::default()
        }
    }

    #[test]
    fn detects_ios_alert() {
        let handler = DialogHandler::new(DialogConfig::default());
        let alert = make_ios_alert(vec![
            make_button("Don't Allow", "XCUIElementTypeButton"),
            make_button("Allow While Using App", "XCUIElementTypeButton"),
        ]);
        let root = Element {
            hint: None,
            platform_id: "root".to_string(),
            label: None,
            text: None,
            element_type: "Application".to_string(),
            bounds: Rect {
                x: 0,
                y: 0,
                width: 390,
                height: 844,
            },
            enabled: true,
            visible: true,
            children: vec![alert],
            ..Default::default()
        };

        assert!(handler.has_system_dialog(&root));
    }

    #[test]
    fn finds_allow_button_over_deny() {
        let handler = DialogHandler::new(DialogConfig::default());
        let alert = make_ios_alert(vec![
            make_button("Don't Allow", "XCUIElementTypeButton"),
            make_button("Allow While Using App", "XCUIElementTypeButton"),
        ]);
        let root = Element {
            hint: None,
            platform_id: "root".to_string(),
            label: None,
            text: None,
            element_type: "Application".to_string(),
            bounds: Rect {
                x: 0,
                y: 0,
                width: 390,
                height: 844,
            },
            enabled: true,
            visible: true,
            children: vec![alert],
            ..Default::default()
        };

        let button = handler.find_dismiss_button(&root);
        assert!(button.is_some());
        assert_eq!(
            button.unwrap().label.as_deref(),
            Some("Allow While Using App")
        );
    }

    #[test]
    fn no_dialog_returns_false() {
        let handler = DialogHandler::new(DialogConfig::default());
        let root = Element {
            hint: None,
            platform_id: "root".to_string(),
            label: None,
            text: None,
            element_type: "Application".to_string(),
            bounds: Rect {
                x: 0,
                y: 0,
                width: 390,
                height: 844,
            },
            enabled: true,
            visible: true,
            children: vec![make_button("Login", "XCUIElementTypeButton")],
            ..Default::default()
        };

        assert!(!handler.has_system_dialog(&root));
    }

    #[test]
    fn custom_dismiss_labels_work() {
        let config = DialogConfig {
            custom_dismiss_labels: vec!["Dismiss".to_string()],
            ..Default::default()
        };
        let handler = DialogHandler::new(config);
        let alert = make_ios_alert(vec![make_button("Dismiss", "XCUIElementTypeButton")]);
        let root = Element {
            hint: None,
            platform_id: "root".to_string(),
            label: None,
            text: None,
            element_type: "Application".to_string(),
            bounds: Rect {
                x: 0,
                y: 0,
                width: 390,
                height: 844,
            },
            enabled: true,
            visible: true,
            children: vec![alert],
            ..Default::default()
        };

        let button = handler.find_dismiss_button(&root);
        assert!(button.is_some());
        assert_eq!(button.unwrap().label.as_deref(), Some("Dismiss"));
    }

    #[test]
    fn app_elements_named_like_alerts_are_not_system_dialogs() {
        let handler = DialogHandler::new(DialogConfig::default());
        let banner = Element {
            hint: None,
            platform_id: "SuccessAlertBanner".to_string(),
            label: Some("Order placed".to_string()),
            text: None,
            element_type: "Other".to_string(),
            bounds: Rect {
                x: 0,
                y: 0,
                width: 390,
                height: 80,
            },
            enabled: true,
            visible: true,
            children: vec![make_button("OK", "XCUIElementTypeButton")],
            ..Default::default()
        };
        assert!(handler.find_dialog_element(&banner).is_none());
    }

    fn in_app_alert() -> Element {
        let mut alert = make_ios_alert(vec![
            make_button("Cancel", "XCUIElementTypeButton"),
            make_button("Continue", "XCUIElementTypeButton"),
        ]);
        alert.label = Some("Delete account?".into());
        alert
    }

    #[test]
    fn the_apps_own_alert_is_not_a_system_dialog() {
        // VEL-011: an in-app UIAlertController's "Continue" was confirmed.
        let handler = DialogHandler::new(DialogConfig::default());
        assert!(handler.find_dismiss_button(&in_app_alert()).is_none());
        // Worded like a system prompt, or merged in from SpringBoard: it is.
        let mut system = in_app_alert();
        system.label = Some("\u{201c}Shop\u{201d} Would Like to Send You Notifications".into());
        assert!(handler.find_dismiss_button(&system).is_some());
        let mut merged = in_app_alert();
        merged.label = Some("Sign in with Apple ID".into());
        for b in &mut merged.children {
            b.platform_id = format!("velocity-system-alert-button:{}", b.label.clone().unwrap());
        }
        assert!(handler.find_dismiss_button(&merged).is_some());
        // An Android AlertDialog of the app's own.
        let mut dialog = make_button("", "android.widget.FrameLayout");
        dialog.children = vec![
            Element {
                platform_id: "android:id/alertTitle".into(),
                ..make_button("Delete account?", "android.widget.TextView")
            },
            make_button("OK", "android.widget.Button"),
        ];
        assert!(handler.find_dismiss_button(&dialog).is_none());
    }

    /// Review 09 #7: "would like to" without the quoted app name before it
    /// is the app's own marketing prompt.
    #[test]
    fn in_app_would_like_to_is_not_a_system_prompt() {
        let handler = DialogHandler::new(DialogConfig::default());
        let mut alert = in_app_alert();
        alert.label = Some("We would like to send you offers".into());
        assert!(handler.find_dismiss_button(&alert).is_none());
        alert.label = Some("\"Shop\" Would Like to Send You Notifications".into());
        assert!(handler.find_dismiss_button(&alert).is_some());
    }

    /// Review 09 #7: a localised permission prompt is answered by the
    /// grant button's id when no English label is there.
    #[test]
    fn a_localised_permission_prompt_is_answered_by_id() {
        let handler = DialogHandler::new(DialogConfig::default()).for_app("com.shop");
        let pc = "com.android.permissioncontroller";
        let button = |id: &str, label: &str| Element {
            platform_id: format!("{pc}:id/{id}"),
            ..make_button(label, "android.widget.Button")
        };
        let mut dialog = make_button("", "android.widget.LinearLayout");
        dialog.platform_id = format!("{pc}:id/grant_dialog");
        dialog.children = vec![
            button("permission_allow_button", "Zulassen"),
            button(
                "permission_allow_foreground_only_button",
                "Bei Nutzung der App",
            ),
            button("permission_allow_one_time_button", "Nur dieses Mal"),
            button("permission_deny_button", "Nicht zulassen"),
        ];
        let tapped = handler.find_dismiss_button(&dialog).unwrap();
        assert_eq!(tapped.label.as_deref(), Some("Bei Nutzung der App"));
        // Only the permission controller's ids count.
        let mut app = dialog.clone();
        for b in &mut app.children {
            b.platform_id = b.platform_id.replace(pc, "com.shop");
        }
        assert!(permission_button_by_id(&app).is_none());
    }

    #[test]
    fn in_app_wording_with_to_access_is_not_a_system_prompt() {
        // C-m2: "Sign in to access your account" [Cancel][Continue] was
        // taken for a permission prompt and confirmed.
        let handler = DialogHandler::new(DialogConfig::default());
        let mut alert = in_app_alert();
        alert.label = Some("Sign in to access your account".into());
        assert!(handler.find_dismiss_button(&alert).is_none());
        alert.label = Some("Upgrade to use your rewards".into());
        assert!(handler.find_dismiss_button(&alert).is_none());
        // Apple's own wording still is.
        for label in [
            "Allow \u{201c}Shop\u{201d} to use your location?",
            "Allow \u{201c}Shop\u{201d} to track your activity across other companies\u{2019} apps and websites?",
            "\u{201c}Shop\u{201d} Would Like to Access Your Photos",
            "Open in \u{201c}Shop\u{201d}?",
        ] {
            alert.label = Some(label.into());
            assert!(handler.find_dismiss_button(&alert).is_some(), "{label}");
        }
    }

    #[test]
    fn a_system_package_s_alert_dialog_is_still_answered() {
        // C-m3: Play services' location-accuracy prompt is a framework
        // AlertDialog (alertTitle) shown by another package.
        let dialog = |pkg: &str| {
            let mut d = make_button("", "android.widget.FrameLayout");
            d.children = vec![
                Element {
                    platform_id: "android:id/alertTitle".into(),
                    ..make_button("Turn on location accuracy?", "android.widget.TextView")
                },
                Element {
                    platform_id: format!("{pkg}:id/message"),
                    ..make_button("For a better experience…", "android.widget.TextView")
                },
                Element {
                    platform_id: "android:id/button1".into(),
                    ..make_button("OK", "android.widget.Button")
                },
            ];
            d
        };
        let handler = DialogHandler::new(DialogConfig::default()).for_app("com.shop");
        assert!(handler
            .find_dismiss_button(&dialog("com.google.android.gms"))
            .is_some());
        assert!(handler
            .find_dismiss_button(&dialog("com.android.vending"))
            .is_some());
        // Another package that isn't the system's: a library's dialog in
        // the app (M-4).
        assert!(handler
            .find_dismiss_button(&dialog("com.stripe.android.paymentsheet"))
            .is_none());
        assert!(handler
            .find_dismiss_button(&dialog("com.other.app"))
            .is_none());
        // The app's own dialog is the test's to answer, also with another
        // package's window (the keyboard) elsewhere on screen.
        assert!(handler.find_dismiss_button(&dialog("com.shop")).is_none());
        let mut screen = make_button("", "android.widget.FrameLayout");
        screen.children = vec![
            dialog("com.shop"),
            Element {
                platform_id: "com.google.android.inputmethod.latin:id/key_pos_0_0".into(),
                ..make_button("q", "android.widget.FrameLayout")
            },
        ];
        assert!(handler.find_dismiss_button(&screen).is_none());
        // Without the app id, only known system packages count.
        let unknown = DialogHandler::new(DialogConfig::default());
        assert!(unknown.find_dismiss_button(&dialog("com.shop")).is_none());
        assert!(unknown
            .find_dismiss_button(&dialog("com.google.android.gms"))
            .is_some());

        // M-4: a debug / flavoured build. The applicationId has a suffix,
        // the resource ids carry the namespace; the dialog is the app's.
        for app in ["com.shop.debug", "com.shop.staging", "com.shop"] {
            let h = DialogHandler::new(DialogConfig::default()).for_app(app);
            for pkg in ["com.shop", "com.shop.feature.checkout"] {
                assert!(
                    h.find_dismiss_button(&dialog(pkg)).is_none(),
                    "{app}: {pkg}:id/… answered as a system dialog"
                );
            }
            assert!(h
                .find_dismiss_button(&dialog("com.google.android.gms"))
                .is_some());
        }
        // A system-looking namespace that is no known system package (an
        // AOSP-derived app under another applicationId) is the app's.
        for pkg in ["com.android.contacts", "com.google.android.apps.notes"] {
            assert!(handler.find_dismiss_button(&dialog(pkg)).is_none(), "{pkg}");
        }
        // An app whose own id is a system-looking package still owns it.
        let settings = DialogHandler::new(DialogConfig::default()).for_app("com.android.settings");
        assert!(settings
            .find_dismiss_button(&dialog("com.android.settings"))
            .is_none());
    }

    /// A builder for the crash dialog used in the tests.
    fn crash_dialog(title: &str, close_id: &str, close_label: &str) -> Element {
        Element {
            hint: None,
            platform_id: "android:id/aerr_dialog".to_string(),
            label: None,
            text: None,
            element_type: "android.widget.FrameLayout".to_string(),
            bounds: Rect {
                x: 0,
                y: 0,
                width: 390,
                height: 844,
            },
            enabled: true,
            visible: true,
            children: vec![
                Element {
                    hint: None,
                    platform_id: "android:id/alertTitle".to_string(),
                    label: Some(title.to_string()),
                    text: Some(title.to_string()),
                    element_type: "android.widget.TextView".to_string(),
                    bounds: Rect {
                        x: 0,
                        y: 0,
                        width: 390,
                        height: 44,
                    },
                    enabled: true,
                    visible: true,
                    children: vec![],
                    ..Default::default()
                },
                Element {
                    hint: None,
                    platform_id: close_id.to_string(),
                    label: Some(close_label.to_string()),
                    text: Some(close_label.to_string()),
                    element_type: "android.widget.Button".to_string(),
                    bounds: Rect {
                        x: 0,
                        y: 0,
                        width: 390,
                        height: 44,
                    },
                    enabled: true,
                    visible: true,
                    children: vec![],
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    /// A driver that serves one fixed hierarchy and records taps and crash_report.
    struct CrashHierarchyDriver {
        screens: std::sync::Mutex<Vec<Element>>,
        taps: std::sync::Mutex<Vec<String>>,
        crash: std::sync::Mutex<Option<String>>,
    }

    impl CrashHierarchyDriver {
        fn new(screens: Vec<Element>, crash: Option<String>) -> Self {
            Self {
                screens: std::sync::Mutex::new(screens),
                taps: std::sync::Mutex::new(Vec::new()),
                crash: std::sync::Mutex::new(crash),
            }
        }
    }

    #[async_trait::async_trait]
    impl PlatformDriver for CrashHierarchyDriver {
        async fn list_devices(&self) -> Result<Vec<DeviceInfo>> {
            Ok(vec![])
        }
        async fn boot_device(&self, _: &str) -> Result<()> {
            Ok(())
        }
        async fn shutdown_device(&self, _: &str) -> Result<()> {
            Ok(())
        }
        async fn install_app(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        async fn launch_app(&self, _: &str, _: &str, _: bool) -> Result<()> {
            Ok(())
        }
        async fn stop_app(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        async fn find_element(&self, _: &str, _: &Selector) -> Result<Element> {
            Err(VelocityError::Unsupported("not used".into()))
        }
        async fn find_elements(&self, _: &str, _: &Selector) -> Result<Vec<Element>> {
            Ok(vec![])
        }
        async fn get_hierarchy(&self, _: &str) -> Result<Element> {
            let mut screens = self.screens.lock().unwrap();
            Ok(screens.remove(0))
        }
        async fn tap(&self, _: &str, el: &Element) -> Result<()> {
            self.taps.lock().unwrap().push(el.platform_id.clone());
            Ok(())
        }
        async fn double_tap(&self, _: &str, _: &Element) -> Result<()> {
            Ok(())
        }
        async fn long_press(&self, _: &str, _: &Element, _: u64) -> Result<()> {
            Ok(())
        }
        async fn input_text(&self, _: &str, _: &Element, _: &str) -> Result<()> {
            Ok(())
        }
        async fn clear_text(&self, _: &str, _: &Element) -> Result<()> {
            Ok(())
        }
        async fn swipe(&self, _: &str, _: Direction) -> Result<()> {
            Ok(())
        }
        async fn swipe_coords(&self, _: &str, _: (i32, i32), _: (i32, i32)) -> Result<()> {
            Ok(())
        }
        async fn press_key(&self, _: &str, _: Key) -> Result<()> {
            Ok(())
        }
        async fn screenshot(&self, _: &str) -> Result<Vec<u8>> {
            Ok(vec![])
        }
        async fn screen_size(&self, _: &str) -> Result<(i32, i32)> {
            Ok((1080, 1920))
        }
        async fn get_element_text(&self, _: &str, _: &Element) -> Result<String> {
            Ok(String::new())
        }
        async fn is_element_visible(&self, _: &str, _: &Element) -> Result<bool> {
            Ok(true)
        }
        async fn crash_report(&self, _: &str, _: &str) -> Option<String> {
            self.crash.lock().unwrap().clone()
        }
    }

    #[tokio::test]
    async fn crash_dialog_of_another_app_is_closed() {
        let handler = DialogHandler::new(DialogConfig::default()).for_app("com.android.settings");
        let driver = CrashHierarchyDriver::new(
            vec![
                crash_dialog(
                    "com.google.androidenv.accessibilityforwarder keeps stopping",
                    "android:id/aerr_close",
                    "Close app",
                ),
                Element::default(),
            ],
            None,
        );
        let n = handler.dismiss_unless_target(&driver, "d", None).await;
        assert_eq!(n, 1);
        assert_eq!(driver.taps.lock().unwrap().len(), 1);
        assert_eq!(driver.taps.lock().unwrap()[0], "android:id/aerr_close");
    }

    /// The dialog over the app's screen: the step's target is on that screen
    /// (behind the dialog), not in the dialog, so the dialog is still closed.
    #[tokio::test]
    async fn a_step_target_behind_the_crash_dialog_does_not_keep_it_open() {
        let handler = DialogHandler::new(DialogConfig::default()).for_app("com.android.settings");
        let behind = Element {
            platform_id: "android:id/title".to_string(),
            text: Some("Network & internet".to_string()),
            element_type: "android.widget.TextView".to_string(),
            ..Default::default()
        };
        let screen = Element {
            children: vec![
                behind,
                crash_dialog(
                    "com.google.androidenv.accessibilityforwarder keeps stopping",
                    "android:id/aerr_close",
                    "Close app",
                ),
            ],
            ..Default::default()
        };
        let driver = CrashHierarchyDriver::new(vec![screen, Element::default()], None);
        let target = Selector::Text("Network & internet".into());
        let n = handler
            .dismiss_unless_target(&driver, "d", Some(&target))
            .await;
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn crash_dialog_naming_our_app_is_left_alone() {
        let handler = DialogHandler::new(DialogConfig::default()).for_app("com.example.app");
        let driver = CrashHierarchyDriver::new(
            vec![
                crash_dialog(
                    "com.example.app keeps stopping",
                    "android:id/aerr_close",
                    "Close app",
                ),
                Element::default(),
            ],
            None,
        );
        let n = handler.dismiss_unless_target(&driver, "d", None).await;
        assert_eq!(n, 0);
        assert!(driver.taps.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn labelled_crash_dialog_with_crash_report_is_left_alone() {
        let handler = DialogHandler::new(DialogConfig::default()).for_app("com.android.settings");
        let driver = CrashHierarchyDriver::new(
            vec![
                crash_dialog(
                    "Settings keeps stopping",
                    "android:id/aerr_close",
                    "Close app",
                ),
                Element::default(),
            ],
            Some("boom".into()),
        );
        let n = handler.dismiss_unless_target(&driver, "d", None).await;
        assert_eq!(n, 0);
        assert!(driver.taps.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn crash_dialog_targeted_by_the_step_is_left_alone() {
        let handler = DialogHandler::new(DialogConfig::default()).for_app("com.android.settings");
        let driver = CrashHierarchyDriver::new(
            vec![
                crash_dialog(
                    "com.google.androidenv.accessibilityforwarder keeps stopping",
                    "android:id/aerr_close",
                    "Close app",
                ),
                Element::default(),
            ],
            None,
        );
        let target = Selector::Id("android:id/aerr_close".to_string());
        let n = handler
            .dismiss_unless_target(&driver, "d", Some(&target))
            .await;
        assert_eq!(n, 0);
        assert!(driver.taps.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn crash_dialog_with_empty_app_id_is_left_alone() {
        let handler = DialogHandler::new(DialogConfig::default());
        let driver = CrashHierarchyDriver::new(
            vec![
                crash_dialog(
                    "com.google.androidenv.accessibilityforwarder keeps stopping",
                    "android:id/aerr_close",
                    "Close app",
                ),
                Element::default(),
            ],
            None,
        );
        let n = handler.dismiss_unless_target(&driver, "d", None).await;
        assert_eq!(n, 0);
        assert!(driver.taps.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn anr_dialog_of_another_app_is_closed() {
        let handler = DialogHandler::new(DialogConfig::default()).for_app("com.android.settings");
        let root = Element {
            hint: None,
            platform_id: "root".to_string(),
            label: None,
            text: None,
            element_type: "Application".to_string(),
            bounds: Rect {
                x: 0,
                y: 0,
                width: 390,
                height: 844,
            },
            enabled: true,
            visible: true,
            children: vec![crash_dialog(
                "Chrome isn't responding",
                "android:id/aerr_close",
                "Wait",
            )],
            ..Default::default()
        };
        let driver = CrashHierarchyDriver::new(vec![root, Element::default()], None);
        let n = handler.dismiss_unless_target(&driver, "d", None).await;
        assert_eq!(n, 1);
        assert_eq!(driver.taps.lock().unwrap()[0], "android:id/aerr_close");
    }
}
