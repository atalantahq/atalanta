//! `velocity signing`: headless iOS provisioning through the App Store
//! Connect API (an API key, never an Apple ID password).

use clap::{Args, Subcommand};
use colored::Colorize;
use velocity_ios::asc::{AscClient, AscKeyRef};

#[derive(Args, Clone)]
pub struct KeyArgs {
    /// The API key's `.p8` file (else VELOCITY_ASC_KEY_PATH)
    #[arg(long)]
    key_path: Option<String>,
    /// The key's ID (else VELOCITY_ASC_KEY_ID)
    #[arg(long)]
    key_id: Option<String>,
    /// The team's issuer ID (else VELOCITY_ASC_ISSUER_ID)
    #[arg(long)]
    issuer_id: Option<String>,
}

#[derive(Subcommand)]
pub enum SigningCommand {
    /// Check the API key works, and list the team's development certificates
    Check {
        #[command(flatten)]
        key: KeyArgs,
    },
    /// List the iOS devices registered with the team
    Devices {
        #[command(flatten)]
        key: KeyArgs,
    },
    /// Register iPhones/iPads with the team (default: every one connected)
    RegisterDevice {
        /// A device UDID (repeatable); none = all connected real devices
        #[arg(long)]
        udid: Vec<String>,
        /// Name for a single --udid
        #[arg(long)]
        name: Option<String>,
        #[command(flatten)]
        key: KeyArgs,
    },
    /// Create a development profile for a bundle id and save it
    Profile {
        /// Bundle id to sign (created on the team if missing)
        #[arg(long)]
        bundle_id: String,
        /// Where to write the .mobileprovision
        #[arg(long, short)]
        out: String,
        /// Devices to include (registered first if needed); none = every enabled device
        #[arg(long)]
        udid: Vec<String>,
        #[command(flatten)]
        key: KeyArgs,
    },
}

fn key_ref(k: &KeyArgs) -> anyhow::Result<AscKeyRef> {
    let env = AscKeyRef::from_env()?;
    let path = k
        .key_path
        .clone()
        .or(env.as_ref().map(|e| e.path.display().to_string()));
    let key_id = k.key_id.clone().or(env.as_ref().map(|e| e.key_id.clone()));
    let issuer = k
        .issuer_id
        .clone()
        .or(env.as_ref().map(|e| e.issuer_id.clone()));
    match (path, key_id, issuer) {
        (Some(path), Some(key_id), Some(issuer_id)) => Ok(AscKeyRef {
            path: path.into(),
            key_id,
            issuer_id,
        }),
        _ => usage_bail!(
            "an App Store Connect API key is needed: --key-path/--key-id/--issuer-id or \
             VELOCITY_ASC_KEY_PATH / VELOCITY_ASC_KEY_ID / VELOCITY_ASC_ISSUER_ID \
             (App Store Connect → Users and Access → Integrations)"
        ),
    }
}

fn client(k: &KeyArgs) -> anyhow::Result<AscClient> {
    let key = key_ref(k)?.load()?;
    Ok(AscClient::new(key))
}

pub async fn execute(command: SigningCommand) -> anyhow::Result<i32> {
    let e = anyhow::Error::from;
    match command {
        SigningCommand::Check { key } => {
            let c = client(&key)?;
            c.check().await.map_err(e)?;
            println!("{} API key works", "✓".green().bold());
            let certs = c.development_certificates().await.map_err(e)?;
            if certs.is_empty() {
                println!(
                    "  {} no development certificate on the team yet; xcodebuild creates one \
                     on the first signed build when VELOCITY_ASC_* is set",
                    "!".yellow()
                );
            }
            for cert in certs {
                println!(
                    "  certificate {}  {}  (expires {})",
                    cert.id, cert.name, cert.expires
                );
            }
            Ok(0)
        }
        SigningCommand::Devices { key } => {
            for d in client(&key)?.devices().await.map_err(e)? {
                let state = if d.enabled {
                    "enabled".green()
                } else {
                    "disabled".dimmed()
                };
                println!("  {:<42} {:<28} {state}", d.udid, d.name);
            }
            Ok(0)
        }
        SigningCommand::RegisterDevice { udid, name, key } => {
            let c = client(&key)?;
            let targets: Vec<(String, String)> = if udid.is_empty() {
                // Real devices only: simulators have no team registration.
                velocity_ios::devicectl::Devicectl::new()
                    .list_devices()
                    .await
                    .into_iter()
                    .map(|d| (d.id, d.name))
                    .collect()
            } else {
                udid.iter()
                    .map(|u| {
                        (
                            u.clone(),
                            name.clone().unwrap_or_else(|| format!("Velocity {u}")),
                        )
                    })
                    .collect()
            };
            if targets.is_empty() {
                return Err(super::no_device(
                    "no connected iPhone/iPad found; pass --udid",
                    Vec::new(),
                ));
            }
            for (u, n) in targets {
                let (d, added) = c.ensure_device(&u, &n).await.map_err(e)?;
                let what = if added {
                    "registered".green()
                } else {
                    "already registered".dimmed()
                };
                println!("  {} {} ({}) {what}", "✓".green(), d.udid, d.name);
            }
            Ok(0)
        }
        SigningCommand::Profile {
            bundle_id,
            out,
            udid,
            key,
        } => {
            let c = client(&key)?;
            let (bundle_res, created) = c
                .ensure_bundle_id(&bundle_id, &format!("Velocity {bundle_id}"))
                .await
                .map_err(e)?;
            if created {
                println!("  {} created bundle id {bundle_id}", "✓".green());
            }
            let certs = c.development_certificates().await.map_err(e)?;
            if certs.is_empty() {
                anyhow::bail!(
                    "the team has no development certificate; run one signed build with \
                     VELOCITY_ASC_* set (xcodebuild creates it) or create one in Xcode"
                );
            }
            let mut device_ids = Vec::new();
            if udid.is_empty() {
                device_ids.extend(
                    c.devices()
                        .await
                        .map_err(e)?
                        .into_iter()
                        .filter(|d| d.enabled)
                        .map(|d| d.id),
                );
            } else {
                for u in &udid {
                    device_ids.push(
                        c.ensure_device(u, &format!("Velocity {u}"))
                            .await
                            .map_err(e)?
                            .0
                            .id,
                    );
                }
            }
            if device_ids.is_empty() {
                anyhow::bail!("no registered devices to put in the profile; run `velocity signing register-device`");
            }
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let bytes = c
                .create_development_profile(
                    &format!("Velocity {bundle_id} {stamp}"),
                    &bundle_res,
                    &certs.iter().map(|c| c.id.clone()).collect::<Vec<_>>(),
                    &device_ids,
                )
                .await
                .map_err(e)?;
            std::fs::write(&out, &bytes)?;
            println!(
                "{} profile for {bundle_id}: {} certificate(s), {} device(s) → {out}",
                "✓".green().bold(),
                certs.len(),
                device_ids.len()
            );
            Ok(0)
        }
    }
}
