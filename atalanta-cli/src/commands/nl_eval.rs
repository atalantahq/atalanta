//! `atalanta nl-eval` — measure natural-language planner consistency (pass^k)
//! on the traces saved by earlier runs, or compare models on a fixed task
//! set (`--model provider:model`, repeatable) and recommend a default.

use std::collections::HashMap;

use atalanta_nl::eval::{builtin_tasks, compare, evaluate_with, load_traces_counted, Price, Trace};
use atalanta_nl::provider::parse_model_spec;
use atalanta_nl::{AiSettings, NaturalPlanner};
use clap::Args;
use colored::Colorize;

#[derive(Args)]
pub struct NlEvalArgs {
    /// Directory of traces written by `atalanta run` (one per planned step)
    #[arg(long, default_value = atalanta_runner::nl::TRACES_DIR)]
    traces: String,

    /// pass^k: the chance that k re-plans all reproduce the recorded action
    #[arg(short, long, default_value_t = 5, value_parser = clap::value_parser!(u16).range(1..=32))]
    k: u16,

    /// Re-plans per trace (at least k; more gives a steadier pass^k).
    /// Defaults to k
    #[arg(short = 'n', long, value_parser = clap::value_parser!(u16).range(1..=32))]
    samples: Option<u16>,

    /// Fail (exit 1) when pass^k is below this fraction, e.g. 0.95
    #[arg(long, value_parser = fraction)]
    min_pass_k: Option<f64>,

    /// Exit 3 when more than this share of model calls errored (an outage
    /// or a bad key isn't a planner score). Answers the planner got wrong,
    /// including ones grounding rejected, are misses, not errors
    #[arg(long, default_value_t = 0.1, value_parser = fraction)]
    max_error_rate: f64,

    /// Write the full report as JSON to this path
    #[arg(long)]
    json: Option<String>,

    /// Compare models: provider:model, optionally @base-url, for any
    /// provider preset (anthropic:claude-haiku-5-5, gemini:gemini-3.8-flash,
    /// bedrock:anthropic.claude-haiku-5-5, ollama:llama3.1:8b; see
    /// docs/AI-PROVIDERS.md). Repeatable. Each runs the same task set k
    /// times per task
    #[arg(long = "model", value_name = "PROVIDER:MODEL")]
    models: Vec<String>,

    /// The task set: `builtin` (the fixed set shipped with Atalanta, the
    /// default with --model) or a directory of traces (the default without)
    #[arg(long)]
    tasks: Option<String>,

    /// Price per million tokens for the cost column:
    /// PROVIDER:MODEL=INPUT,OUTPUT in dollars. Repeatable
    #[arg(long = "price", value_name = "PROVIDER:MODEL=IN,OUT")]
    prices: Vec<String>,

    /// With --model: print which model to make the default (best pass^k,
    /// then cheapest among those within 2 points of it)
    #[arg(long)]
    recommend: bool,
}

/// `--price provider:model=IN,OUT`.
fn parse_price(s: &str) -> Result<(String, Price), String> {
    let bad =
        || format!("--price {s:?} must be PROVIDER:MODEL=IN,OUT (dollars per million tokens)");
    let (model, rest) = s.split_once('=').ok_or_else(bad)?;
    let (i, o) = rest.split_once(',').ok_or_else(bad)?;
    let num = |v: &str| {
        v.trim()
            .parse::<f64>()
            .ok()
            .filter(|n| n.is_finite() && *n >= 0.0)
    };
    match (num(i), num(o)) {
        (Some(i), Some(o)) => Ok((model.trim().to_string(), (i, o))),
        _ => Err(bad()),
    }
}

/// The task set: built in, or the traces in a directory.
fn load_tasks(dir_or_builtin: &str) -> anyhow::Result<Vec<Trace>> {
    if dir_or_builtin == "builtin" {
        return Ok(builtin_tasks());
    }
    let loaded = load_traces_counted(std::path::Path::new(dir_or_builtin))?;
    if let Some(skipped) = loaded.skip_summary() {
        println!("{} {skipped}", "!".yellow().bold());
    }
    if loaded.traces.is_empty() {
        usage_bail!(
            "no usable traces in {dir_or_builtin} — run a suite with natural-language steps first"
        );
    }
    Ok(loaded.traces)
}

/// `--model …`: every model on one task set, a table, a JSON report and,
/// with `--recommend`, the pick.
async fn execute_compare(args: &NlEvalArgs, k: usize, samples: usize) -> anyhow::Result<i32> {
    let tasks = load_tasks(args.tasks.as_deref().unwrap_or("builtin"))?;
    let prices: HashMap<String, Price> = args
        .prices
        .iter()
        .map(|p| parse_price(p))
        .collect::<Result<_, _>>()
        .map_err(|e| usage_err!("{e}"))?;
    let mut planners: Vec<(String, Box<dyn NaturalPlanner>)> = Vec::new();
    for spec in &args.models {
        let settings = parse_model_spec(spec)?;
        let planner = settings.planner()?;
        // The label as written, so --price matches it.
        planners.push((spec.clone(), planner));
    }
    println!(
        "{} {} model(s) x {} task(s) x{} ({} model calls)",
        "=>".cyan().bold(),
        planners.len(),
        tasks.len(),
        samples,
        planners.len() * tasks.len() * samples
    );
    let models: Vec<(String, &dyn NaturalPlanner)> = planners
        .iter()
        .map(|(l, p)| (l.clone(), p.as_ref()))
        .collect();
    let comparison = compare(&models, &tasks, samples, k, &prices).await;
    println!();
    print!("{}", comparison.table());
    let rec = comparison.recommend();
    if args.recommend {
        println!();
        match &rec {
            Some(r) => println!(
                "  {} recommended default: {} ({})",
                "✓".green(),
                r.model.bold(),
                r.why
            ),
            None => println!(
                "  {} no model to recommend: every one failed or errored",
                "!".yellow()
            ),
        }
    }
    if let Some(path) = &args.json {
        let doc = serde_json::json!({ "comparison": comparison, "recommendation": rec });
        std::fs::write(path, serde_json::to_string_pretty(&doc)?)?;
        println!("  report: {path}");
    }
    if comparison
        .rows
        .iter()
        .all(|r| r.report.error_rate > args.max_error_rate)
    {
        eprintln!(
            "{} every model's calls mostly failed; scores are not meaningful",
            "error:".red().bold()
        );
        return Ok(3);
    }
    Ok(match args.min_pass_k {
        Some(min) if comparison.rows.iter().any(|r| r.report.pass_hat_k < min) => 1,
        _ => 0,
    })
}

/// A share in [0, 1] (`NaN` and out-of-range values are refused).
fn fraction(s: &str) -> Result<f64, String> {
    match s.parse::<f64>() {
        Ok(v) if (0.0..=1.0).contains(&v) => Ok(v),
        _ => Err(format!("{s:?} is not a fraction between 0 and 1")),
    }
}

pub async fn execute(args: NlEvalArgs) -> anyhow::Result<i32> {
    let k = usize::from(args.k);
    let samples = args.samples.map(usize::from).unwrap_or(k);
    if samples < k {
        usage_bail!("-n {samples} is fewer than -k {k}: pass^k needs at least k re-plans");
    }
    if !args.models.is_empty() {
        return execute_compare(&args, k, samples).await;
    }
    if args.recommend {
        usage_bail!("--recommend compares models: give two or more --model provider:model");
    }
    let traces = load_tasks(args.tasks.as_deref().unwrap_or(&args.traces))?;
    let Some(settings) = AiSettings::from_env()? else {
        usage_bail!(
            "nl-eval needs a model: set ANTHROPIC_API_KEY, OPENAI_API_KEY, or \
             ATALANTA_NL_API_KEY with ATALANTA_NL_API_URL (or pass --model)"
        );
    };
    let planner = settings.planner()?;
    println!("{} model {}", "=>".cyan().bold(), settings.label());
    println!(
        "{} Re-planning {} step(s) x{} ({} model calls)",
        "=>".cyan().bold(),
        traces.len(),
        samples,
        traces.len() * samples
    );

    let report = evaluate_with(planner.as_ref(), &traces, samples, k).await;

    for r in &report.results {
        let mark = if r.errors == report.samples {
            "!".red()
        } else if r.matched == report.samples - r.errors {
            "✓".green()
        } else if r.matched == 0 {
            "✗".red()
        } else {
            "~".yellow()
        };
        println!(
            "  {mark} {} [{}] {}/{} reproduced, {} distinct answer(s), {} rejected, {} error(s)",
            r.intent, r.scope, r.matched, report.samples, r.distinct, r.rejected, r.errors
        );
        if let Some(d) = &r.divergent {
            println!("      differed: {d}");
        }
    }
    println!();
    println!(
        "  pass@1 {:.1}% (95% CI by trace {:.1}-{:.1}%)   pass^{} {:.1}%   self-consistent {:.1}%   errors {:.1}%   latency p50 {} ms / p95 {} ms",
        report.pass_at_1 * 100.0,
        report.pass_at_1_ci.0 * 100.0,
        report.pass_at_1_ci.1 * 100.0,
        report.k,
        report.pass_hat_k * 100.0,
        report.consistent * 100.0,
        report.error_rate * 100.0,
        report.p50_ms,
        report.p95_ms
    );

    if let Some(path) = &args.json {
        std::fs::write(path, serde_json::to_string_pretty(&report)?)?;
        println!("  report: {path}");
    }
    // Mostly errors: the scores mean nothing, and that is a service or
    // setup failure, not a planner result (VEL-039).
    if report.error_rate > args.max_error_rate {
        eprintln!(
            "{} {:.0}% of model calls failed (limit {:.0}%); scores are not meaningful",
            "error:".red().bold(),
            report.error_rate * 100.0,
            args.max_error_rate * 100.0
        );
        return Ok(3);
    }
    Ok(match args.min_pass_k {
        Some(min) if report.pass_hat_k < min => 1,
        _ => 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: NlEvalArgs,
    }

    // VEL-039: -k 0, a NaN or out-of-range --min-pass-k were accepted.
    #[test]
    fn k_and_fractions_are_range_checked() {
        assert!(Cli::try_parse_from(["x", "-k", "0"]).is_err());
        assert!(Cli::try_parse_from(["x", "-k", "33"]).is_err());
        assert!(Cli::try_parse_from(["x", "--min-pass-k", "NaN"]).is_err());
        assert!(Cli::try_parse_from(["x", "--min-pass-k", "1.5"]).is_err());
        assert!(Cli::try_parse_from(["x", "--max-error-rate", "-0.1"]).is_err());
        let ok = Cli::try_parse_from(["x", "-k", "3", "-n", "8", "--min-pass-k", "0.9"]).unwrap();
        assert_eq!((ok.args.k, ok.args.samples), (3, Some(8)));
    }

    #[test]
    fn models_prices_and_tasks_parse() {
        let c = Cli::try_parse_from([
            "x",
            "--model",
            "anthropic:claude-haiku-5-5",
            "--model",
            "ollama:llama3.1:8b@http://localhost:11434/v1",
            "--price",
            "anthropic:claude-haiku-5-5=1,5",
            "--tasks",
            "builtin",
            "--recommend",
        ])
        .unwrap();
        assert_eq!(c.args.models.len(), 2);
        assert!(c.args.recommend);
        assert_eq!(
            parse_price(&c.args.prices[0]).unwrap(),
            ("anthropic:claude-haiku-5-5".to_string(), (1.0, 5.0))
        );
        assert!(parse_price("openai:x=1").is_err());
        assert!(parse_price("openai:x=-1,2").is_err());
        assert!(load_tasks("builtin").unwrap().len() >= 12);
    }

    // The whole command against a local OpenAI-compatible mock: one model,
    // every task answered with element 0, a table and a JSON report.
    #[tokio::test]
    async fn compare_runs_end_to_end_against_a_mock_endpoint() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let body = serde_json::json!({
                "choices": [{ "finish_reason": "stop", "message": { "content":
                    "{\"op\":\"tap\",\"element\":0,\"text\":\"\",\"direction\":\"none\",\"key\":\"none\",\"ms\":0}" } }],
                "usage": { "prompt_tokens": 50, "completion_tokens": 5 }
            })
            .to_string();
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 1 << 16];
                let mut got = Vec::new();
                loop {
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    got.extend_from_slice(&buf[..n]);
                    let t = String::from_utf8_lossy(&got).to_string();
                    if let Some(end) = t.find("\r\n\r\n") {
                        let len = t
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if got.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        let json = std::env::temp_dir().join(format!("nl-eval-cmp-{}.json", std::process::id()));
        let spec = format!("openai-compatible:mock@{base}");
        let price = format!("{spec}=1,2");
        let c = Cli::try_parse_from([
            "x",
            "-k",
            "1",
            "--model",
            &spec,
            "--price",
            &price,
            "--recommend",
            "--json",
            json.to_str().unwrap(),
        ])
        .unwrap();
        let code = execute(c.args).await.unwrap();
        assert_eq!(code, 0);
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
        let row = &doc["comparison"]["rows"][0];
        assert_eq!(row["model"], spec.as_str());
        assert!(row["usage"]["inputTokens"].as_u64().unwrap() > 0);
        assert!(row["costUsd"].as_f64().unwrap() > 0.0);
        assert_eq!(doc["recommendation"]["model"], spec.as_str());
        let _ = std::fs::remove_file(json);
    }
}
