mod actions;
mod daemon;
mod harness;
mod image_display;
mod payment;
mod review;
mod search;

use actions::{RunContext, outcome_to_json, run_actions, tool_schema};
use anyhow::{Context, Result, bail};
use daemon::{ManagedDaemon, headless, launch_browser_ephemeral, parse_run_request, session_id};
use payment::PaymentVault;
use std::{fs, io::Read};

fn main() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match args.first().map(String::as_str) {
        Some("chat") => harness::chat(),
        Some("stop") => ManagedDaemon::stop_stale(),
        Some("run") => run_cli(&args[1..]),
        Some("schema") => {
            println!("{}", serde_json::to_string_pretty(&tool_schema())?);
            Ok(())
        }
        None => bail!("{}", usage()),
        Some(path) if path.ends_with(".json") => run_cli(&[path.to_string()]),
        Some(_) => bail!("{}", usage()),
    }
}

fn run_cli(args: &[String]) -> Result<()> {
    let request = read_run_request(args)?;
    let payment = PaymentVault::load()?;
    let browser = launch_browser_ephemeral(headless(), None)?;
    let tab = browser.new_tab()?;
    let session = session_id();
    let mut context = RunContext {
        tab: &tab,
        payment: &payment,
        paused: None,
        session_id: &session,
        handoff_url: "",
    };
    let outcome = run_actions(&mut context, &request)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&outcome_to_json(outcome))?
    );
    Ok(())
}

fn read_run_requests(args: &[String]) -> Result<String> {
    match args.first() {
        Some(path) => fs::read_to_string(path)
            .with_context(|| format!("failed to read run request from {path}")),
        None => {
            let mut body = String::new();
            std::io::stdin().read_to_string(&mut body)?;
            Ok(body)
        }
    }
}

fn read_run_request(args: &[String]) -> Result<actions::RunRequest> {
    parse_run_request(&read_run_requests(args)?)
}

fn usage() -> &'static str {
    r#"Usage:
  cargo run -- chat
  cargo run -- stop
  cargo run -- schema
  cargo run -- run [request.json]
  cargo run -- request.json

  chat    start Emissary and the browser daemon together
  stop    clean up a stale daemon lock/processes
  run     execute JSON browser actions once (separate headless session)

Set VENICE_API_KEY before chat."#
}
