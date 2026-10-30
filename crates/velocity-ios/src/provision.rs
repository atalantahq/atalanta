//! Simulator choices for `velocity devices up --ios N`.
//!
//! A fresh simulator spends 135–259 s on first-boot migration, so Velocity
//! never boots a stock one per run. It keeps clones named
//! `velocity-<type>-<n>` and reuses them, most recently used first (warm
//! caches, and the clone a previous run already installed the app on). New
//! clones come from a "golden" simulator that has already booted once:
//! `simctl clone` is an APFS copy, so the clone starts past migration.

use velocity_common::{Result, VelocityError};

/// A simulator from `simctl list devices --json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sim {
    pub udid: String,
    pub name: String,
    /// `Booted`, `Shutdown`, `Booting`, …
    pub state: String,
    pub device_type: String,
    pub runtime: String,
    /// ISO 8601, as simctl prints it; `None` if it never booted.
    pub last_booted_at: Option<String>,
}

impl Sim {
    pub fn booted(&self) -> bool {
        self.state == "Booted"
    }
    pub fn shutdown(&self) -> bool {
        self.state == "Shutdown"
    }
}

/// Available simulators.
pub fn parse_sims(v: &serde_json::Value) -> Vec<Sim> {
    let Some(runtimes) = v.get("devices").and_then(|d| d.as_object()) else {
        return Vec::new();
    };
    let s = |e: &serde_json::Value, k: &str| e.get(k).and_then(|x| x.as_str()).map(String::from);
    runtimes
        .iter()
        .flat_map(|(runtime, list)| {
            list.as_array()
                .into_iter()
                .flatten()
                .filter(|e| e.get("isAvailable").and_then(|a| a.as_bool()) == Some(true))
                .filter_map(move |e| {
                    Some(Sim {
                        udid: s(e, "udid")?,
                        name: s(e, "name")?,
                        state: s(e, "state").unwrap_or_default(),
                        device_type: s(e, "deviceTypeIdentifier").unwrap_or_default(),
                        runtime: runtime.clone(),
                        last_booted_at: s(e, "lastBootedAt"),
                    })
                })
        })
        .collect()
}

/// A device type from `simctl list devicetypes --json`: (name, identifier).
pub fn resolve_device_type(v: &serde_json::Value, wanted: &str) -> Result<(String, String)> {
    let types: Vec<(String, String)> = v
        .get("devicetypes")
        .and_then(|t| t.as_array())
        .into_iter()
        .flatten()
        .filter_map(|t| {
            Some((
                t.get("name")?.as_str()?.to_string(),
                t.get("identifier")?.as_str()?.to_string(),
            ))
        })
        .collect();
    types
        .iter()
        .find(|(name, id)| name.eq_ignore_ascii_case(wanted) || id == wanted)
        .cloned()
        .ok_or_else(|| {
            let iphones: Vec<&str> = types
                .iter()
                .map(|(n, _)| n.as_str())
                .filter(|n| n.starts_with("iPhone"))
                .collect();
            VelocityError::Config(format!(
                "no simulator device type '{wanted}'. iPhones available: {}",
                iphones.join(", ")
            ))
        })
}

/// `iPhone 17 Pro` → `velocity-iPhone-17-Pro`.
pub fn clone_prefix(type_name: &str) -> String {
    format!(
        "velocity-{}",
        type_name.split_whitespace().collect::<Vec<_>>().join("-")
    )
}

pub fn clone_name(type_name: &str, n: u32) -> String {
    format!("{}-{n}", clone_prefix(type_name))
}

pub fn golden_name(type_name: &str) -> String {
    format!("{}-golden", clone_prefix(type_name))
}

/// `n` of a `velocity-<type>-<n>` clone of this type.
pub fn clone_index(name: &str, type_name: &str) -> Option<u32> {
    name.strip_prefix(&clone_prefix(type_name))?
        .strip_prefix('-')?
        .parse()
        .ok()
}

/// Velocity's clones of this type, most recently used first.
pub fn clones<'a>(sims: &'a [Sim], type_name: &str) -> Vec<&'a Sim> {
    let mut out: Vec<&Sim> = sims
        .iter()
        .filter(|s| clone_index(&s.name, type_name).is_some())
        .collect();
    // ISO 8601 sorts as text; never-booted clones go last.
    out.sort_by(|a, b| b.last_booted_at.cmp(&a.last_booted_at));
    out
}

/// The lowest clone numbers not taken yet.
pub fn next_indexes(sims: &[Sim], type_name: &str, count: usize) -> Vec<u32> {
    let used: Vec<u32> = sims
        .iter()
        .filter_map(|s| clone_index(&s.name, type_name))
        .collect();
    (1..).filter(|i| !used.contains(i)).take(count).collect()
}

/// What to do to have `n` booted clones of a type.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Clones already booted (counted, not touched).
    pub already: Vec<String>,
    /// Clones to boot (`bootstatus -b`): one already booting first, then
    /// shut-down ones, most recently used first, then one still shutting
    /// down. A clone in transition used to be ignored and a new one
    /// cloned in its place (VEL-302).
    pub reuse: Vec<String>,
    /// New clones to make from the golden simulator, by number.
    pub create: Vec<u32>,
}

pub fn plan(sims: &[Sim], type_name: &str, n: usize) -> Plan {
    let all = clones(sims, type_name);
    let already: Vec<String> = all
        .iter()
        .filter(|s| s.booted())
        .take(n)
        .map(|s| s.udid.clone())
        .collect();
    let need = n - already.len();
    let in_state = |state: &'static str| all.iter().filter(move |s| s.state == state);
    let reuse: Vec<String> = in_state("Booting")
        .chain(all.iter().filter(|s| s.shutdown()))
        .chain(in_state("Shutting Down"))
        .take(need)
        .map(|s| s.udid.clone())
        .collect();
    let create = next_indexes(sims, type_name, need - reuse.len());
    Plan {
        already,
        reuse,
        create,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sim(udid: &str, name: &str, state: &str, booted: Option<&str>) -> Sim {
        Sim {
            udid: udid.into(),
            name: name.into(),
            state: state.into(),
            device_type: "com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro".into(),
            runtime: "com.apple.CoreSimulator.SimRuntime.iOS-26-0".into(),
            last_booted_at: booted.map(String::from),
        }
    }

    #[test]
    fn parses_available_simulators_with_type_and_last_boot() {
        let v = json!({ "devices": { "com.apple.CoreSimulator.SimRuntime.iOS-26-0": [
            { "udid": "A", "name": "iPhone 17 Pro", "state": "Shutdown", "isAvailable": true,
              "deviceTypeIdentifier": "com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro",
              "lastBootedAt": "2026-10-06T08:28:50Z" },
            { "udid": "B", "name": "Old", "state": "Shutdown", "isAvailable": false }
        ]}});
        let sims = parse_sims(&v);
        assert_eq!(sims.len(), 1);
        assert_eq!(
            sims[0].last_booted_at.as_deref(),
            Some("2026-10-06T08:28:50Z")
        );
        assert!(sims[0].shutdown());
    }

    #[test]
    fn device_types_match_by_name_or_identifier() {
        let v = json!({ "devicetypes": [
            { "name": "iPhone 17 Pro", "identifier": "com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro" },
            { "name": "iPhone 17", "identifier": "com.apple.CoreSimulator.SimDeviceType.iPhone-17" }
        ]});
        assert_eq!(resolve_device_type(&v, "iphone 17").unwrap().0, "iPhone 17");
        assert_eq!(
            resolve_device_type(&v, "com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro")
                .unwrap()
                .0,
            "iPhone 17 Pro"
        );
        let err = resolve_device_type(&v, "iPhone 99")
            .unwrap_err()
            .to_string();
        assert!(err.contains("iPhone 17 Pro, iPhone 17"), "{err}");
    }

    #[test]
    fn clone_names_are_per_type_and_numbered() {
        assert_eq!(clone_name("iPhone 17 Pro", 2), "velocity-iPhone-17-Pro-2");
        assert_eq!(golden_name("iPhone 17"), "velocity-iPhone-17-golden");
        assert_eq!(
            clone_index("velocity-iPhone-17-Pro-2", "iPhone 17 Pro"),
            Some(2)
        );
        // "iPhone 17" clones are not "iPhone 17 Pro" clones, and vice versa.
        assert_eq!(clone_index("velocity-iPhone-17-Pro-2", "iPhone 17"), None);
        assert_eq!(clone_index("velocity-iPhone-17-golden", "iPhone 17"), None);
    }

    #[test]
    fn reuses_the_most_recently_used_clones_before_making_new_ones() {
        let t = "iPhone 17 Pro";
        let sims = vec![
            sim(
                "old",
                "velocity-iPhone-17-Pro-1",
                "Shutdown",
                Some("2026-01-01T00:00:00Z"),
            ),
            sim(
                "up",
                "velocity-iPhone-17-Pro-2",
                "Booted",
                Some("2026-10-06T00:00:00Z"),
            ),
            sim(
                "recent",
                "velocity-iPhone-17-Pro-3",
                "Shutdown",
                Some("2026-10-01T00:00:00Z"),
            ),
            sim("never", "velocity-iPhone-17-Pro-5", "Shutdown", None),
            sim(
                "user",
                "iPhone 17 Pro",
                "Shutdown",
                Some("2026-10-06T08:00:00Z"),
            ),
        ];
        let p = plan(&sims, t, 3);
        assert_eq!(p.already, vec!["up"]);
        assert_eq!(p.reuse, vec!["recent", "old"]);
        assert!(p.create.is_empty());

        let p = plan(&sims, t, 6);
        assert_eq!(p.reuse, vec!["recent", "old", "never"]);
        assert_eq!(p.create, vec![4, 6], "fills the gaps in the numbering");

        let p = plan(&sims, t, 1);
        assert_eq!(
            p,
            Plan {
                already: vec!["up".into()],
                ..Default::default()
            }
        );
    }

    #[test]
    fn clones_in_transition_are_used_not_replaced() {
        // VEL-302: Booting / Shutting Down clones were invisible to the plan,
        // which cloned new simulators beside them.
        let t = "iPhone 17 Pro";
        let sims = vec![
            sim("down", "velocity-iPhone-17-Pro-1", "Shutdown", None),
            sim("leaving", "velocity-iPhone-17-Pro-2", "Shutting Down", None),
            sim("coming", "velocity-iPhone-17-Pro-3", "Booting", None),
        ];
        let p = plan(&sims, t, 3);
        assert_eq!(p.reuse, vec!["coming", "down", "leaving"]);
        assert!(p.create.is_empty(), "{p:?}");
        assert_eq!(plan(&sims, t, 1).reuse, vec!["coming"]);
    }
}
