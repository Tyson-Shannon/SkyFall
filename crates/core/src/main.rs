use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use skyfall_core::collect::Collector;
use skyfall_core::config::Config;
use skyfall_core::runner::Runner;
use skyfall_core::VERSION;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<()> {
    let (cmd, cfg_path) = parse_args(args);
    let cfg = match &cfg_path {
        Some(p) => Config::load(p)?,
        None => Config::default(),
    };

    match cmd.as_str() {
        "version" => {
            println!("skyfall v{VERSION}");
            Ok(())
        }
        "collect" => {
            let snap = Collector::new(cfg).collect()?;
            println!("{}", serde_json::to_string_pretty(&snap)?);
            Ok(())
        }
        _ => {
            let mut runner = Runner::spawn(cfg)?;
            wait_for_sigint();
            log::info!("shutting down (SIGINT)");
            runner.stop();
            runner.outcome().transpose()?;
            Ok(())
        }
    }
}

/// Block until Ctrl+C. A fresh subscription each call is fine here — there is
/// exactly one await, no loop, so a signal can never be dropped.
fn wait_for_sigint() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("building signal runtime");
    let _ = rt.block_on(tokio::signal::ctrl_c());
}

/// `skyfall [--config <path>] [collect]`
fn parse_args(args: &[String]) -> (String, Option<PathBuf>) {
    let mut cmd = String::new();
    let mut cfg = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--config" | "-c" => cfg = it.next().map(PathBuf::from),
            other if other.starts_with("--config=") => {
                cfg = Some(PathBuf::from(other.trim_start_matches("--config=")))
            }
            other if !other.starts_with('-') && cmd.is_empty() => cmd = other.to_string(),
            other => {
                eprintln!("ignoring unknown argument: {other}");
            }
        }
    }
    if cmd.is_empty() {
        cmd = "run".to_string();
    }
    (cmd, cfg)
}
