//! `velocity baselines`: review the `assertScreenshot` comparisons the last
//! run failed, and accept the actual screen as the new baseline (U1).
//!
//! The same list the inspector's `/review` page shows (`--serve` opens it).
//! Accepting writes the baseline file atomically; commit it afterwards.

use clap::{Args, Subcommand};
use colored::Colorize;
use velocity_core::baselines::{self, PendingBaseline};

#[derive(Subcommand)]
pub enum BaselinesCommand {
    /// List failed screenshot comparisons: name, diff image and the
    /// baseline accepting would replace
    Review(ReviewArgs),
    /// Make the actual screen the baseline: an assertScreenshot name, a
    /// listed id (`diffs/home`) or `all`
    Accept(PickArgs),
    /// Keep the baseline and drop the comparison from the list
    Reject(PickArgs),
}

#[derive(Args)]
pub struct ResultsDir {
    /// The run's artifacts folder (`run --artifacts-dir`)
    #[arg(long, default_value = "velocity-results")]
    results_dir: std::path::PathBuf,
}

#[derive(Args)]
pub struct ReviewArgs {
    #[command(flatten)]
    dir: ResultsDir,

    /// Print the list as JSON
    #[arg(long, conflicts_with = "serve")]
    json: bool,

    /// Serve the inspector's review page (images side by side, Accept and
    /// Reject buttons) instead of printing the list
    #[arg(long)]
    serve: bool,

    /// Port for --serve
    #[arg(long, default_value = "9876", requires = "serve")]
    port: u16,

    /// Address for --serve. Defaults to loopback; anything else is still
    /// behind the per-session token in the printed URL
    #[arg(long, default_value = "127.0.0.1", requires = "serve")]
    host: std::net::IpAddr,

    /// With --serve: the page's Accept writes into this `velocity-baselines`
    /// folder (same `<platform>/<W>x<H>/<name>.png` inside) instead of the
    /// path the run recorded: for artifacts downloaded from a CI runner
    #[arg(long, value_name = "DIR", requires = "serve")]
    baselines_dir: Option<std::path::PathBuf>,
}

#[derive(Args)]
pub struct PickArgs {
    /// assertScreenshot name, listed id, or `all`
    #[arg(value_name = "NAME|all")]
    which: String,

    /// Accept into this `velocity-baselines` folder (same
    /// `<platform>/<W>x<H>/<name>.png` inside) instead of the path the run
    /// recorded: for artifacts downloaded from a CI runner
    #[arg(long, value_name = "DIR")]
    baselines_dir: Option<std::path::PathBuf>,

    #[command(flatten)]
    dir: ResultsDir,
}

pub async fn execute(command: BaselinesCommand) -> anyhow::Result<i32> {
    match command {
        BaselinesCommand::Review(args) => review(args).await,
        BaselinesCommand::Accept(args) => pick(args, true),
        BaselinesCommand::Reject(args) => pick(args, false),
    }
}

async fn review(args: ReviewArgs) -> anyhow::Result<i32> {
    let dir = args.dir.results_dir;
    if args.serve {
        let listener = velocity_inspector::InspectorServer::bind_on(args.host, args.port).await?;
        let addr = listener.local_addr()?;
        let server = velocity_inspector::InspectorServer::review_only(&dir)
            .with_baselines_dir(args.baselines_dir);
        eprintln!(
            "{} Screenshot review for {} at {}",
            "=>".cyan().bold(),
            dir.display(),
            server.review_url(addr).underline()
        );
        server.start_with_listener(listener).await?;
        return Ok(0);
    }
    let list = baselines::pending(&dir);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&list)?);
        return Ok(0);
    }
    if list.is_empty() {
        println!(
            "{} No failed screenshot comparisons to review in {}",
            "=>".cyan().bold(),
            dir.display()
        );
        return Ok(0);
    }
    println!(
        "{} {} failed screenshot comparison(s) in {}:",
        "=>".cyan().bold(),
        list.len(),
        dir.display()
    );
    for p in &list {
        print_one(p);
    }
    println!(
        "\nAccept with `velocity baselines accept <name|all>`, or open them side by side \
         with `velocity baselines review --serve`."
    );
    Ok(0)
}

fn print_one(p: &PendingBaseline) {
    println!("\n  {} ({})", p.name.bold(), p.id);
    if !p.detail.is_empty() {
        println!("    {}", p.detail);
    }
    let image = p.diff.as_ref().unwrap_or(&p.actual);
    println!("    diff:     {}", image.display());
    println!("    actual:   {}", p.actual.display());
    println!("    baseline: {}", p.baseline.display());
}

fn pick(args: PickArgs, accept: bool) -> anyhow::Result<i32> {
    let dir = args.dir.results_dir;
    let list = baselines::pending(&dir);
    let chosen = baselines::select(&list, &args.which);
    if chosen.is_empty() {
        if args.which == "all" {
            println!(
                "{} No failed screenshot comparisons to review in {}",
                "=>".cyan().bold(),
                dir.display()
            );
            return Ok(0);
        }
        let names: Vec<&str> = list.iter().map(|p| p.name.as_str()).collect();
        usage_bail!(
            "No failed screenshot comparison named {:?} in {} (pending: {})",
            args.which,
            dir.display(),
            if names.is_empty() {
                "none".to_string()
            } else {
                names.join(", ")
            }
        );
    }
    let mut failed = 0;
    let results: Vec<_> = if accept {
        baselines::accept_batch(&chosen, args.baselines_dir.as_deref())
            .into_iter()
            .map(|(p, r)| {
                (
                    p,
                    r.map(|path| format!("accepted; wrote {}", path.display())),
                )
            })
            .collect()
    } else {
        chosen
            .iter()
            .map(|p| {
                let r = baselines::reject(p)
                    .map(|()| "rejected; baseline unchanged".to_string())
                    .map_err(baselines::AcceptError::Failed);
                (*p, r)
            })
            .collect()
    };
    for (p, done) in results {
        match done {
            Ok(msg) => println!("{} {}: {msg}", "=>".cyan().bold(), p.name),
            // An out-of-date record is skipped, not a failure.
            Err(baselines::AcceptError::Stale(e)) => {
                eprintln!("{} {}: {e}", "warning:".yellow().bold(), p.name)
            }
            Err(e) => {
                failed += 1;
                eprintln!("{} {}: {e}", "error:".red().bold(), p.name);
            }
        }
    }
    if accept && failed == 0 {
        println!("Commit the baseline file(s) with your change.");
    }
    // A baseline that couldn't be written is a file-system problem.
    Ok(if failed > 0 { 5 } else { 0 })
}
