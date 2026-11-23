//! `velocity run --profile <file>`: a Chrome trace of the run.
//!
//! Every span (test, step, settle, …) becomes a "complete" event with its
//! duration; each test gets its own track, so parallel runs line up per
//! test. Open the file in https://ui.perfetto.dev or `chrome://tracing`.
//! Written by hand instead of pulling in `tracing-chrome`: it is about a
//! hundred lines on top of `tracing-subscriber`.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use serde_json::{json, Value};
use tracing::span::{Attributes, Id};
use tracing::{field::Visit, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

struct Shared {
    path: String,
    start: Instant,
    events: Mutex<Vec<Value>>,
}

static PROFILE: OnceLock<Arc<Shared>> = OnceLock::new();

/// Start recording; [`flush`] writes `path`.
pub fn layer(path: String) -> ChromeLayer {
    let shared = PROFILE
        .get_or_init(|| {
            Arc::new(Shared {
                path,
                start: Instant::now(),
                events: Mutex::new(Vec::new()),
            })
        })
        .clone();
    ChromeLayer { shared }
}

/// Write the trace, if one is being recorded. Call before exiting.
pub fn flush() {
    let Some(shared) = PROFILE.get() else { return };
    let events = std::mem::take(&mut *shared.events.lock().unwrap_or_else(|e| e.into_inner()));
    let doc = json!({ "traceEvents": events, "displayTimeUnit": "ms" });
    match write(&shared.path, doc) {
        Ok(()) => eprintln!(
            "=> Profile: {} (open in https://ui.perfetto.dev)",
            shared.path
        ),
        Err(e) => eprintln!("profile: could not write {}: {e}", shared.path),
    }
}

/// The trace as written: redacted (event fields carry driver errors and
/// heal warnings with interpolated values, VEL-322) and readable by the
/// owner only, as profiles get uploaded with CI artifacts. The string
/// values are redacted, not the serialised text: an all-digit secret (a
/// PIN, an OTP) was masked inside timestamps and made the JSON invalid.
fn write(path: &str, mut doc: Value) -> std::io::Result<()> {
    velocity_common::redact::redact_value(&mut doc);
    velocity_common::fsutil::write_private(std::path::Path::new(path), doc.to_string().as_bytes())
}

pub struct ChromeLayer {
    shared: Arc<Shared>,
}

/// Stored on each span: when it opened, its fields, its track.
struct Open {
    at: Instant,
    args: serde_json::Map<String, Value>,
    track: u64,
}

#[derive(Default)]
struct Fields(serde_json::Map<String, Value>);

impl Visit for Fields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_string(), json!(format!("{value:?}")));
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_string(), json!(value));
    }
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0.insert(field.name().to_string(), json!(value));
    }
    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.0.insert(field.name().to_string(), json!(value));
    }
    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.0.insert(field.name().to_string(), json!(value));
    }
}

impl ChromeLayer {
    fn us(&self, at: Instant) -> u64 {
        at.saturating_duration_since(self.shared.start).as_micros() as u64
    }

    fn push(&self, event: Value) {
        self.shared
            .events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(event);
    }
}

impl<S> Layer<S> for ChromeLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut fields = Fields::default();
        attrs.record(&mut fields);
        // A test's spans share its track; spans outside a test get their own.
        let track = span
            .scope()
            .from_root()
            .next()
            .map(|root| root.id().into_u64())
            .unwrap_or_else(|| id.into_u64());
        span.extensions_mut().insert(Open {
            at: Instant::now(),
            args: fields.0,
            track,
        });
    }

    fn on_record(&self, id: &Id, values: &tracing::span::Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut ext = span.extensions_mut();
        if let Some(open) = ext.get_mut::<Open>() {
            let mut fields = Fields::default();
            values.record(&mut fields);
            open.args.extend(fields.0);
        }
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else { return };
        let ext = span.extensions();
        let Some(open) = ext.get::<Open>() else {
            return;
        };
        let name = match open.args.get("name").and_then(Value::as_str) {
            Some(n) if span.name() == "test" => format!("test: {n}"),
            _ => match open.args.get("action").and_then(Value::as_str) {
                Some(a) => format!("{} {a}", span.name()),
                None => span.name().to_string(),
            },
        };
        let start = self.us(open.at);
        let dur = self.us(Instant::now()).saturating_sub(start);
        if span.name() == "test" {
            // Label the track with the test's name.
            self.push(json!({
                "name": "thread_name",
                "ph": "M",
                "pid": 1,
                "tid": open.track,
                "args": { "name": name },
            }));
        }
        self.push(json!({
            "name": name,
            "cat": span.metadata().target(),
            "ph": "X",
            "ts": start,
            "dur": dur,
            "pid": 1,
            "tid": open.track,
            "args": open.args,
        }));
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        let track = ctx
            .event_scope(event)
            .and_then(|s| s.from_root().next().map(|r| r.id().into_u64()))
            .unwrap_or(0);
        let name = fields
            .0
            .remove("message")
            .and_then(|m| m.as_str().map(|s| s.trim_matches('"').to_string()))
            .unwrap_or_else(|| event.metadata().name().to_string());
        self.push(json!({
            "name": name,
            "cat": event.metadata().target(),
            "ph": "i",
            "s": "t",
            "ts": self.us(Instant::now()),
            "pid": 1,
            "tid": track,
            "args": fields.0,
        }));
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn the_profile_is_redacted_and_private() {
        use std::os::unix::fs::PermissionsExt;
        velocity_common::redact::register_secret("prof-S3cret-9921");
        let path =
            std::env::temp_dir().join(format!("velocity-profile-{}.json", std::process::id()));
        let doc = serde_json::json!({
            "traceEvents": [{ "name": "driver call failed", "args": { "error": "no element text=\"prof-S3cret-9921\"" } }]
        });
        super::write(&path.to_string_lossy(), doc).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("S3cret"), "{text}");
        assert!(serde_json::from_str::<serde_json::Value>(&text).is_ok());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_file(&path);
    }

    // m-4: an all-digit secret (a PIN) is masked in strings only; numbers
    // that contain it stay numbers and the file stays valid JSON.
    #[test]
    fn a_numeric_secret_does_not_corrupt_the_trace() {
        velocity_common::redact::register_secret("48213977");
        let path =
            std::env::temp_dir().join(format!("velocity-profile-num-{}.json", std::process::id()));
        let doc = serde_json::json!({
            "traceEvents": [{ "name": "step", "ts": 1248213977u64, "dur": 482139770u64,
                "args": { "text": "pin 48213977" } }]
        });
        super::write(&path.to_string_lossy(), doc).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let back: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        let ev = &back["traceEvents"][0];
        assert_eq!(ev["ts"], 1248213977u64);
        assert_eq!(ev["dur"], 482139770u64);
        assert!(!ev["args"]["text"].as_str().unwrap().contains("48213977"));
        let _ = std::fs::remove_file(&path);
    }
}
