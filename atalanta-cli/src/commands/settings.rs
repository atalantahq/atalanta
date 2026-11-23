//! `atalanta settings`: every named setting, its variable, default and values.

use atalanta_common::settings::SETTINGS;
use clap::Args;
use colored::Colorize;

#[derive(Args)]
pub struct SettingsArgs {
    /// Only settings whose name contains this (e.g. `android`, `ios`)
    filter: Option<String>,
}

pub fn execute(args: SettingsArgs) -> anyhow::Result<i32> {
    let filter = args.filter.unwrap_or_default().to_lowercase();
    println!(
        "{} Set in a suite (`config: settings: {{ android: {{ scroll: semantic }} }}`) or per run \
         (`atalanta run --set android.scroll=semantic`). `--set` also takes any suite config key \
         (`--set sync.reactionMs=150`).\n",
        "=>".cyan().bold()
    );
    for s in SETTINGS
        .iter()
        .filter(|s| s.key.to_lowercase().contains(&filter))
    {
        // A secret's value (`ios.runnerToken`) is never printed: only that
        // it is set. The page can end up in a CI log or a bug report.
        // (`secrets` holds variable names, which are not secret.)
        let current = atalanta_common::env::var(s.env).ok().map(|v| {
            if s.key != "secrets" && atalanta_common::redact::is_secret_name(s.env) {
                "set".to_string()
            } else {
                v
            }
        });
        println!(
            "  {:<24} {:<34} default {}{}",
            s.key.bold(),
            s.values.dimmed(),
            if s.default.is_empty() { "-" } else { s.default },
            current
                .map(|v| format!("  (now {v})").yellow().to_string())
                .unwrap_or_default()
        );
        println!("  {:<24} {} [{}]", "", s.help, s.env.dimmed());
    }
    Ok(0)
}
