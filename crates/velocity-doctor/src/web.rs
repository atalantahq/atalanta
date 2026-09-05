//! Web: a Chrome or Chromium for the CDP driver.

use std::path::PathBuf;

use crate::{Area, AreaResult, Check, Ctx, Fix, Platform};

const A: Area = Area::Web;

/// Where Chrome is looked for, in the order chromiumoxide looks: `CHROME`,
/// the usual names on PATH, then the usual install locations.
pub(crate) fn find_chrome(ctx: &Ctx) -> Option<PathBuf> {
    let env = ctx.env();
    if let Some(p) = env.var("CHROME") {
        if let Some(found) = env.which(&p) {
            return Some(found);
        }
    }
    for name in [
        "google-chrome-stable",
        "google-chrome",
        "chromium",
        "chromium-browser",
        "chrome",
    ] {
        if let Some(p) = env.which(name) {
            return Some(p);
        }
    }
    let home = env.home();
    let apps = [
        "Google Chrome.app/Contents/MacOS/Google Chrome",
        "Chromium.app/Contents/MacOS/Chromium",
        "Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
    ];
    if std::env::consts::OS == "macos" {
        for root in [PathBuf::from("/Applications"), home.join("Applications")] {
            for a in apps {
                let p = root.join(a);
                if crate::env::is_executable(&p) {
                    return Some(p);
                }
            }
        }
    }
    None
}

pub(crate) async fn checks(ctx: &Ctx) -> AreaResult {
    let Some(chrome) = find_chrome(ctx) else {
        return (
            vec![Check::new(
                "web.chrome",
                A,
                ctx.missing(Platform::Web),
                "Chrome / Chromium not found (needed for web tests)",
            )
            .with_fix(Fix::guidance(
                "install Google Chrome or Chromium, or set CHROME (or a suite's `web: chromePath:`)",
            ))],
            false,
        );
    };
    let version = ctx
        .env()
        .run(&chrome, &["--version"], ctx.opts.timeout)
        .await
        .stdout()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    (
        vec![
            Check::ok("web.chrome", A, version.unwrap_or_else(|| "found".into()))
                .with_detail(chrome.display().to_string()),
        ],
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::fake::Tools;
    use crate::{Options, Status};

    fn ctx(t: &Tools, required: bool) -> Ctx {
        Ctx {
            opts: Options {
                env: t.env(),
                ..Options::default()
            },
            required: if required {
                vec![Platform::Web]
            } else {
                vec![]
            },
            apps: vec![],
        }
    }

    #[tokio::test]
    async fn chrome_on_path_is_found_with_its_version() {
        let t = Tools::new();
        t.tool("chromium", "echo 'Chromium 140.0.1'");
        let (c, usable) = checks(&ctx(&t, true)).await;
        assert!(usable);
        assert_eq!(c[0].summary, "Chromium 140.0.1");
    }

    #[tokio::test]
    async fn the_chrome_variable_wins_and_missing_chrome_fails_only_when_wanted() {
        let mut t = Tools::new();
        let custom = t.path("custom/chrome-bin");
        crate::env::fake::write_script(&custom, "echo 'Google Chrome 141'");
        t.set("CHROME", &custom.to_string_lossy());
        let (c, _) = checks(&ctx(&t, false)).await;
        assert_eq!(
            c[0].detail.as_deref(),
            Some(custom.to_string_lossy().as_ref())
        );

        if std::env::consts::OS != "macos" {
            let t = Tools::new();
            let (c, _) = checks(&ctx(&t, true)).await;
            assert_eq!(c[0].status, Status::Fail);
            let (c, _) = checks(&ctx(&t, false)).await;
            assert_eq!(c[0].status, Status::Skip);
        }
    }
}
