use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug)]
pub enum Args {
    Chat(ChatOptions),
    Setup,
    Stop,
    Run { request_json: Option<PathBuf> },
    Schema,
}

#[derive(Debug)]
pub struct ChatOptions {
    pub new: bool,
    pub resume: Option<String>,
}

#[derive(Debug, Parser)]
#[command(
    name = "emissary-agent",
    about = "Minimal assistant harness with a built-in browser-use tool",
    arg_required_else_help = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[arg(value_name = "REQUEST_JSON", help = "Run a request JSON file directly")]
    request_json: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start Emissary and the browser daemon together.
    Chat {
        /// Start a fresh conversation instead of resuming the latest one.
        #[arg(long)]
        new: bool,
        /// Resume a specific conversation session ID.
        #[arg(long, value_name = "SESSION_ID")]
        resume: Option<String>,
    },
    /// Configure 1Password checkout item references.
    Setup,
    /// Clean up a stale daemon lock/processes.
    Stop,
    /// Execute JSON browser actions once in a separate headless session.
    Run {
        /// Request JSON file. Reads from stdin when omitted.
        request_json: Option<PathBuf>,
    },
    /// Print the browser tool JSON schema.
    Schema,
}

pub fn parse() -> Result<Args> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Chat { new, resume }) => {
            if new && resume.is_some() {
                bail!("--new and --resume cannot be used together");
            }
            Ok(Args::Chat(ChatOptions { new, resume }))
        }
        Some(Command::Setup) => Ok(Args::Setup),
        Some(Command::Stop) => Ok(Args::Stop),
        Some(Command::Run { request_json }) => Ok(Args::Run { request_json }),
        Some(Command::Schema) => Ok(Args::Schema),
        None => {
            let path = cli
                .request_json
                .context("missing command or request JSON file")?;
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                bail!("expected a command or a .json request file");
            }
            Ok(Args::Run {
                request_json: Some(path),
            })
        }
    }
}
