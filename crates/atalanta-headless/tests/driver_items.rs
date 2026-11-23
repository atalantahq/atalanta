//! Headless driver regressions: shared ids, password fields, stopApp, a missing
//! launchApp, the per-process device id, and layouts with runtime-only content.

use atalanta_common::{AtalantaError, Element, Platform, PlatformDriver, Selector};
use atalanta_headless::android::inflate::AndroidInflater;
use atalanta_headless::android::resources::ResourceTable;
use atalanta_headless::{HeadlessConfig, HeadlessDriver};

const NS: &str = r#"xmlns:android="http://schemas.android.com/apk/res/android""#;
const DEV: &str = "headless-0";

async fn launched(xml: &str) -> (HeadlessDriver, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("layout.xml");
    std::fs::write(&path, xml).unwrap();
    let driver = HeadlessDriver::new(
        Platform::Android,
        HeadlessConfig {
            app_path: Some(path.display().to_string()),
            ..Default::default()
        },
    );
    driver.launch_app(DEV, "app", false).await.unwrap();
    (driver, dir)
}

fn all<'a>(e: &'a Element, out: &mut Vec<&'a Element>) {
    out.push(e);
    for c in &e.children {
        all(c, out);
    }
}

async fn texts(driver: &HeadlessDriver) -> Vec<Option<String>> {
    let tree = driver.get_hierarchy(DEV).await.unwrap();
    let mut els = Vec::new();
    all(&tree, &mut els);
    els.into_iter()
        .filter(|e| e.element_type == "EditText")
        .map(|e| e.text.clone())
        .collect()
}

/// VEL-339: two fields share an id (an included row twice). Typing into the
/// second one wrote into the first.
#[tokio::test]
async fn input_text_into_the_second_of_two_fields_sharing_an_id() {
    let (driver, _dir) = launched(&format!(
        r#"<LinearLayout {NS} android:layout_width="match_parent" android:layout_height="match_parent" android:orientation="vertical">
            <EditText android:id="@+id/field" android:layout_width="match_parent" android:layout_height="48dp"/>
            <EditText android:id="@+id/field" android:layout_width="match_parent" android:layout_height="48dp"/>
        </LinearLayout>"#
    ))
    .await;
    let second = driver
        .find_elements(DEV, &Selector::Id("field".into()))
        .await
        .unwrap()
        .remove(1);
    driver.input_text(DEV, &second, "second").await.unwrap();
    assert_eq!(texts(&driver).await, vec![None, Some("second".into())]);
}

/// VEL-102: an id-less field used to be `headless:id/node_N`, which an app view
/// with `android:id="@+id/node_1"` collided with.
#[tokio::test]
async fn input_text_into_an_id_less_field_next_to_an_app_node_1() {
    let (driver, _dir) = launched(&format!(
        r#"<LinearLayout {NS} android:layout_width="match_parent" android:layout_height="match_parent" android:orientation="vertical">
            <EditText android:layout_width="match_parent" android:layout_height="48dp"/>
            <EditText android:id="@+id/node_1" android:layout_width="match_parent" android:layout_height="48dp"/>
        </LinearLayout>"#
    ))
    .await;
    let tree = driver.get_hierarchy(DEV).await.unwrap();
    let anonymous = tree.children[0].clone();
    driver.input_text(DEV, &anonymous, "typed").await.unwrap();
    assert_eq!(texts(&driver).await, vec![Some("typed".into()), None]);

    let found = driver
        .find_element(DEV, &Selector::Id("node_1".into()))
        .await
        .unwrap();
    assert_eq!(found.platform_id, "headless:id/node_1");
}

/// VEL-254: a password field shows and reports bullets, as on a device.
#[tokio::test]
async fn password_fields_report_bullets() {
    let (driver, _dir) = launched(&format!(
        r#"<LinearLayout {NS} android:layout_width="match_parent" android:layout_height="match_parent" android:orientation="vertical">
            <EditText android:id="@+id/pin" android:inputType="numberPassword" android:layout_width="match_parent" android:layout_height="48dp"/>
            <EditText android:id="@+id/pw" android:inputType="textPassword" android:layout_width="match_parent" android:layout_height="48dp"/>
            <EditText android:id="@+id/shown" android:inputType="textVisiblePassword" android:layout_width="match_parent" android:layout_height="48dp"/>
        </LinearLayout>"#
    ))
    .await;
    for id in ["pin", "pw", "shown"] {
        let el = driver
            .find_element(DEV, &Selector::Id(id.into()))
            .await
            .unwrap();
        driver.input_text(DEV, &el, "1234").await.unwrap();
    }
    assert_eq!(
        texts(&driver).await,
        vec![
            Some("••••".into()),
            Some("••••".into()),
            Some("1234".into())
        ]
    );
}

/// D-16 (VEL-348): typing appends to what the field holds, as the device
/// drivers do; clearText empties it; a password field appends bullets.
#[tokio::test]
async fn input_text_appends_and_clear_text_empties() {
    let (driver, _dir) = launched(&format!(
        r#"<LinearLayout {NS} android:layout_width="match_parent" android:layout_height="match_parent" android:orientation="vertical">
            <EditText android:id="@+id/name" android:text="Ada" android:layout_width="match_parent" android:layout_height="48dp"/>
            <EditText android:id="@+id/pw" android:inputType="textPassword" android:layout_width="match_parent" android:layout_height="48dp"/>
        </LinearLayout>"#
    ))
    .await;
    let find = |id: &'static str| {
        let driver = &driver;
        async move {
            driver
                .find_element(DEV, &Selector::Id(id.into()))
                .await
                .unwrap()
        }
    };
    driver
        .input_text(DEV, &find("name").await, " Lovelace")
        .await
        .unwrap();
    driver
        .input_text(DEV, &find("pw").await, "ab")
        .await
        .unwrap();
    driver
        .input_text(DEV, &find("pw").await, "c")
        .await
        .unwrap();
    assert_eq!(
        texts(&driver).await,
        vec![Some("Ada Lovelace".into()), Some("•••".into())]
    );
    driver.clear_text(DEV, &find("name").await).await.unwrap();
    driver
        .input_text(DEV, &find("name").await, "Grace")
        .await
        .unwrap();
    assert_eq!(texts(&driver).await[0], Some("Grace".into()));
}

/// VEL-357: after stopApp nothing of the app is on screen.
#[tokio::test]
async fn stop_app_empties_the_screen_until_the_next_launch() {
    let (driver, _dir) = launched(&format!(
        r#"<LinearLayout {NS} android:layout_width="match_parent" android:layout_height="match_parent"><Button android:id="@+id/go" android:text="Go"/></LinearLayout>"#
    ))
    .await;
    let go = Selector::Id("go".into());
    assert!(driver.find_element(DEV, &go).await.is_ok());
    driver.stop_app(DEV, "app").await.unwrap();
    assert!(matches!(
        driver.find_element(DEV, &go).await,
        Err(AtalantaError::ElementNotFound { .. })
    ));
    assert!(driver.get_hierarchy(DEV).await.unwrap().children.is_empty());
    driver.launch_app(DEV, "app", false).await.unwrap();
    assert!(driver.find_element(DEV, &go).await.is_ok());
}

/// VEL-255: a test without launchApp is a config error (exit 2, not retried),
/// not a 30 s device failure.
#[tokio::test]
async fn lookups_before_launch_app_are_config_errors() {
    let driver = HeadlessDriver::new(Platform::Android, HeadlessConfig::default());
    let sel = Selector::Text("Go".into());
    // Unprepared, and prepared (a session with no layout).
    for prepare in [false, true] {
        if prepare {
            driver.prepare(DEV).await.unwrap();
        }
        let err = driver.find_element(DEV, &sel).await.unwrap_err();
        assert!(
            matches!(&err, AtalantaError::Config(m) if m.contains("launchApp")),
            "{err:?}"
        );
        assert_eq!(err.exit_code(), 2);
        assert!(matches!(
            driver.get_hierarchy(DEV).await,
            Err(AtalantaError::Config(_))
        ));
        assert!(matches!(
            driver.find_elements(DEV, &sel).await,
            Err(AtalantaError::Config(_))
        ));
    }
}

/// VEL-052: the device id is per process, so two headless runs don't share a lease.
#[tokio::test]
async fn the_device_id_is_per_process() {
    let driver = HeadlessDriver::new(Platform::Android, HeadlessConfig::default());
    let devices = driver.list_devices().await.unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].id, format!("headless-{}", std::process::id()));
    assert_eq!(
        devices[0].id,
        atalanta_headless::driver::headless_device_id()
    );
}

/// N7: a ComposeView or fragment container is filled in by code, so headless
/// warns that it renders empty instead of silently passing negative assertions.
#[test]
fn runtime_content_views_are_warned_about() {
    let inflater = AndroidInflater::new(ResourceTable::empty());
    inflater
        .inflate_xml(&format!(
            r#"<LinearLayout {NS} android:layout_width="match_parent" android:layout_height="match_parent">
                <androidx.compose.ui.platform.ComposeView android:id="@+id/compose" android:layout_width="match_parent" android:layout_height="100dp"/>
                <androidx.fragment.app.FragmentContainerView android:id="@+id/host" android:layout_width="match_parent" android:layout_height="100dp"/>
                <TextView android:text="plain"/>
            </LinearLayout>"#
        ))
        .unwrap();
    let warnings = inflater.take_warnings();
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(
        warnings[0].contains("ComposeView 'compose'"),
        "{warnings:?}"
    );
    assert!(
        warnings[1].contains("FragmentContainerView 'host'"),
        "{warnings:?}"
    );
    assert!(inflater.take_warnings().is_empty());
}

/// VEL-S21: a non-UTF-8 file name next to the storyboard no longer aborts the search.
#[cfg(unix)]
#[tokio::test]
async fn ios_layout_search_skips_non_utf8_names() {
    use std::os::unix::ffi::OsStrExt;
    let dir = tempfile::tempdir().unwrap();
    // read_dir order is unspecified: enough odd names that one comes first.
    for i in 0..32u8 {
        let name = [0xff, b'a' + (i % 26), b'0' + i / 26];
        let odd = std::ffi::OsStr::from_bytes(&name);
        if std::fs::write(dir.path().join(odd), b"x").is_err() {
            // APFS refuses non-UTF-8 names (EILSEQ); only Linux filesystems
            // hit this, so there the skip would make the test vacuous
            // (review 07 M6).
            #[cfg(target_os = "linux")]
            panic!("a Linux filesystem refused a non-UTF-8 name");
            #[cfg(not(target_os = "linux"))]
            {
                eprintln!("skipped: this filesystem rejects non-UTF-8 names");
                return;
            }
        }
    }
    std::fs::write(
        dir.path().join("Main.storyboard"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<document type="com.apple.InterfaceBuilder3.CocoaTouch.Storyboard.XIB" version="3.0" initialViewController="vc">
  <scenes><scene sceneID="s"><objects>
    <viewController id="vc"><view key="view" id="root"><rect key="frame" x="0" y="0" width="390" height="844"/>
      <subviews><label id="hello" text="Hello"><rect key="frame" x="10" y="10" width="100" height="20"/></label></subviews>
    </view></viewController>
  </objects></scene></scenes>
</document>"#,
    )
    .unwrap();
    let driver = HeadlessDriver::new(
        Platform::Ios,
        HeadlessConfig {
            app_path: Some(dir.path().display().to_string()),
            initial_layout: Some("Main".into()),
            ..Default::default()
        },
    );
    driver.launch_app(DEV, "app", false).await.unwrap();
}

/// D-16: a UITextField's placeholder is shown as its text, but typing
/// replaces it rather than appending to it.
#[tokio::test]
async fn typing_into_an_ios_field_replaces_its_placeholder() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Main.xib");
    std::fs::write(
        &path,
        r#"<document><objects><view id="root"><rect key="frame" x="0" y="0" width="390" height="844"/><subviews>
            <textField id="email" placeholder="Email"><rect key="frame" x="0" y="100" width="390" height="44"/></textField>
        </subviews></view></objects></document>"#,
    )
    .unwrap();
    let driver = HeadlessDriver::new(
        Platform::Ios,
        HeadlessConfig {
            app_path: Some(path.display().to_string()),
            width: 390,
            height: 844,
            ..Default::default()
        },
    );
    driver.launch_app(DEV, "app", false).await.unwrap();
    let field = driver
        .find_element(DEV, &Selector::Id("email".into()))
        .await
        .unwrap();
    driver.input_text(DEV, &field, "a@b").await.unwrap();
    let field = driver
        .find_element(DEV, &Selector::Id("email".into()))
        .await
        .unwrap();
    driver.input_text(DEV, &field, ".c").await.unwrap();
    let field = driver
        .find_element(DEV, &Selector::Id("email".into()))
        .await
        .unwrap();
    assert_eq!(field.text.as_deref(), Some("a@b.c"));
}
