//! `velocity settings`: every named setting, its variable, default and values.

use clap::Args;
use colored::Colorize;
use velocity_common::settings::SETTINGS;

#[derive(Args)]
pub struct SettingsArgs {
    /// Only settings whose name contains this (e.g. `android`, `ios`)
    filter: Option<String>,
}

pub fn execute(args: SettingsArgs) -> anyhow::Result<i32> {
    let filter = args.filter.unwrap_or_default().to_lowercase();
    println!(
        "{} Set in a suite (`config: settings: {{ android: {{ scroll: semantic }} }}`) or per run \
         (`velocity run --set android.scroll=semantic`). `--set` also takes any suite config key \
         (`--set sync.reactionMs=150`).\n",
        "=>".cyan().bold()
    );
    for s in SETTINGS
        .iter()
        .filter(|s| s.key.to_lowercase().contains(&filter))
    {
        let current = velocity_common::env::var(s.env).ok();
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
