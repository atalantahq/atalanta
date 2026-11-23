/// `bail!` for a usage or configuration error: exit 2, not the generic
/// internal-error code.
macro_rules! usage_bail {
    ($($t:tt)*) => {
        return Err($crate::commands::usage(format!($($t)*)))
    };
}

/// `anyhow!` for a usage or configuration error (exit 2).
macro_rules! usage_err {
    ($($t:tt)*) => {
        $crate::commands::usage(format!($($t)*))
    };
}

pub mod agent;
pub mod api;
pub mod baselines;
pub mod daemon;
pub mod device;
pub mod doctor;
pub mod duration_arg;
pub mod env_file;
pub mod explore;
pub mod gate;
pub mod heal;
pub mod history;
pub mod init;
pub mod inspect;
pub mod lint;
pub mod login;
pub mod mcp;
pub mod migrate;
pub mod nl_eval;
pub mod provision;
pub mod repl;
pub mod results;
pub mod run;
pub mod schema;
pub mod settings;
pub mod shard_plan;
pub mod signing;
pub mod trace;
pub mod validate;
pub mod verify_change;

use atalanta_common::{AtalantaError, Platform, PlatformDriver};

/// A suite load error as shown under `✗`: a configuration error without
/// its `Configuration error:` prefix (the line already says what failed),
/// a missing file as one plain sentence with what to do.
pub fn load_error(path: &str, e: &AtalantaError) -> String {
    if !std::path::Path::new(path).exists() {
        return format!(
            "{path} does not exist (atalanta init writes a first suite; atalanta run -c FILE picks another)"
        );
    }
    match e {
        AtalantaError::Config(m) => m.clone(),
        other => other.to_string(),
    }
}

/// A usage or configuration error as an `anyhow::Error` that exits 2.
pub fn usage(message: String) -> anyhow::Error {
    AtalantaError::Config(message).into()
}

/// "No device to use" as an error that exits 3 (a device problem), with
/// `message` as what is printed.
pub fn no_device(message: &str, available: Vec<String>) -> anyhow::Error {
    anyhow::Error::new(AtalantaError::DeviceNotFound {
        id: String::new(),
        available,
    })
    .context(message.to_string())
}

/// The exit code for a command's error. A `AtalantaError` (anywhere in the
/// chain) has its own; anything else is an internal error, 5 — it used to
/// be 1, which says "tests failed".
pub fn exit_code_for(e: &anyhow::Error) -> i32 {
    e.chain()
        .find_map(|c| c.downcast_ref::<AtalantaError>())
        .map_or(5, AtalantaError::exit_code)
}

pub fn create_driver(platform: Platform) -> Box<dyn PlatformDriver> {
    match platform {
        Platform::Ios => Box::new(atalanta_ios::IosDriver::new()),
        Platform::Android => Box::new(atalanta_android::AndroidDriver::new()),
        // Default window and headless Chrome; `run` applies a suite's `web:`.
        Platform::Web => Box::new(atalanta_web::WebDriver::new(
            atalanta_web::WebDriverConfig::default(),
        )),
    }
}
