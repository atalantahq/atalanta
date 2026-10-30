//! Headless scrolling: swipes and `scrollUntilVisible` move scroll containers, so
//! content below the fold (not visible since VEL-109) can be reached.

use velocity_common::{Direction, Element, Platform, PlatformDriver, Selector};
use velocity_core::executor::TestExecutor;
use velocity_headless::{HeadlessConfig, HeadlessDriver};

const NS: &str = r#"xmlns:android="http://schemas.android.com/apk/res/android""#;
const DEV: &str = "headless-0";

/// A driver showing `xml` on a 1080x1920 screen at density 3, launched.
async fn launched(xml: &str) -> (HeadlessDriver, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("layout.xml");
    std::fs::write(&path, xml).unwrap();
    let driver = HeadlessDriver::new(
        Platform::Android,
        HeadlessConfig {
            app_path: Some(path.display().to_string()),
            width: 1080,
            height: 1920,
            density: 3.0,
            ..Default::default()
        },
    );
    driver.launch_app(DEV, "app", false).await.unwrap();
    (driver, dir)
}

fn by_id<'a>(root: &'a Element, id: &str) -> &'a Element {
    fn walk<'a>(e: &'a Element, want: &str) -> Option<&'a Element> {
        if e.platform_id == want {
            return Some(e);
        }
        e.children.iter().find_map(|c| walk(c, want))
    }
    walk(root, &format!("headless:id/{id}")).unwrap_or_else(|| panic!("no {id}"))
}

async fn element(driver: &HeadlessDriver, id: &str) -> Element {
    by_id(&driver.get_hierarchy(DEV).await.unwrap(), id).clone()
}

/// A full-screen ScrollView: a 48dp header, `spacer_dp` of filler and a 48dp footer.
fn long_page(spacer_dp: u32) -> String {
    format!(
        r#"<ScrollView {NS} android:id="@+id/scroller" android:layout_width="match_parent" android:layout_height="match_parent">
            <LinearLayout android:id="@+id/content" android:layout_width="match_parent" android:layout_height="wrap_content" android:orientation="vertical">
                <TextView android:id="@+id/header" android:layout_width="match_parent" android:layout_height="48dp" android:text="Header"/>
                <View android:id="@+id/spacer" android:layout_width="match_parent" android:layout_height="{spacer_dp}dp"/>
                <Button android:id="@+id/footer" android:layout_width="match_parent" android:layout_height="48dp" android:text="Footer"/>
            </LinearLayout>
        </ScrollView>"#
    )
}

#[tokio::test]
async fn scroll_until_visible_reaches_a_footer_below_the_fold() {
    let (driver, _dir) = launched(&long_page(2000)).await;
    assert!(
        !element(&driver, "footer").await.visible,
        "starts below the fold"
    );

    let suite = velocity_core::parser::parse_suite_from_str(
        r#"
appId: app
config:
  platform: android
  driver: headless
tests:
  - name: footer
    steps:
      - assertNotVisible: { id: footer }
      - scrollUntilVisible:
          selector: { id: footer }
          direction: down
          maxScrolls: 10
      - assertVisible: { id: footer }
      - tap: { id: footer }
"#,
    )
    .unwrap();
    // Nothing written into the crate: artifacts go to a temp dir, no healer files.
    let out = tempfile::tempdir().unwrap();
    let mut config = suite.config.clone();
    config.artifacts.output_dir = out.path().display().to_string();
    config.healing.persist_healed = false;
    let mut ex = TestExecutor::new(&driver, config, &suite.app_id);
    let r = ex.execute_test(&suite.tests[0], DEV, "app").await.unwrap();
    assert_eq!(
        r.status,
        velocity_common::TestStatus::Passed,
        "{:?}",
        r.error_message
    );

    let footer = element(&driver, "footer").await;
    assert!(footer.visible);
    assert!(
        footer.bounds.y + footer.bounds.height <= 1920,
        "{:?}",
        footer.bounds
    );
    // The header scrolled off the top.
    assert!(!element(&driver, "header").await.visible);
}

#[tokio::test]
async fn swipe_up_shows_what_is_below_and_clamps_at_both_ends() {
    let (driver, _dir) = launched(&long_page(2000)).await;
    // Content: 48 + 2000 + 48 dp = 6288 px in a 1920 px ScrollView.
    let max = 6288 - 1920;

    // `swipe: up` is the finger moving up, 40% of the screen: the content moves up.
    driver.swipe(DEV, Direction::Up).await.unwrap();
    assert_eq!(element(&driver, "header").await.bounds.y, -768);

    for _ in 0..20 {
        driver.swipe(DEV, Direction::Up).await.unwrap();
    }
    let footer = element(&driver, "footer").await;
    assert_eq!(
        footer.bounds.y + footer.bounds.height,
        1920,
        "stops at the end"
    );
    assert_eq!(element(&driver, "header").await.bounds.y, -max);
    assert!(footer.visible);

    for _ in 0..20 {
        driver.swipe(DEV, Direction::Down).await.unwrap();
    }
    let header = element(&driver, "header").await;
    assert_eq!(header.bounds.y, 0, "stops at the top");
    assert!(header.visible);
    assert!(!element(&driver, "footer").await.visible);
}

#[tokio::test]
async fn horizontal_scroll_view_scrolls_sideways() {
    let xml = format!(
        r#"<LinearLayout {NS} android:layout_width="match_parent" android:layout_height="match_parent" android:orientation="vertical">
            <HorizontalScrollView android:id="@+id/carousel" android:layout_width="match_parent" android:layout_height="100dp">
                <LinearLayout android:layout_width="wrap_content" android:layout_height="match_parent" android:orientation="horizontal">
                    <View android:id="@+id/first" android:layout_width="300dp" android:layout_height="100dp"/>
                    <View android:id="@+id/second" android:layout_width="300dp" android:layout_height="100dp"/>
                    <View android:id="@+id/last" android:layout_width="300dp" android:layout_height="100dp"/>
                </LinearLayout>
            </HorizontalScrollView>
        </LinearLayout>"#
    );
    let (driver, _dir) = launched(&xml).await;
    assert_eq!(element(&driver, "last").await.bounds.x, 1800);
    assert!(!element(&driver, "last").await.visible);

    // Finger right to left across the carousel.
    driver
        .swipe_coords(DEV, (900, 150), (100, 150))
        .await
        .unwrap();
    assert_eq!(element(&driver, "first").await.bounds.x, -800);
    assert_eq!(element(&driver, "last").await.bounds.x, 1000);
    assert!(element(&driver, "last").await.visible);

    // A vertical drag over it moves nothing: it only scrolls sideways, and the page
    // fits the screen.
    driver
        .swipe_coords(DEV, (500, 250), (500, 50))
        .await
        .unwrap();
    let first = element(&driver, "first").await;
    assert_eq!((first.bounds.x, first.bounds.y), (-800, 0));

    // Clamped at the right end: 2700 px of content in 1080.
    driver
        .swipe_coords(DEV, (1000, 150), (-5000, 150))
        .await
        .unwrap();
    let last = element(&driver, "last").await;
    assert_eq!(last.bounds.x + last.bounds.width, 1080);
}

#[tokio::test]
async fn a_nested_scroll_view_under_the_finger_scrolls_instead_of_the_page() {
    let xml = format!(
        r#"<ScrollView {NS} android:id="@+id/outer" android:layout_width="match_parent" android:layout_height="match_parent">
            <LinearLayout android:layout_width="match_parent" android:layout_height="wrap_content" android:orientation="vertical">
                <TextView android:id="@+id/title" android:layout_width="match_parent" android:layout_height="100dp" android:text="Title"/>
                <NestedScrollView android:id="@+id/inner" android:layout_width="match_parent" android:layout_height="200dp">
                    <LinearLayout android:layout_width="match_parent" android:layout_height="wrap_content" android:orientation="vertical">
                        <View android:id="@+id/row_a" android:layout_width="match_parent" android:layout_height="300dp"/>
                        <View android:id="@+id/row_b" android:layout_width="match_parent" android:layout_height="300dp"/>
                    </LinearLayout>
                </NestedScrollView>
                <View android:id="@+id/tail" android:layout_width="match_parent" android:layout_height="800dp"/>
            </LinearLayout>
        </ScrollView>"#
    );
    let (driver, _dir) = launched(&xml).await;
    // The inner list is at 300..900 px, holding 1800 px of rows.
    assert_eq!(element(&driver, "row_b").await.bounds.y, 1200);

    driver
        .swipe_coords(DEV, (540, 800), (540, 600))
        .await
        .unwrap();
    assert_eq!(
        element(&driver, "title").await.bounds.y,
        0,
        "page stayed put"
    );
    assert_eq!(element(&driver, "row_a").await.bounds.y, 100);

    // Row A scrolled out of the list, under the title: still on screen, but clipped
    // by the list, so not visible.
    driver
        .swipe_coords(DEV, (540, 800), (540, 0))
        .await
        .unwrap();
    let row_a = element(&driver, "row_a").await;
    assert_eq!((row_a.bounds.y, row_a.bounds.height), (-700, 900));
    assert!(
        !row_a.visible,
        "clipped by the inner list: {:?}",
        row_a.bounds
    );
    assert!(element(&driver, "row_b").await.visible);
    assert!(element(&driver, "title").await.visible);

    // The list can take 200 px more; the other 300 move the page.
    driver
        .swipe_coords(DEV, (540, 800), (540, 300))
        .await
        .unwrap();
    assert_eq!(element(&driver, "title").await.bounds.y, -300);
    assert_eq!(element(&driver, "row_b").await.bounds.y, 1200 - 1200 - 300);

    // Outside the inner list the page scrolls.
    driver
        .swipe_coords(DEV, (540, 1500), (540, 1400))
        .await
        .unwrap();
    assert_eq!(element(&driver, "title").await.bounds.y, -400);
}

#[tokio::test]
async fn a_launch_scrolls_back_to_the_top_and_screenshots_show_the_scroll() {
    let xml = format!(
        r##"<ScrollView {NS} android:layout_width="match_parent" android:layout_height="match_parent">
            <LinearLayout android:layout_width="match_parent" android:layout_height="wrap_content" android:orientation="vertical">
                <View android:id="@+id/red" android:layout_width="match_parent" android:layout_height="700dp" android:background="#FF0000"/>
                <View android:id="@+id/blue" android:layout_width="match_parent" android:layout_height="700dp" android:background="#0000FF"/>
            </LinearLayout>
        </ScrollView>"##
    );
    let (driver, _dir) = launched(&xml).await;
    let pixel = |png: Vec<u8>, x: usize, y: usize| {
        let decoder = png::Decoder::new(std::io::Cursor::new(png));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        let i = (y * info.width as usize + x) * 4;
        (buf[i], buf[i + 1], buf[i + 2])
    };
    assert_eq!(
        pixel(driver.screenshot(DEV).await.unwrap(), 540, 100),
        (255, 0, 0)
    );

    driver
        .swipe_coords(DEV, (540, 1800), (540, 200))
        .await
        .unwrap();
    assert_eq!(element(&driver, "blue").await.bounds.y, 2100 - 1600);
    assert_eq!(
        pixel(driver.screenshot(DEV).await.unwrap(), 540, 600),
        (0, 0, 255)
    );

    driver.launch_app(DEV, "app", true).await.unwrap();
    assert_eq!(element(&driver, "red").await.bounds.y, 0);
    assert_eq!(
        pixel(driver.screenshot(DEV).await.unwrap(), 540, 100),
        (255, 0, 0)
    );
}

#[tokio::test]
async fn a_long_layout_with_no_scroll_view_scrolls_the_window() {
    let xml = format!(
        r#"<LinearLayout {NS} android:layout_width="match_parent" android:layout_height="match_parent" android:orientation="vertical">
            <View android:id="@+id/spacer" android:layout_width="match_parent" android:layout_height="1000dp"/>
            <Button android:id="@+id/footer" android:layout_width="match_parent" android:layout_height="48dp" android:text="Footer"/>
        </LinearLayout>"#
    );
    let (driver, _dir) = launched(&xml).await;
    assert!(!element(&driver, "footer").await.visible);
    for _ in 0..5 {
        driver.swipe(DEV, Direction::Up).await.unwrap();
    }
    let footer = element(&driver, "footer").await;
    assert!(footer.visible);
    assert_eq!(footer.bounds.y + footer.bounds.height, 1920);
    let found = driver
        .find_element(DEV, &Selector::Id("footer".into()))
        .await
        .unwrap();
    assert!(found.visible);
}

#[tokio::test]
async fn an_ios_scroll_view_scrolls() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Main.xib");
    std::fs::write(
        &path,
        r#"<document><objects><view id="root"><rect key="frame" x="0" y="0" width="390" height="844"/><subviews>
            <scrollView id="list"><rect key="frame" x="0" y="0" width="390" height="844"/><subviews>
                <view id="top"><rect key="frame" x="0" y="0" width="390" height="800"/></view>
                <button id="bottom" title="Bottom"><rect key="frame" x="0" y="800" width="390" height="100"/></button>
            </subviews></scrollView>
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
    // Not squashed to fit: 900 pt of content in 844.
    let bottom = by_id(&driver.get_hierarchy(DEV).await.unwrap(), "bottom").clone();
    assert_eq!((bottom.bounds.y, bottom.bounds.height), (800, 100));
    assert!(bottom.visible, "partly on screen");
    driver.swipe(DEV, Direction::Up).await.unwrap();
    let tree = driver.get_hierarchy(DEV).await.unwrap();
    // Clamped: the content ends at the bottom of the screen.
    assert_eq!(by_id(&tree, "bottom").bounds.y, 744);
    assert_eq!(by_id(&tree, "top").bounds.y, -56);
}

// Review MAJOR-3 (b): a form in a ScrollView under a toolbar. Its lower field is laid
// out below the fold (no longer squashed into the viewport) and must be reachable
// with scrollUntilVisible, then typed into.
#[tokio::test]
async fn a_form_field_below_the_fold_is_reached_by_scrolling() {
    let fields: String = (0..12)
        .map(|i| {
            format!(
                r#"<EditText android:id="@+id/field{i}" android:layout_width="match_parent" android:layout_height="64dp" android:hint="Field {i}"/>
                   <View android:layout_width="match_parent" android:layout_height="120dp"/>"#
            )
        })
        .collect();
    let xml = format!(
        r#"<LinearLayout {NS} android:layout_width="match_parent" android:layout_height="match_parent" android:orientation="vertical">
            <View android:id="@+id/toolbar" android:layout_width="match_parent" android:layout_height="56dp"/>
            <ScrollView android:id="@+id/scroller" android:layout_width="match_parent" android:layout_height="match_parent">
                <LinearLayout android:layout_width="match_parent" android:layout_height="wrap_content" android:orientation="vertical">{fields}</LinearLayout>
            </ScrollView>
        </LinearLayout>"#
    );
    let (driver, _dir) = launched(&xml).await;
    assert!(
        !element(&driver, "field11").await.visible,
        "starts below the fold"
    );

    let suite = velocity_core::parser::parse_suite_from_str(
        r#"
appId: app
config:
  platform: android
  driver: headless
tests:
  - name: last field
    steps:
      - scrollUntilVisible:
          selector: { id: field11 }
          direction: down
          maxScrolls: 15
      - assertVisible: { id: field11 }
      - inputText:
          selector: { id: field11 }
          text: hello
"#,
    )
    .unwrap();
    let out = tempfile::tempdir().unwrap();
    let mut config = suite.config.clone();
    config.artifacts.output_dir = out.path().display().to_string();
    config.healing.persist_healed = false;
    let mut ex = TestExecutor::new(&driver, config, &suite.app_id);
    let r = ex.execute_test(&suite.tests[0], DEV, "app").await.unwrap();
    assert_eq!(
        r.status,
        velocity_common::TestStatus::Passed,
        "{:?}",
        r.error_message
    );
    let field = element(&driver, "field11").await;
    assert!(field.visible);
    assert!(
        field.bounds.y + field.bounds.height <= 1920,
        "{:?}",
        field.bounds
    );
}
