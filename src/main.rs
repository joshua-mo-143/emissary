mod actions;
mod args;
mod browser_dom;
mod conversation;
mod daemon;
mod harness;
mod image_display;
mod payment;
mod privacy;
mod review;

use actions::{RunContext, outcome_to_json, run_actions, tool_schema};
use anyhow::{Context, Result};
use args::Args;
use daemon::{ManagedDaemon, headless, launch_browser_ephemeral, parse_run_request, session_id};
use payment::{OnePasswordSetup, PaymentVault};
use std::{
    fs,
    io::{self, Read, Write},
    path::Path,
};

fn main() -> Result<()> {
    match args::parse()? {
        Args::Chat(options) => harness::chat(options),
        Args::Stop => ManagedDaemon::stop_stale(),
        Args::Setup => setup_cli(),
        Args::Run { request_json } => run_cli(request_json.as_deref()),
        Args::Schema => {
            println!("{}", serde_json::to_string_pretty(&tool_schema())?);
            Ok(())
        }
    }
}

fn run_cli(request_json: Option<&Path>) -> Result<()> {
    let request = read_run_request(request_json)?;
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

fn read_run_requests(request_json: Option<&Path>) -> Result<String> {
    match request_json {
        Some(path) => fs::read_to_string(path)
            .with_context(|| format!("failed to read run request from {}", path.display())),
        None => {
            let mut body = String::new();
            std::io::stdin().read_to_string(&mut body)?;
            Ok(body)
        }
    }
}

fn read_run_request(request_json: Option<&Path>) -> Result<actions::RunRequest> {
    parse_run_request(&read_run_requests(request_json)?)
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
