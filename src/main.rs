mod actions;
mod browser_dom;
mod daemon;
mod harness;
mod image_display;
mod payment;
mod review;
mod search;

use actions::{RunContext, outcome_to_json, run_actions, tool_schema};
use anyhow::{Context, Result, bail};
use daemon::{ManagedDaemon, headless, launch_browser_ephemeral, parse_run_request, session_id};
use payment::{OnePasswordSetup, PaymentVault};
use std::{
    fs,
    io::{self, Read, Write},
};

fn main() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match args.first().map(String::as_str) {
        Some("chat") => harness::chat(),
        Some("stop") => ManagedDaemon::stop_stale(),
        Some("setup") => setup_cli(),
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

fn setup_cli() -> Result<()> {
    println!("Configure 1Password checkout items for Emissary.");
    println!("This stores item names/IDs only, not decrypted card or address values.\n");

    let vault = prompt_optional("1Password vault (optional, blank if item names are unique)")?;
    let card = prompt_required("Credit Card item title or ID")?;
    let address = prompt_optional("Shared Identity/address item (optional)")?;
    let billing_address = prompt_optional("Billing Identity/address item (optional)")?;
    let shipping_address = prompt_optional("Shipping Identity/address item (optional)")?;

    let setup = OnePasswordSetup {
        vault,
        card,
        address,
        billing_address,
        shipping_address,
    };
    let path = PaymentVault::save_setup(&setup)?;

    println!("\nSaved 1Password setup to {}", path.display());
    println!("Loaded payment profile: default");
    println!("Run `cargo run -- chat` to start Emissary.");
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

fn prompt_required(label: &str) -> Result<String> {
    loop {
        let value = prompt(label)?;
        if !value.is_empty() {
            return Ok(value);
        }
        println!("{label} is required.");
    }
}

fn prompt_optional(label: &str) -> Result<Option<String>> {
    let value = prompt(label)?;
    Ok((!value.is_empty()).then_some(value))
}

fn prompt(label: &str) -> Result<String> {
    print!("{label}: ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_owned())
}

fn usage() -> &'static str {
    r#"Usage:
  cargo run -- chat
  cargo run -- setup
  cargo run -- stop
  cargo run -- schema
  cargo run -- run [request.json]
  cargo run -- request.json

  chat    start Emissary and the browser daemon together
  setup   configure 1Password checkout item references
  stop    clean up a stale daemon lock/processes
  run     execute JSON browser actions once (separate headless session)

Set VENICE_API_KEY before chat."#
}
