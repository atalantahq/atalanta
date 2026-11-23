//! Golden render: one representative layout (colours, alpha, bold, text alpha,
//! wrapping) must render pixel-identically on every machine. The renderer is
//! CPU-only with bundled fonts, so any difference is a renderer change.
//!
//! After an intended rendering change, regenerate with
//! `ATALANTA_UPDATE_GOLDEN=1 cargo test -p atalanta-headless --test golden`.

use atalanta_common::{Platform, PlatformDriver};
use atalanta_headless::{HeadlessConfig, HeadlessDriver};

const LAYOUT: &str = r##"<LinearLayout xmlns:android="http://schemas.android.com/apk/res/android"
    android:layout_width="match_parent" android:layout_height="match_parent"
    android:orientation="vertical" android:padding="8dp" android:background="#FFFFFF">
    <TextView android:id="@+id/title" android:layout_width="match_parent" android:layout_height="wrap_content"
        android:text="Sign in" android:textSize="22sp" android:textStyle="bold" android:textColor="#202020"/>
    <TextView android:id="@+id/subtitle" android:layout_width="match_parent" android:layout_height="wrap_content"
        android:text="Use the account you registered with. Long text wraps onto the next line."
        android:textSize="14sp" android:textColor="#80000000"/>
    <View android:layout_width="match_parent" android:layout_height="24dp" android:background="#FF0000" android:alpha="0.5"/>
    <LinearLayout android:layout_width="match_parent" android:layout_height="wrap_content" android:orientation="horizontal" android:alpha="0.6">
        <Button android:id="@+id/cancel" android:layout_width="0dp" android:layout_weight="1" android:layout_height="40dp"
            android:text="Cancel" android:background="#DDDDDD"/>
        <Button android:id="@+id/ok" android:layout_width="0dp" android:layout_weight="1" android:layout_height="40dp"
            android:text="OK" android:textStyle="bold" android:background="#2060C0" android:textColor="#FFFFFF"/>
    </LinearLayout>
</LinearLayout>"##;

#[tokio::test]
async fn representative_layout_matches_its_golden_png() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("layout.xml");
    std::fs::write(&path, LAYOUT).unwrap();
    let driver = HeadlessDriver::new(
        Platform::Android,
        HeadlessConfig {
            app_path: Some(path.display().to_string()),
            width: 360,
            height: 240,
            density: 1.5,
            ..Default::default()
        },
    );
    driver.launch_app("headless-0", "app", false).await.unwrap();
    let png = driver.screenshot("headless-0").await.unwrap();

    let golden = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/sign_in.png");
    if std::env::var_os("ATALANTA_UPDATE_GOLDEN").is_some() {
        std::fs::write(&golden, &png).unwrap();
    }
    let baseline = std::fs::read(&golden).expect("golden missing; set ATALANTA_UPDATE_GOLDEN=1");
    let result = atalanta_core::visual::compare_images_with(&baseline, &png, &[], 1.0, 1).unwrap();
    if result.diff_pixel_count != 0 {
        let actual = std::env::temp_dir().join("atalanta-headless-golden-actual.png");
        std::fs::write(&actual, &png).unwrap();
        panic!(
            "{} pixels differ from {} (actual written to {})",
            result.diff_pixel_count,
            golden.display(),
            actual.display()
        );
    }
}
