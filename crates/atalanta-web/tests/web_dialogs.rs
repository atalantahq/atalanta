//! Review 10 #9 and #11: JavaScript dialogs follow `web.dialogs` /
//! `web.promptText`, also in cross-origin frames, and downloads follow
//! `web.downloads`. Its own test binary: it changes the process-wide
//! settings (`atalanta_common::env::set_settings`, not `set_var`), so its
//! steps run in one test, one after another.

mod common;

use std::collections::HashMap;

use atalanta_common::PlatformDriver;
use common::{driver, eventually, find, serve, wait_text};

fn settings(pairs: &[(&str, &str)]) {
    atalanta_common::env::set_settings(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect::<HashMap<_, _>>(),
    );
}

const PAGE: &str = r##"<!doctype html><html><body>
<button id="c" onclick="document.getElementById('out').textContent = 'confirm:' + confirm('Delete?')">Delete</button>
<button id="p" onclick="document.getElementById('out').textContent = 'prompt:' + prompt('Name?')">Name</button>
<a id="dl" download="report.txt" href="/report.txt">Download</a>
<div id="out">idle</div>
</body></html>"##;

const INNER: &str = r##"<!doctype html><html><body>
<button id="fc" onclick="document.getElementById('fo').textContent = 'frame:' + confirm('Pay?')">Pay</button>
<button id="fa" onclick="alert('Hello'); document.getElementById('fo').textContent = 'after alert'">Alert</button>
<div id="fo">idle</div>
</body></html>"##;

#[tokio::test]
async fn dialogs_and_downloads_follow_their_settings() {
    require_chrome!();
    let dir = std::env::temp_dir().join(format!("atalanta-downloads-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let inner = serve(&[("/inner.html", INNER)]);
    let port = inner.rsplit(':').next().unwrap().to_string();
    let outer = format!(
        "{PAGE}<iframe src=\"http://localhost:{port}/inner.html\" width=400 height=200></iframe>"
    );
    let base = serve(&[("/d.html", outer.as_str()), ("/report.txt", "hello")]);

    // Default: confirm and prompt dismissed, downloads refused.
    settings(&[]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/d.html"), false)
        .await
        .unwrap();
    let c = find(&d, "#c").await;
    d.tap("web", &c).await.unwrap();
    assert_eq!(
        wait_text(&d, "#out", "confirm:false").await,
        "confirm:false"
    );
    // A dialog in a cross-origin frame is answered too: the frame went
    // on and its elements could still be read.
    let fa = d
        .find_element("web", &common::css("#fa"))
        .await
        .expect("the frame's button");
    d.tap("web", &fa).await.unwrap();
    assert_eq!(wait_text(&d, "#fo", "after alert").await, "after alert");
    let fc = d.find_element("web", &common::css("#fc")).await.unwrap();
    d.tap("web", &fc).await.unwrap();
    assert_eq!(wait_text(&d, "#fo", "frame:false").await, "frame:false");
    d.cleanup().await;

    // `web.dialogs: accept`, `web.promptText`, `web.downloads: <dir>`.
    settings(&[
        ("ATALANTA_WEB_DIALOGS", "accept"),
        ("ATALANTA_WEB_PROMPT_TEXT", "Ada"),
        ("ATALANTA_WEB_DOWNLOADS", dir.to_str().unwrap()),
    ]);
    let d = driver(vec![]).await;
    d.launch_app("web", &format!("{base}/d.html"), false)
        .await
        .unwrap();
    let c = find(&d, "#c").await;
    d.tap("web", &c).await.unwrap();
    assert_eq!(wait_text(&d, "#out", "confirm:true").await, "confirm:true");
    let p = find(&d, "#p").await;
    d.tap("web", &p).await.unwrap();
    assert_eq!(wait_text(&d, "#out", "prompt:Ada").await, "prompt:Ada");
    let fc = d.find_element("web", &common::css("#fc")).await.unwrap();
    d.tap("web", &fc).await.unwrap();
    assert_eq!(wait_text(&d, "#fo", "frame:true").await, "frame:true");
    let dl = find(&d, "#dl").await;
    d.tap("web", &dl).await.unwrap();
    let saved = dir.join("report.txt");
    assert!(
        eventually(|| async { std::fs::read_to_string(&saved).ok().as_deref() == Some("hello") })
            .await,
        "the download was not saved in {}",
        dir.display()
    );
    d.cleanup().await;
    settings(&[]);
    let _ = std::fs::remove_dir_all(&dir);
}
