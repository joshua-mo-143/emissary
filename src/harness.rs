use crate::actions::{Action, BrowserToolArguments};
use crate::args::ChatOptions;
use crate::conversation::{
    ConversationOrigin, ConversationSelection, ConversationSession, ConversationStore,
};
use crate::daemon::{ManagedDaemon, install_shutdown_handler, runtime_dir};
use crate::image_display::{self, InlineImageResult};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{
    io::{self, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

const MAX_AGENT_STEPS: usize = 12;
const POST_TOOL_CONTINUE_PROMPT: &str = "\
Continue the user's task now. Your previous response looked like an intermediate progress update \
after a browser tool result, not a final answer. If the task is complete or you need user input, \
state that clearly; otherwise call the browser tool for the next step.";

pub fn chat(options: ChatOptions) -> Result<()> {
    let runtime_dir = runtime_dir()?;
    let llm = LlmConfig::load()?;
    let daemon = ManagedDaemon::start()?;
    let status = daemon.status();
    println!(
        "Emissary started (session {}, handoff on demand)",
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
    let mut conversation =
        ConversationStore::new(&runtime_dir).open(conversation_selection(options))?;
    let mut messages = vec![ChatMessage::system(system_prompt(&payment_keys, &status))];
    messages.extend(conversation.messages().iter().cloned());
    match conversation.origin() {
        ConversationOrigin::New => println!("Conversation: {} (new)", conversation.id()),
        ConversationOrigin::Resumed => println!(
            "Conversation: {} (resumed, {} messages)",
            conversation.id(),
            conversation.messages().len()
        ),
    }
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

        let user_message = ChatMessage::user(line);
        conversation.append_message(&user_message)?;
        messages.push(user_message);
        println!("\nassistant> Working... input is paused until I finish.");
        io::stdout().flush()?;
        let _input_guard = TerminalInputGuard::block();
        let reply = run_agent_turn(
            &shared,
            &llm,
            &tool,
            &mut messages,
            &runtime_dir,
            &mut conversation,
        )?;
        conversation.append_message(&reply.message)?;
        messages.push(reply.message);
        println!("\nassistant> {}\n", reply.text);
    }

    shared.lock().expect("daemon mutex poisoned").shutdown();
    println!("Emissary stopped.");
    Ok(())
}

struct TerminalInputGuard {
    #[cfg(unix)]
    active: bool,
}

#[cfg(unix)]
static TERMINAL_RESTORE_ON_EXIT: std::sync::Once = std::sync::Once::new();

#[cfg(unix)]
static TERMINAL_ORIGINAL: Mutex<Option<libc::termios>> = Mutex::new(None);

impl TerminalInputGuard {
    fn block() -> Self {
        #[cfg(unix)]
        {
            Self::block_unix()
        }

        #[cfg(not(unix))]
        {
            Self {}
        }
    }

    #[cfg(unix)]
    fn block_unix() -> Self {
        TERMINAL_RESTORE_ON_EXIT.call_once(|| unsafe {
            libc::atexit(restore_terminal_input_at_exit);
        });

        let fd = libc::STDIN_FILENO;
        if unsafe { libc::isatty(fd) } != 1 {
            return Self { active: false };
        }

        let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
        if unsafe { libc::tcgetattr(fd, original.as_mut_ptr()) } != 0 {
            return Self { active: false };
        }

        let original = unsafe { original.assume_init() };
        let mut blocked = original;
        blocked.c_lflag &= !(libc::ECHO | libc::ICANON);
        blocked.c_cc[libc::VMIN] = 0;
        blocked.c_cc[libc::VTIME] = 0;

        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &blocked) } != 0 {
            return Self { active: false };
        }

        *TERMINAL_ORIGINAL.lock().expect("terminal mutex poisoned") = Some(original);
        Self { active: true }
    }
}

impl Drop for TerminalInputGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if self.active {
            restore_terminal_input();
        }
    }
}

#[cfg(unix)]
extern "C" fn restore_terminal_input_at_exit() {
    restore_terminal_input();
}

#[cfg(unix)]
fn restore_terminal_input() {
    let Some(original) = TERMINAL_ORIGINAL
        .lock()
        .expect("terminal mutex poisoned")
        .take()
    else {
        return;
    };

    let fd = libc::STDIN_FILENO;
    unsafe {
        libc::tcflush(fd, libc::TCIFLUSH);
        libc::tcsetattr(fd, libc::TCSANOW, &original);
    }
}

fn conversation_selection(options: ChatOptions) -> ConversationSelection {
    if options.new {
        ConversationSelection::New
    } else if let Some(id) = options.resume {
        ConversationSelection::Resume(id)
    } else {
        ConversationSelection::ResumeLatest
    }
}

struct LlmConfig {
    api_key: String,
    base_url: String,
    model: String,
    http: reqwest::blocking::Client,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ChatMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

impl ChatMessage {
    pub(crate) fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_owned(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub(crate) fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_owned(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub(crate) fn assistant_text(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_owned(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub(crate) fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".to_owned(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
        }
    }

    pub(crate) fn content_text(&self) -> &str {
        self.content.as_deref().unwrap_or_default()
    }

    pub(crate) fn is_system(&self) -> bool {
        self.role == "system"
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ToolCall {
    id: String,
    #[serde(default)]
    r#type: Option<String>,
    function: ToolFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ToolFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, Serialize)]
struct ChatCompletionRequest {
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<&'static str>,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    #[serde(default)]
    choices: Vec<ChatChoice>,
    #[serde(default)]
    error: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessage,
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

    fn complete(&self, messages: &[ChatMessage], tools: &[Value]) -> Result<ChatMessage> {
        let request = ChatCompletionRequest {
            model: self.model.clone(),
            messages: messages.to_vec(),
            tools: tools.to_vec(),
            tool_choice: (!tools.is_empty()).then_some("auto"),
        };

        let response = self
            .http
            .post(format!(
                "{}/chat/completions",
                self.base_url.trim_end_matches('/')
            ))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&request)
            .send()
            .with_context(|| {
                format!(
                    "LLM request failed (timeout {}s; set VENICE_TIMEOUT_SECS to override)",
                    env_u64("VENICE_TIMEOUT_SECS", 300)
                )
            })?;

        let status = response.status();
        let raw = response.text().context("failed to read LLM response")?;
        let payload: ChatCompletionResponse = serde_json::from_str(&raw).with_context(|| {
            format!(
                "LLM returned non-JSON response (HTTP {status}): {}",
                truncate_error_body(&raw)
            )
        })?;
        if !status.is_success() {
            bail!(
                "LLM HTTP {status}: {}",
                serde_json::to_string_pretty(
                    &serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| json!(raw))
                )
                .unwrap_or_else(|_| truncate_error_body(&raw))
            );
        }
        if let Some(error) = payload.error {
            bail!(
                "LLM error: {}",
                serde_json::to_string_pretty(&error).unwrap_or_else(|_| error.to_string())
            );
        }

        payload
            .choices
            .into_iter()
            .next()
            .map(|choice| choice.message)
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
    message: ChatMessage,
    text: String,
}

fn run_agent_turn(
    daemon: &Arc<Mutex<ManagedDaemon>>,
    llm: &LlmConfig,
    tool: &Value,
    messages: &mut Vec<ChatMessage>,
    runtime_dir: &PathBuf,
    conversation: &mut ConversationSession,
) -> Result<AgentReply> {
    let mut post_tool_followup_pending = false;

    for _ in 0..MAX_AGENT_STEPS {
        let response = llm.complete(messages, std::slice::from_ref(tool))?;
        let tool_calls = response.tool_calls.clone().unwrap_or_default();

        if tool_calls.is_empty() {
            let text = response.content_text().to_owned();
            if post_tool_followup_pending && should_auto_continue_after_tool_response(&text) {
                let assistant_message = ChatMessage::assistant_text(text);
                conversation.append_message(&assistant_message)?;
                messages.push(assistant_message);
                let continue_message = ChatMessage::user(POST_TOOL_CONTINUE_PROMPT);
                conversation.append_message(&continue_message)?;
                messages.push(continue_message);
                post_tool_followup_pending = false;
                continue;
            }

            return Ok(AgentReply {
                message: ChatMessage::assistant_text(text.clone()),
                text,
            });
        }

        post_tool_followup_pending = false;
        conversation.append_message(&response)?;
        messages.push(response);

        for call in tool_calls {
            let mut stop_after_tool_error = false;
            let mut successful_tool_result = false;
            let call_id = call.id.clone();
            let name = call.function.name.as_str();
            let arguments = call.function.arguments.as_str();

            let tool_content = if name != "browser" {
                stop_after_tool_error = true;
                json!({ "error": format!("unknown tool {name}") }).to_string()
            } else {
                match parse_browser_arguments(arguments) {
                    Ok(actions) => {
                        let response =
                            daemon.lock().expect("daemon mutex poisoned").run(actions)?;
                        if response.needs_human_handoff() {
                            show_handoff_to_user(response.body(), runtime_dir)?;
                        } else if response.is_error() {
                            show_browser_error_to_user(response.body());
                            stop_after_tool_error = true;
                        } else {
                            show_browser_images_to_user(response.body(), runtime_dir)?;
                            successful_tool_result = true;
                        }
                        format_tool_result_for_model(response.body())
                    }
                    Err(error) => {
                        stop_after_tool_error = true;
                        json!({ "status": "error", "error": error.to_string() }).to_string()
                    }
                }
            };

            let tool_message = ChatMessage::tool(call_id, tool_content);
            conversation.append_message(&tool_message)?;
            messages.push(tool_message);
            post_tool_followup_pending |= successful_tool_result;

            if stop_after_tool_error {
                messages.push(ChatMessage::system("The browser tool just returned a recoverable error. Do not call tools again in this turn. Briefly explain what failed and ask the user how to proceed or suggest a safer next step."));
                let response = llm.complete(messages, &[])?;
                let text = response
                    .content
                    .as_deref()
                    .filter(|text| !text.trim().is_empty())
                    .unwrap_or("The browser action failed. Please choose how you want to proceed.")
                    .to_owned();
                return Ok(AgentReply {
                    message: ChatMessage::assistant_text(text.clone()),
                    text,
                });
            }
        }
    }

    let text =
        "I hit the step limit for this turn. Please continue or narrow the request.".to_owned();
    Ok(AgentReply {
        message: ChatMessage::assistant_text(text.clone()),
        text,
    })
}

fn should_auto_continue_after_tool_response(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return true;
    }

    let lower = trimmed.to_lowercase();
    !is_waiting_on_user(trimmed, &lower) && !has_explicit_completion_signal(&lower)
}

fn is_waiting_on_user(text: &str, lower: &str) -> bool {
    if text.contains('?') {
        return true;
    }

    [
        "please confirm",
        "please log",
        "please sign",
        "please complete",
        "please review",
        "please submit",
        "let me know",
        "i need you",
        "waiting for",
        "log in",
        "login",
        "sign in",
        "handoff",
        "manually",
        "authentication",
        "authenticate",
        "verification",
        "verify your",
        "2fa",
        "bank",
    ]
    .iter()
    .any(|phrase| lower.contains(phrase))
}

fn has_explicit_completion_signal(lower: &str) -> bool {
    let normalized =
        lower.trim_start_matches(|ch: char| ch.is_ascii_punctuation() || ch.is_whitespace());

    let completed = [
        "all set",
        "you're all set",
        "completed",
        "finished",
        "task is complete",
        "i've completed",
        "i have completed",
        "i've finished",
        "i have finished",
        "final answer",
    ]
    .iter()
    .any(|phrase| lower.contains(phrase));

    let failed = [
        "i couldn't",
        "i could not",
        "i can't",
        "i cannot",
        "i was unable",
        "unable to",
    ]
    .iter()
    .any(|phrase| lower.contains(phrase));

    completed || failed || normalized.starts_with("done")
}

fn parse_browser_arguments(arguments: &str) -> Result<Vec<Action>> {
    let parsed: BrowserToolArguments =
        serde_json::from_str(arguments).context("invalid browser tool arguments")?;
    Ok(parsed.actions)
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
        "none loaded; payment/address vault actions are unavailable until the user configures 1Password".to_owned()
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
        "You are Emissary, a minimal personal assistant with one tool: browser.\n\
         Use the browser tool to carry out web tasks in a persistent Chrome session. \
         Send short ordered batches of JSON actions. Each tool result includes `title` and \
         `pageText` plus `elements` refs (visible controls after the batch finishes, including accessible iframe contents). Use `html` only when you need markup.\n\
         Discovery:\n\
         - Use the persistent browser session for discovery, reading, and interaction. \
         Navigate to search engines or known sites when the user needs information from the web.\n\
         Selectors:\n\
         - Prefer observe -> clickRef/typeRef. Do not invent CSS selectors when an element ref is available.\n\
         - Element refs may include a `frame` label for iframe contents; use the same clickRef/typeRef/fillPaymentRefs actions with those refs.\n\
         - click/type/wait use standard CSS (document.querySelector). No :contains(), :text(), or Playwright syntax.\n\
         - Prefer simple selectors (#id, [aria-label=\"...\"], input[name=\"...\"]).\n\
         - When you only know visible label text, use clickText instead of click.\n\
         - XPath works in click/wait/type when the selector starts with //.\n\
         Errors:\n\
         - If an action fails (missing element, invalid selector, etc.), the tool returns status error with pageText so you can adjust and retry.\n\
         - If pageState is bot_challenge or mode is blocked, stop retrying automation. Ask whether to use another site, continue manually in a normal browser, or retry after the user has cleared Cloudflare outside the harness.\n\
         Credentials:\n\
         - Never put card numbers, CVV, shipping address, billing address, email, or phone in tool arguments.\n\
         - Use fillPayment with a profile key when checkout needs card details.\n\
         - Use fillAddress with kind `shipping` or `billing` when checkout needs address/contact fields. Loaded profiles: {profiles}.\n\
         - If no payment profile is loaded and a task needs checkout credentials, ask the user to run setup before continuing.\n\
         Payment:\n\
         - Never put card numbers, CVV, or any payment secret values in tool arguments.\n\
         - Review the basket/order summary before entering payment when checkout flow allows it.\n\
         - Once payment/card fields are visible, do not call screenshot, html, text, or observe to inspect them. \
         Use autoFillPaymentAndContinue with the chosen profile so the runtime fills the vault-backed fields \
         and clicks only a guarded non-final continue/next/checkout control without model-selected fields or buttons.\n\
         - For checkout fields, first use observe, then map visible input refs to vault credential IDs with fillPaymentRefs. \
         Send IDs like `default:card_number`, `default:exp`, `default:exp_month`, `default:exp_year`, `default:cvc`, `default:name`, and `default:postal_code`; never send the values. Loaded profiles: {profiles}.\n\
         - Use fillPayment/fillPaymentRefs only as fallbacks when autoFillPaymentAndContinue is not appropriate.\n\
         Images:\n\
         - When the user asks about appearance, product photos, or wants to buy a specific item, use screenshot before checkout when helpful. \
         Prefer a selector for the product image/card when available; otherwise capture the visible page.\n\
         - Screenshots are shown to the user separately and omitted from model context. Do not reproduce base64.\n\
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

    if let Some(note) = body
        .pointer("/review/review_note")
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
    {
        println!("\n[review] {note}");
    }

    if let Some(screenshot) = body
        .pointer("/review/screenshot_base64")
        .and_then(Value::as_str)
        .filter(|data| !data.is_empty())
    {
        show_browser_image(
            "Order review screenshot",
            "review-latest.png",
            screenshot,
            runtime_dir,
        )?;
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
                "\nSites like Uber Eats often block automation behind Cloudflare. Complete the check in the handoff browser, then tell Emissary you are done."
            );
        } else {
            println!("\nComplete authentication in the browser, then tell Emissary you are done.");
        }
    } else {
        println!(
            "\nReview the order, submit via the handoff browser if it looks right, then say you are done."
        );
    }

    Ok(())
}

struct BrowserImage {
    label: String,
    screenshot_base64: String,
    order_summary: Option<String>,
    note: Option<String>,
}

fn show_browser_images_to_user(body: &Value, runtime_dir: &PathBuf) -> Result<()> {
    let mut images = Vec::new();
    collect_browser_images(body, &mut images);

    for (index, image) in images.iter().enumerate() {
        if let Some(summary) = image
            .order_summary
            .as_deref()
            .filter(|text| !text.trim().is_empty())
        {
            println!("\n--- Order review ---\n{summary}");
        }
        if let Some(note) = image.note.as_deref().filter(|text| !text.trim().is_empty()) {
            println!("\n[image] {note}");
        }

        let filename = if image.label == "Order review screenshot" {
            "review-latest.png".to_owned()
        } else if images.len() == 1 {
            "screenshot-latest.png".to_owned()
        } else {
            format!("screenshot-{}.png", index + 1)
        };
        show_browser_image(
            &image.label,
            &filename,
            &image.screenshot_base64,
            runtime_dir,
        )?;
    }

    Ok(())
}

fn collect_browser_images(value: &Value, images: &mut Vec<BrowserImage>) {
    match value {
        Value::Object(map) => {
            if let Some(screenshot) = map
                .get("screenshot_base64")
                .and_then(Value::as_str)
                .filter(|data| !data.is_empty())
            {
                images.push(BrowserImage {
                    label: browser_image_label(map),
                    screenshot_base64: screenshot.to_owned(),
                    order_summary: map
                        .get("order_summary")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    note: map
                        .get("review_note")
                        .or_else(|| map.get("screenshot_note"))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                });
                return;
            }

            for nested in map.values() {
                collect_browser_images(nested, images);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_browser_images(item, images);
            }
        }
        _ => {}
    }
}

fn browser_image_label(map: &Map<String, Value>) -> String {
    if map.contains_key("review_scope") {
        return "Order review screenshot".to_owned();
    }

    match map.get("screenshot_scope").and_then(Value::as_str) {
        Some("selected_element") => "Selected page screenshot".to_owned(),
        Some("page_viewport") => "Page screenshot".to_owned(),
        Some(scope) => format!("{scope} screenshot"),
        None => "Browser screenshot".to_owned(),
    }
}

fn show_browser_image(
    label: &str,
    filename: &str,
    screenshot_base64: &str,
    runtime_dir: &PathBuf,
) -> Result<()> {
    println!("\n--- {label} ---");
    let path = image_display::save_base64_png(runtime_dir, filename, screenshot_base64)?;
    match image_display::render_inline(&path) {
        InlineImageResult::Rendered => {}
        InlineImageResult::Skipped => {}
        InlineImageResult::Failed(error) => eprintln!("[image] inline render failed: {error}"),
    }
    println!("Image saved: {}", path.display());
    Ok(())
}

fn show_browser_error_to_user(body: &Value) {
    if let Some(error) = body.get("error").and_then(Value::as_str) {
        eprintln!("\n[browser] {error}");
    }
}

fn format_tool_result_for_model(body: &Value) -> String {
    let mut sanitized = body.clone();
    strip_screenshot_data(&mut sanitized);
    let mut out = Map::new();
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

#[cfg(test)]
mod tests {
    use super::{
        ChatCompletionResponse, parse_browser_arguments, should_auto_continue_after_tool_response,
    };
    use crate::actions::Action;

    #[test]
    fn parses_text_only_llm_response() {
        let response: ChatCompletionResponse = serde_json::from_str(
            r#"{
                "choices": [
                    { "message": { "role": "assistant", "content": "hello" } }
                ]
            }"#,
        )
        .unwrap();

        let message = &response.choices[0].message;
        assert_eq!(message.role, "assistant");
        assert_eq!(message.content.as_deref(), Some("hello"));
        assert!(message.tool_calls.is_none());
    }

    #[test]
    fn parses_tool_call_llm_response() {
        let response: ChatCompletionResponse = serde_json::from_str(
            r#"{
                "choices": [
                    {
                        "message": {
                            "role": "assistant",
                            "tool_calls": [
                                {
                                    "id": "call_1",
                                    "type": "function",
                                    "function": {
                                        "name": "browser",
                                        "arguments": "{\"actions\":[{\"op\":\"observe\"}]}"
                                    }
                                }
                            ]
                        }
                    }
                ]
            }"#,
        )
        .unwrap();

        let call = &response.choices[0].message.tool_calls.as_ref().unwrap()[0];
        assert_eq!(call.id, "call_1");
        assert_eq!(call.function.name, "browser");
    }

    #[test]
    fn parses_browser_tool_arguments() {
        let actions = parse_browser_arguments(
            r#"{"actions":[{"op":"navigate","url":"https://example.com"}]}"#,
        )
        .unwrap();
        assert!(matches!(actions[0], Action::Navigate { .. }));
    }

    #[test]
    fn auto_continues_empty_post_tool_response() {
        assert!(should_auto_continue_after_tool_response("   "));
    }

    #[test]
    fn auto_continues_ambiguous_post_tool_progress() {
        assert!(should_auto_continue_after_tool_response(
            "The page has loaded and I can see the search results."
        ));
        assert!(should_auto_continue_after_tool_response(
            "I found the menu and can see several matching options."
        ));
    }

    #[test]
    fn does_not_auto_continue_when_waiting_for_user() {
        assert!(!should_auto_continue_after_tool_response(
            "Please review the order and submit it in the handoff browser."
        ));
        assert!(!should_auto_continue_after_tool_response(
            "Which restaurant should I use?"
        ));
    }

    #[test]
    fn does_not_auto_continue_clear_final_response() {
        assert!(!should_auto_continue_after_tool_response(
            "Done - I found the restaurant's current hours."
        ));
        assert!(!should_auto_continue_after_tool_response(
            "Final answer: the restaurant closes at 9 PM."
        ));
        assert!(!should_auto_continue_after_tool_response(
            "I was unable to find that item in stock."
        ));
    }
}
