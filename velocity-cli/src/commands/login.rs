//! `velocity login <suite>` (row 1.5, AI-021 / VOI-001): run a stored login
//! flow so an agent can start from a logged-in app without ever handling
//! the password. claude-code #78160: agents refuse to type passwords, and
//! 13 of 17 real-app tests stopped at login.
//!
//! It is `velocity run` on one test (the first, or `--test`), with the
//! app left as the flow leaves it. Its `${NAME}`s come from the
//! environment, `--env`, `${keychain:NAME}` (the OS keychain) or
//! `${totp:NAME}` (a one-time code), and every value is masked in the
//! output and the reports. The exit code is `run`'s.

use clap::{Args, Parser};

#[derive(Args)]
pub struct LoginArgs {
    /// The suite holding the login flow
    suite: String,

    /// The test to run (default: the suite's first)
    #[arg(long, value_name = "NAME")]
    test: Option<String>,

    /// Device to log in on (default: the first booted one)
    #[arg(short, long)]
    device: Option<String>,

    /// ios, android or web
    #[arg(short, long)]
    platform: Option<String>,

    /// A value for a ${NAME} (repeatable), NAME=VALUE
    #[arg(long = "env", value_name = "NAME=VALUE")]
    env: Vec<String>,

    /// Where the reports go
    #[arg(long, default_value = "velocity-results/login", value_name = "DIR")]
    artifacts_dir: String,
}

#[derive(Parser)]
#[command(no_binary_name = true)]
struct AsRun {
    #[command(flatten)]
    args: super::run::RunArgs,
}

pub async fn execute(args: LoginArgs) -> anyhow::Result<i32> {
    let test = match args.test.clone() {
        Some(t) => t,
        None => {
            let suite = velocity_core::parse_suite(&args.suite)
                .map_err(|e| usage_err!("{}: {e}", args.suite))?;
            match suite.tests.first() {
                Some(t) => t.name.clone(),
                None => usage_bail!("{} has no test to log in with", args.suite),
            }
        }
    };
    let argv = run_argv(&args.suite, &test, &args);
    let run = AsRun::try_parse_from(&argv).map_err(|e| usage_err!("{e}"))?;
    Box::pin(super::run::execute(run.args)).await
}

/// `velocity run` arguments for the login: just that test, its own reports,
/// no result cache (a login must really run), no retries of a wrong
/// password (an account may lock).
fn run_argv(suite: &str, test: &str, args: &LoginArgs) -> Vec<String> {
    // The name as a glob that matches only itself.
    let exact: String = test
        .chars()
        .flat_map(|c| match c {
            '*' | '?' | '[' | ']' => vec!['[', c, ']'],
            c => vec![c],
        })
        .collect();
    let mut a: Vec<String> = vec![
        suite.into(),
        "--filter".into(),
        exact,
        "--retries".into(),
        "0".into(),
        "--artifacts-dir".into(),
        args.artifacts_dir.clone(),
        "--report".into(),
        "json".into(),
    ];
    if let Some(d) = &args.device {
        a.extend(["--device".into(), d.clone()]);
    }
    if let Some(p) = &args.platform {
        a.extend(["--platform".into(), p.clone()]);
    }
    for e in &args.env {
        a.extend(["--env".into(), e.clone()]);
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Parser)]
    #[command(no_binary_name = true)]
    struct P {
        #[command(flatten)]
        args: LoginArgs,
    }

    #[test]
    fn login_runs_one_test_once_with_its_env() {
        let p = P::try_parse_from([
            "login.yaml",
            "--test",
            "sign in*",
            "-d",
            "emulator-5554",
            "--env",
            "EMAIL=a@b.c",
        ])
        .unwrap();
        let argv = run_argv("login.yaml", "sign in*", &p.args);
        let run = AsRun::try_parse_from(&argv).expect("run accepts the arguments");
        drop(run);
        let joined = argv.join(" ");
        assert!(joined.contains("--filter sign in[*]"), "{joined}");
        assert!(
            joined.contains("--retries 0") && joined.contains("--device emulator-5554"),
            "{joined}"
        );
        assert!(joined.contains("--env EMAIL=a@b.c"), "{joined}");
    }
}
