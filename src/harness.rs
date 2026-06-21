use crate::actions::Action;
use crate::daemon::{ManagedDaemon, install_shutdown_handler, runtime_dir};
use crate::payment::PaymentVault;
use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde_json::{Map, Value, json};
use std::{
    fs,
    io::{self, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

const MAX_AGENT_STEPS: usize = 12;

pub fn chat() -> Result<()> {
    let runtime_dir = runtime_dir()?;
    let llm = LlmConfig::load()?;
    let daemon = ManagedDaemon::start()?;
    let status = daemon.status();
    println!(
        "Telephone started (session {}, handoff on demand)",
        status
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
    );

    let shared = Arc::new(Mutex::new(daemon));
    install_shutdown_handler(shared.clone())?;

    let schema = {
        let guard = shared.lock().expect("daemon mutex poisoned");
        guard.schema()
    };
    let payment_keys = {
        let guard = shared.lock().expect("daemon mutex poisoned");
        guard.payment_keys()
    };
    if payment_keys.is_empty() {
        eprintln!(
            "warning: no payment profiles loaded; edit {}",
            PaymentVault::payment_file_path().display()
        );
    }

    let mut messages = vec![json!({
        "role": "system",
        "content": system_prompt(&payment_keys, &status),
    })];
    let tool = openai_tool(&schema);
    println!("Type a message, or 'exit' to quit.\n");

    loop {
        print!("you> ");
        io::stdout().flush()?;
        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if matches!(line, "exit" | "quit") {
            break;
        }

        messages.push(json!({ "role": "user", "content": line }));
        let reply = run_agent_turn(&shared, &llm, &tool, &mut messages, &runtime_dir)?;
        messages.push(reply.message);
        println!("\nassistant> {}\n", reply.text);
    }

    shared.lock().expect("daemon mutex poisoned").shutdown();
    println!("Telephone stopped.");
    Ok(())
}

struct LlmConfig {
    api_key: String,
    base_url: String,
    model: String,
    http: reqwest::blocking::Client,
}

impl LlmConfig {
    fn load() -> Result<Self> {
        let api_key =
            std::env::var("VENICE_API_KEY").context("set VENICE_API_KEY for Venice AI")?;
        let timeout = env_u64("VENICE_TIMEOUT_SECS", 300);
        Ok(Self {
            api_key,
            base_url: env_string("VENICE_BASE_URL", "https://api.venice.ai/api/v1"),
            model: env_string("VENICE_MODEL", "deepseek-v4-flash"),
            http: reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(timeout))
                .connect_timeout(Duration::from_secs(30))
                .build()
                .context("failed to build Venice HTTP client")?,
        })
    }

    fn complete(&self, messages: &[Value], tools: &[Value]) -> Result<Value> {
        let mut request = Map::new();
        request.insert("model".to_owned(), json!(self.model));
        request.insert("messages".to_owned(), json!(messages));
        if !tools.is_empty() {
            request.insert("tools".to_owned(), json!(tools));
            request.insert("tool_choice".to_owned(), json!("auto"));
        }

        let response = self
            .http
            .post(format!(
                "{}/chat/completions",
                self.base_url.trim_end_matches('/')
            ))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&Value::Object(request))
            .send()
            .with_context(|| {
                format!(
                    "LLM request failed (timeout {}s; set VENICE_TIMEOUT_SECS to override)",
                    env_u64("VENICE_TIMEOUT_SECS", 300)
                )
            })?;

        let status = response.status();
        let raw = response.text().context("failed to read LLM response")?;
        let payload: Value = serde_json::from_str(&raw).with_context(|| {
            format!(
                "LLM returned non-JSON response (HTTP {status}): {}",
                truncate_error_body(&raw)
            )
        })?;
        if !status.is_success() {
            bail!(
                "LLM HTTP {status}: {}",
                serde_json::to_string_pretty(&payload)
                    .unwrap_or_else(|_| truncate_error_body(&raw))
            );
        }
        if let Some(error) = payload.get("error") {
            bail!(
                "LLM error: {}",
                serde_json::to_string_pretty(error).unwrap_or_else(|_| error.to_string())
            );
        }

        payload
            .get("choices")
            .and_then(|choices| choices.get(0))
            .and_then(|choice| choice.get("message"))
            .cloned()
            .context("LLM returned no message")
    }
}

fn truncate_error_body(raw: &str) -> String {
    const MAX_CHARS: usize = 4000;
    if raw.len() <= MAX_CHARS {
        return raw.to_owned();
    }

    let mut end = MAX_CHARS;
    while end > 0 && !raw.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}... [truncated, {} chars total]", &raw[..end], raw.len())
}

struct AgentReply {
    message: Value,
    text: String,
}

fn run_agent_turn(
    daemon: &Arc<Mutex<ManagedDaemon>>,
    llm: &LlmConfig,
    tool: &Value,
    messages: &mut Vec<Value>,
    runtime_dir: &PathBuf,
) -> Result<AgentReply> {
    for _ in 0..MAX_AGENT_STEPS {
        let response = llm.complete(messages, std::slice::from_ref(tool))?;
        let tool_calls = response
            .get("tool_calls")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        if tool_calls.is_empty() {
            let text = response
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            return Ok(AgentReply {
                message: json!({ "role": "assistant", "content": text }),
                text,
            });
        }

        messages.push(response);

        for call in tool_calls {
            let mut stop_after_tool_error = false;
            let call_id = call
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let function = call.get("function").cloned().unwrap_or(Value::Null);
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let arguments = function
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");

            let tool_content = if name != "browser" {
                stop_after_tool_error = true;
                json!({ "error": format!("unknown tool {name}") }).to_string()
            } else {
                match parse_browser_arguments(arguments) {
                    Ok(actions) => {
                        let (status, body) =
                            daemon.lock().expect("daemon mutex poisoned").run(actions)?;
                        if status == 409 || body.get("status") == Some(&json!("needs_human")) {
                            show_handoff_to_user(&body, runtime_dir)?;
                        } else if body.get("status") == Some(&json!("error")) {
                            show_browser_error_to_user(&body);
                            stop_after_tool_error = true;
                        }
                        format_tool_result_for_model(status, &body)
                    }
                    Err(error) => {
                        stop_after_tool_error = true;
                        json!({ "status": "error", "error": error.to_string() }).to_string()
                    }
                }
            };

            messages.push(json!({
                "role": "tool",
                "tool_call_id": call_id,
                "content": tool_content,
            }));

            if stop_after_tool_error {
                messages.push(json!({
                    "role": "system",
                    "content": "The browser tool just returned a recoverable error. Do not call tools again in this turn. Briefly explain what failed and ask the user how to proceed or suggest a safer next step.",
                }));
                let response = llm.complete(messages, &[])?;
                let text = response
                    .get("content")
                    .and_then(Value::as_str)
                    .filter(|text| !text.trim().is_empty())
                    .unwrap_or("The browser action failed. Please choose how you want to proceed.")
                    .to_owned();
                return Ok(AgentReply {
                    message: json!({ "role": "assistant", "content": text }),
                    text,
                });
            }
        }
    }

    let text =
        "I hit the step limit for this turn. Please continue or narrow the request.".to_owned();
    Ok(AgentReply {
        message: json!({ "role": "assistant", "content": text }),
        text,
    })
}

fn parse_browser_arguments(arguments: &str) -> Result<Vec<Action>> {
    let parsed: Value =
        serde_json::from_str(arguments).context("invalid browser tool arguments")?;
    parsed
        .get("actions")
        .and_then(|actions| serde_json::from_value(actions.clone()).ok())
        .ok_or_else(|| anyhow::anyhow!("browser tool arguments missing actions"))
}

fn openai_tool(schema: &Value) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": schema.get("name").cloned().unwrap_or(json!("browser")),
            "description": schema.get("description").cloned().unwrap_or(Value::Null),
            "parameters": schema.get("parameters").cloned().unwrap_or(json!({})),
        }
    })
}

fn system_prompt(payment_keys: &[String], status: &Value) -> String {
    let profiles = if payment_keys.is_empty() {
        "none loaded".to_owned()
    } else {
        payment_keys
            .iter()
            .map(|key| format!("`{key}`"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let session = status
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("unknown");

    format!(
        "You are Telephone, a minimal personal assistant with one tool: browser.\n\
         Use the browser tool to carry out web tasks in a persistent Chrome session. \
         Send short ordered batches of JSON actions. Each tool result includes `title` and \
         `pageText` plus `elements` refs (visible controls after the batch finishes). Use `html` only when you need markup.\n\
         Search:\n\
         - Use webSearch for factual/entity lookup via DuckDuckGo Instant Answer before opening fragile browser pages.\n\
         - webSearch is for discovery and reading; browser is for interactive/session tasks.\n\
         Selectors:\n\
         - Prefer observe -> clickRef/typeRef. Do not invent CSS selectors when an element ref is available.\n\
         - click/type/wait use standard CSS (document.querySelector). No :contains(), :text(), or Playwright syntax.\n\
         - Prefer simple selectors (#id, [aria-label=\"...\"], input[name=\"...\"]).\n\
         - When you only know visible label text, use clickText instead of click.\n\
         - XPath works in click/wait/type when the selector starts with //.\n\
         Errors:\n\
         - If an action fails (missing element, invalid selector, etc.), the tool returns status error with pageText so you can adjust and retry.\n\
         - If pageState is bot_challenge or mode is blocked, stop retrying automation. Ask whether to use another site, continue manually in a normal browser, or retry after the user has cleared Cloudflare outside the harness.\n\
         Payment:\n\
         - Never put card numbers or CVV in tool arguments.\n\
         - Use fillPayment with a profile key when checkout needs card details. Loaded profiles: {profiles}.\n\
         Human handoff:\n\
         - Final purchase submits are blocked by the runtime.\n\
         - When the tool returns needs_human with mode review, summarize the basket/total for the user. \
         A screenshot is shown to them separately; do not reproduce base64.\n\
         - When mode is interactive, the user must complete bank/app authentication via handoff_url.\n\
         - When mode is blocked, do not call more browser actions until the user chooses a new path.\n\
         - After the user says they are done, call browser with {{ \"op\": \"resume\" }} before continuing.\n\
         Session: {session}\n\
         Be concise. Ask the user when you are stuck, need a decision, or waiting on handoff."
    )
}

fn show_handoff_to_user(body: &Value, runtime_dir: &PathBuf) -> Result<()> {
    if let Some(reason) = body.get("reason").and_then(Value::as_str) {
        println!("\n[handoff] {reason}");
    }

    if let Some(summary) = body
        .pointer("/review/order_summary")
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
    {
        println!("\n--- Order review ---\n{summary}");
    }

    if let Some(screenshot) = body
        .pointer("/review/screenshot_base64")
        .and_then(Value::as_str)
        .filter(|data| !data.is_empty())
    {
        let png = BASE64
            .decode(screenshot)
            .context("failed to decode review screenshot")?;
        fs::create_dir_all(runtime_dir)?;
        let path = runtime_dir.join("review-latest.png");
        fs::write(&path, png)?;
        println!("\nScreenshot saved: {}", path.display());
    }

    if let Some(url) = body.get("handoff_url").and_then(Value::as_str) {
        let label = if body.get("mode") == Some(&json!("blocked")) {
            "Open to inspect the blocked browser"
        } else {
            "Open for submit or bank auth"
        };
        println!("\n{label}: {url}");
    }

    if body.get("mode") == Some(&json!("blocked")) {
        println!(
            "\nCloudflare is blocking the automated browser. If the challenge never fully loads in noVNC, continue in a normal browser or choose another site."
        );
    } else if body.get("mode") == Some(&json!("interactive")) {
        let reason = body
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if reason.contains("bot challenge")
            || body.get("pageState") == Some(&json!("bot_challenge"))
        {
            println!(
                "\nSites like Uber Eats often block automation behind Cloudflare. Complete the check in the handoff browser, then tell Telephone you are done."
            );
        } else {
            println!("\nComplete authentication in the browser, then tell Telephone you are done.");
        }
    } else {
        println!(
            "\nReview the order, submit via the handoff browser if it looks right, then say you are done."
        );
    }

    Ok(())
}

fn show_browser_error_to_user(body: &Value) {
    if let Some(error) = body.get("error").and_then(Value::as_str) {
        eprintln!("\n[browser] {error}");
    }
}

fn format_tool_result_for_model(status: u16, body: &Value) -> String {
    let mut sanitized = body.clone();
    strip_screenshot_data(&mut sanitized);
    let mut out = Map::new();
    out.insert("http_status".to_owned(), json!(status));
    if let Value::Object(map) = sanitized {
        out.extend(map);
    }
    Value::Object(out).to_string()
}

fn strip_screenshot_data(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(data)) = map.get("screenshot_base64") {
                map.insert(
                    "screenshot_base64".to_owned(),
                    json!(format!(
                        "[omitted {} base64 chars; shown to user]",
                        data.len()
                    )),
                );
            }
            for nested in map.values_mut() {
                strip_screenshot_data(nested);
            }
        }
        Value::Array(items) => {
            for item in items {
                strip_screenshot_data(item);
            }
        }
        _ => {}
    }
}

fn env_string(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}
