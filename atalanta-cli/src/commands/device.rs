// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

use atalanta_common::{DeviceState, Platform};
use clap::Subcommand;
use colored::Colorize;

use super::create_driver;

#[derive(Subcommand)]
pub enum DeviceCommand {
    /// Boot devices for a run: N headless read-only emulators of one AVD
    /// and/or N simulator clones; optionally install the app on all of them.
    /// Refuses to boot below ~35%/3 GB free memory or 3 GB free disk.
    Up {
        /// Atalanta-started Android emulators to have running (read-only
        /// instances of one AVD). Emulators started some other way don't
        /// count, so `down` only ever stops what `up` booted
        #[arg(long, default_value_t = 0, value_name = "N")]
        android: usize,

        /// Atalanta's iOS simulators to have booted (`atalanta-<type>-<n>`
        /// clones; other booted simulators don't count)
        #[arg(long, default_value_t = 0, value_name = "N")]
        ios: usize,

        /// AVD to run [default: the first of `emulator -list-avds`]
        #[arg(long)]
        avd: Option<String>,

        /// Simulator type, e.g. "iPhone 17 Pro" [default: the type of the
        /// most recently used iPhone simulator]
        #[arg(long)]
        device_type: Option<String>,

        /// Shut-down simulator (UDID or name) to clone new simulators from
        /// [default: atalanta-<type>-golden, created and booted once]
        #[arg(long)]
        golden: Option<String>,

        /// App build to install on every device (.apk / .app), skipped where
        /// that exact build is already installed
        #[arg(long)]
        app: Option<String>,

        /// Bundle id / package of --app, when it can't be read from the build
        #[arg(long)]
        app_id: Option<String>,

        /// First emulator console port to try
        #[arg(long, default_value_t = atalanta_android::emulator::FIRST_PORT)]
        port: u16,

        /// Who these devices belong to; `devices down` stops only its own
        /// [default: $ATALANTA_OWNER, else the CI job id, else none]
        #[arg(long, value_name = "ID")]
        owner: Option<String>,
    },

    /// Stop the devices `up` started (nothing else)
    Down {
        /// Only this platform (ios or android)
        #[arg(short, long)]
        platform: Option<String>,

        /// Also delete the simulator clones Atalanta created
        #[arg(long)]
        delete: bool,

        /// Stop only the devices `up` started for this owner
        /// [default: $ATALANTA_OWNER, else the CI job id, else none]
        #[arg(long, value_name = "ID", conflicts_with = "all")]
        owner: Option<String>,

        /// Stop the devices every owner started (another CI job's too)
        #[arg(long)]
        all: bool,
    },

    /// List available devices
    List {
        /// Filter by platform (ios or android)
        #[arg(short, long)]
        platform: Option<String>,
    },

    /// Boot a device by name or ID
    Boot {
        /// Device name or ID
        name: String,

        /// Platform (ios or android); from the id when it is one
        /// (`emulator-5554`, a UDID), else ios
        #[arg(short, long)]
        platform: Option<String>,
    },

    /// Shutdown a device by name or ID
    Shutdown {
        /// Device name or ID
        name: String,

        /// Platform (ios or android); from the id when it is one
        /// (`emulator-5554`, a UDID), else ios
        #[arg(short, long)]
        platform: Option<String>,
    },

    /// Take a screenshot of a device
    Screenshot {
        /// Device name or ID
        name: String,

        /// Platform (ios or android); from the id when it is one
        /// (`emulator-5554`, a UDID), else ios
        #[arg(short, long)]
        platform: Option<String>,

        /// Output file path
        #[arg(short, long, default_value = "screenshot.png")]
        output: String,
    },
}

/// `--platform`, or the platform `name` plainly belongs to when it is a
/// device id. Names ("iPhone 17 Pro", an AVD) say nothing: iOS, as before.
fn platform_for(name: &str, platform: Option<&str>) -> anyhow::Result<Platform> {
    if let Some(p) = platform {
        return parse_platform(p);
    }
    let android_id = name.starts_with("emulator-") || name.contains(':');
    Ok(match Platform::of_device_id(name) {
        Platform::Ios => Platform::Ios,
        _ if android_id => Platform::Android,
        _ => Platform::Ios,
    })
}

fn parse_platform(s: &str) -> anyhow::Result<Platform> {
    match s {
        "ios" => Ok(Platform::Ios),
        "android" => Ok(Platform::Android),
        other => usage_bail!("Unknown platform '{other}'. Use 'ios' or 'android'."),
    }
}

pub async fn execute(command: DeviceCommand) -> anyhow::Result<i32> {
    match command {
        DeviceCommand::Up {
            android,
            ios,
            avd,
            device_type,
            golden,
            app,
            app_id,
            port,
            owner,
        } => {
            super::provision::up(super::provision::UpArgs {
                android,
                ios,
                avd,
                device_type,
                golden,
                app,
                app_id,
                port,
                owner,
            })
            .await
        }

        DeviceCommand::Down {
            platform,
            delete,
            owner,
            all,
        } => {
            let p = platform.as_deref().map(parse_platform).transpose()?;
            super::provision::down(super::provision::DownArgs {
                platform: p,
                delete,
                owner,
                all,
            })
            .await
        }

        DeviceCommand::List { platform } => {
            let platforms = match platform.as_deref() {
                Some(p) => vec![parse_platform(p)?],
                None => vec![Platform::Ios, Platform::Android],
            };

            println!("{} Scanning for devices...", "=>".cyan().bold());
            println!();
            // Every platform first, then columns as wide as their longest
            // entry: fixed widths (and a platform Display that ignores
            // padding) sheared the table on names like "iPad Pro 11-inch (M4)".
            let mut rows: Vec<Row> = Vec::new();
            for p in platforms {
                let driver = create_driver(p);
                // Silently skip platforms that aren't available.
                if let Ok(devices) = driver.list_devices().await {
                    for d in devices {
                        rows.push((d.id, d.name, d.platform.to_string(), d.state));
                    }
                }
            }
            let (wi, wn, wp) = column_widths(&rows);
            println!(
                "  {}  {}  {}  {}",
                pad("ID", wi).bold(),
                pad("NAME", wn).bold(),
                pad("PLATFORM", wp).bold(),
                "STATE".bold()
            );
            println!("  {}", "─".repeat(wi + wn + wp + 6 + 8).dimmed());
            for (id, name, platform, state) in &rows {
                let state_colored = match state {
                    DeviceState::Booted => state.to_string().green(),
                    DeviceState::Shutdown => state.to_string().dimmed(),
                    DeviceState::Unknown => state.to_string().yellow(),
                };
                println!(
                    "  {}  {}  {}  {state_colored}",
                    pad(id, wi),
                    pad(name, wn),
                    pad(platform, wp)
                );
            }

            println!();
            Ok(0)
        }

        DeviceCommand::Boot { name, platform } => {
            let p = platform_for(&name, platform.as_deref())?;
            println!("{} Booting device {}...", "=>".cyan().bold(), name.bold());
            let driver = create_driver(p);
            driver.boot_device(&name).await?;
            println!(
                "{} Device {} is now booted",
                "=>".green().bold(),
                name.bold()
            );
            Ok(0)
        }

        DeviceCommand::Shutdown { name, platform } => {
            let p = platform_for(&name, platform.as_deref())?;
            println!(
                "{} Shutting down device {}...",
                "=>".cyan().bold(),
                name.bold()
            );
            let driver = create_driver(p);
            driver.shutdown_device(&name).await?;
            println!(
                "{} Device {} is now shut down",
                "=>".green().bold(),
                name.bold()
            );
            Ok(0)
        }

        DeviceCommand::Screenshot {
            name,
            platform,
            output,
        } => {
            let p = platform_for(&name, platform.as_deref())?;
            println!(
                "{} Taking screenshot of {}...",
                "=>".cyan().bold(),
                name.bold()
            );
            let driver = create_driver(p);
            let png_data = driver.screenshot(&name).await?;
            std::fs::write(&output, &png_data)?;
            println!(
                "{} Screenshot saved to {} ({} bytes)",
                "=>".green().bold(),
                output.underline(),
                png_data.len()
            );
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::platform_for;
    use atalanta_common::Platform;

    #[test]
    fn device_commands_read_the_platform_from_ids_not_names() {
        let p = |n: &str| platform_for(n, None).unwrap();
        assert_eq!(p("emulator-5554"), Platform::Android);
        assert_eq!(p("192.168.1.5:5555"), Platform::Android);
        assert_eq!(p("290403AA-C40F-404B-9CB0-DAFB6A943142"), Platform::Ios);
        assert_eq!(p("iPhone 17 Pro"), Platform::Ios);
        assert_eq!(
            platform_for("zii_arm64", Some("android")).unwrap(),
            Platform::Android
        );
    }
}

/// One `device list` line: id, name, platform, state.
type Row = (String, String, String, DeviceState);

/// `s` padded with spaces to `width` characters (not bytes: "Beth’s iPhone").
fn pad(s: &str, width: usize) -> String {
    let n = s.chars().count();
    format!("{s}{}", " ".repeat(width.saturating_sub(n)))
}

/// Widths of the ID, NAME and PLATFORM columns: the longest entry or header.
fn column_widths(rows: &[Row]) -> (usize, usize, usize) {
    let w = |f: &dyn Fn(&Row) -> &String, header: &str| {
        rows.iter()
            .map(|r| f(r).chars().count())
            .chain(std::iter::once(header.len()))
            .max()
            .unwrap_or(0)
    };
    (
        w(&|r| &r.0, "ID"),
        w(&|r| &r.1, "NAME"),
        w(&|r| &r.2, "PLATFORM"),
    )
}

#[cfg(test)]
mod table_tests {
    use super::*;

    #[test]
    fn columns_fit_the_longest_entry_counting_characters() {
        let rows = vec![
            (
                "emulator-5554".to_string(),
                "Pixel".to_string(),
                "android".to_string(),
                DeviceState::Booted,
            ),
            (
                "A1".to_string(),
                "Beth’s iPad Pro 11-inch (M4)".to_string(),
                "ios".to_string(),
                DeviceState::Shutdown,
            ),
        ];
        assert_eq!(column_widths(&rows), (13, 28, 8));
        assert_eq!(pad("Beth’s", 8).chars().count(), 8);
        assert_eq!(pad("toolong", 3), "toolong");
    }
}
