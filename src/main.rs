use anyhow::{Context, Result, anyhow, bail};
use headless_chrome::{Browser, LaunchOptionsBuilder, Tab};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    ffi::OsStr,
    fs,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::Arc,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

const PROFILE_DIR: &str = "automation-profile";
const SENSITIVE_CLICK: &str = "sensitive click blocked:";

fn main() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.first().map(String::as_str) == Some("serve") {
        return serve();
    }

    let task = args.join(" ");
    if task.trim().is_empty() {
        bail!("{}", usage());
    }

    let browser = launch_browser(headless(), None)?;
    let tab = browser.new_tab()?;
    let last = run_task(&tab, &task)?;

    println!("{}", serde_json::to_string_pretty(&last)?);
    Ok(())
}

fn serve() -> Result<()> {
    let display = display_num();
    let vnc_port = env_u16("VNC_PORT", 5900);
    let novnc_port = env_u16("NOVNC_PORT", 6080);
    let api_addr = env_string("API_ADDR", "127.0.0.1:8787");
    let screen = screen_spec();
    let display_name = format!(":{display}");

    let xvfb = ChildGuard::spawn(
        "Xvfb",
        "Xvfb",
        [
            display_name.as_str(),
            "-screen",
            "0",
            screen.as_str(),
            "-nolisten",
            "tcp",
        ],
    )
    .context("failed to start Xvfb; install it with `sudo apt install xvfb`")?;
    thread::sleep(Duration::from_millis(500));

    // SAFETY: this setup is single-threaded until Chrome is launched, so no other
    // thread can concurrently read or mutate the process environment.
    unsafe {
        std::env::set_var("DISPLAY", &display_name);
        std::env::remove_var("WAYLAND_DISPLAY");
        std::env::set_var("GDK_BACKEND", "x11");
        std::env::set_var("XDG_SESSION_TYPE", "x11");
    }

    let x11vnc = ChildGuard::spawn(
        "x11vnc",
        "x11vnc",
        [
            "-display",
            display_name.as_str(),
            "-localhost",
            "-nopw",
            "-forever",
            "-shared",
            "-rfbport",
            &vnc_port.to_string(),
        ],
    )
    .context("failed to start x11vnc; install it with `sudo apt install x11vnc`")?;
    thread::sleep(Duration::from_millis(500));

    let novnc_web = env_string("NOVNC_WEB", "/usr/share/novnc");
    let novnc = ChildGuard::spawn(
        "websockify",
        env_string("WEBSOCKIFY", "websockify"),
        [
            format!("--web={novnc_web}"),
            format!("127.0.0.1:{novnc_port}"),
            format!("127.0.0.1:{vnc_port}"),
        ],
    )
    .context(
        "failed to start websockify/noVNC; install them with `sudo apt install novnc websockify`",
    )?;

    let browser = launch_browser(false, screen_dimensions(&screen))?;
    let tab = browser.new_tab()?;
    let handoff_url =
        format!("http://127.0.0.1:{novnc_port}/vnc.html?autoconnect=true&resize=scale");
    let mut runtime = Runtime {
        _xvfb: xvfb,
        _x11vnc: x11vnc,
        _novnc: novnc,
        _browser: browser,
        tab,
        paused: false,
        session_id: session_id(),
        handoff_url,
    };

    let server = Server::http(&api_addr).map_err(|error| anyhow!("{error}"))?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "status": "ready",
            "api": format!("http://{api_addr}"),
            "handoff_url": runtime.handoff_url,
            "session_id": runtime.session_id,
            "screen": screen,
        }))?
    );

    for request in server.incoming_requests() {
        if let Err(error) = handle_request(request, &mut runtime) {
            eprintln!("{error:#}");
        }
    }

    Ok(())
}

fn launch_browser(headless: bool, window_size: Option<(u32, u32)>) -> Result<Browser> {
    let mut options = LaunchOptionsBuilder::default();
    options
        .headless(headless)
        .user_data_dir(Some(automation_profile()?));
    if let Some(window_size) = window_size {
        options.window_size(Some(window_size));
    }
    if let Ok(path) = std::env::var("CHROME") {
        options.path(Some(PathBuf::from(path)));
    }
    if !headless {
        options.args(vec![
            OsStr::new("--ozone-platform=x11"),
            OsStr::new("--window-position=0,0"),
        ]);
    }

    Browser::new(options.build().map_err(|error| anyhow!("{error}"))?)
}

fn run_task(tab: &Arc<Tab>, task: &str) -> Result<Value> {
    let mut last = Value::Null;

    for step in task
        .split(';')
        .map(str::trim)
        .filter(|step| !step.is_empty())
    {
        let words = shell_words::split(step).with_context(|| format!("bad step: {step}"))?;
        let command = words[0].as_str();

        last = match command {
            "goto" | "open" => {
                let url = arg(&words, 1, step)?;
                tab.navigate_to(url)?;
                tab.wait_until_navigated()?;
                json!({ "url": url, "title": tab.get_title()? })
            }
            "click" => {
                let css = arg(&words, 1, step)?;
                let element = tab.wait_for_element(css)?;
                block_sensitive_click(tab.as_ref(), css)?;
                element.click()?;
                json!({ "clicked": css })
            }
            "type" => {
                let css = arg(&words, 1, step)?;
                let text = words
                    .get(2..)
                    .filter(|parts| !parts.is_empty())
                    .map(|parts| parts.join(" "));
                let text = text.as_deref().context("type expects: type <css> <text>")?;
                tab.wait_for_element(css)?.click()?;
                tab.type_str(text)?;
                json!({ "typed": text, "into": css })
            }
            "press" => {
                let key = arg(&words, 1, step)?;
                tab.press_key(key)?;
                json!({ "pressed": key })
            }
            "wait" => {
                let css = arg(&words, 1, step)?;
                tab.wait_for_element(css)?;
                json!({ "found": css })
            }
            "title" => json!(tab.get_title()?),
            "text" => selector_value(
                tab.as_ref(),
                words.get(1).map_or("body", String::as_str),
                "innerText",
            )?,
            "html" => selector_value(
                tab.as_ref(),
                words.get(1).map_or("body", String::as_str),
                "innerHTML",
            )?,
            "eval" => {
                let js = step[command.len()..].trim();
                if js.is_empty() {
                    bail!("eval expects JavaScript");
                }
                tab.evaluate(js, false)?.value.unwrap_or(Value::Null)
            }
            _ => bail!("unknown command `{command}`\n\n{}", usage()),
        };
    }

    Ok(last)
}

fn handle_request(mut request: Request, runtime: &mut Runtime) -> Result<()> {
    let method = request.method().clone();
    let path = request.url().split('?').next().unwrap_or("/").to_owned();

    let response = match (method, path.as_str()) {
        (Method::Get, "/status") | (Method::Get, "/") => {
            json_response(StatusCode(200), runtime.status())
        }
        (Method::Post, "/handoff") => {
            runtime.paused = true;
            json_response(StatusCode(200), runtime.handoff("manual handoff requested"))
        }
        (Method::Post, "/resume") => {
            runtime.paused = false;
            json_response(
                StatusCode(200),
                json!({ "status": "ready", "session_id": runtime.session_id }),
            )
        }
        (Method::Post, "/task") => {
            if runtime.paused {
                json_response(StatusCode(409), runtime.handoff("human handoff is active"))
            } else {
                let task = task_from_request(&mut request)?;
                match run_task(&runtime.tab, &task) {
                    Ok(result) => {
                        json_response(StatusCode(200), json!({ "status": "ok", "result": result }))
                    }
                    Err(error) if error.to_string().starts_with(SENSITIVE_CLICK) => {
                        runtime.paused = true;
                        json_response(StatusCode(409), runtime.handoff(error.to_string()))
                    }
                    Err(error) => json_response(
                        StatusCode(500),
                        json!({ "status": "error", "error": error.to_string() }),
                    ),
                }
            }
        }
        _ => json_response(
            StatusCode(404),
            json!({ "status": "error", "error": "not found" }),
        ),
    };

    request.respond(response)?;
    Ok(())
}

#[derive(Deserialize)]
struct TaskRequest {
    task: String,
}

#[derive(Serialize)]
struct RuntimeStatus<'a> {
    status: &'a str,
    session_id: &'a str,
    handoff_url: &'a str,
}

struct Runtime {
    _xvfb: ChildGuard,
    _x11vnc: ChildGuard,
    _novnc: ChildGuard,
    _browser: Browser,
    tab: Arc<Tab>,
    paused: bool,
    session_id: String,
    handoff_url: String,
}

impl Runtime {
    fn status(&self) -> Value {
        json!(RuntimeStatus {
            status: if self.paused { "needs_human" } else { "ready" },
            session_id: &self.session_id,
            handoff_url: &self.handoff_url,
        })
    }

    fn handoff(&self, reason: impl Into<String>) -> Value {
        json!({
            "status": "needs_human",
            "reason": reason.into(),
            "session_id": self.session_id,
            "handoff_url": self.handoff_url,
            "resume": "POST /resume",
        })
    }
}

struct ChildGuard {
    name: &'static str,
    child: Child,
}

impl ChildGuard {
    fn spawn<I, S>(
        name: &'static str,
        command: impl AsRef<std::ffi::OsStr>,
        args: I,
    ) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let child = Command::new(command)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let mut guard = Self { name, child };
        thread::sleep(Duration::from_millis(100));
        if let Some(status) = guard.child.try_wait()? {
            bail!("{name} exited during startup with {status}");
        }
        Ok(guard)
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        eprintln!("stopped {}", self.name);
    }
}

fn arg<'a>(words: &'a [String], index: usize, step: &str) -> Result<&'a str> {
    words
        .get(index)
        .map(String::as_str)
        .with_context(|| format!("missing argument in step: {step}"))
}

fn selector_value(tab: &Tab, css: &str, property: &str) -> Result<Value> {
    let css = serde_json::to_string(css)?;
    let js = format!(
        r#"(() => {{
            const el = document.querySelector({css});
            return el ? el.{property} : null;
        }})()"#
    );
    Ok(tab.evaluate(&js, false)?.value.unwrap_or(Value::Null))
}

fn block_sensitive_click(tab: &Tab, css: &str) -> Result<()> {
    let details = clickable_details(tab, css)?;
    if is_sensitive_click(&details) {
        bail!("{SENSITIVE_CLICK} {details}");
    }

    Ok(())
}

fn is_sensitive_click(details: &str) -> bool {
    let lower = details.to_lowercase();
    [
        "checkout",
        "place order",
        "pay",
        "payment",
        "confirm purchase",
        "complete purchase",
        "complete order",
        "submit order",
        "buy now",
        "order now",
        "purchase",
        "subscribe",
        "proceed to payment",
    ]
    .iter()
    .any(|word| lower.contains(word))
}

fn clickable_details(tab: &Tab, css: &str) -> Result<String> {
    let css = serde_json::to_string(css)?;
    let js = format!(
        r#"(() => {{
            const el = document.querySelector({css});
            if (!el) return "";
            const form = el.closest("form");
            const bits = [
                el.innerText,
                el.textContent,
                el.getAttribute("aria-label"),
                el.getAttribute("title"),
                el.getAttribute("value"),
                el.id,
                el.name,
                form && form.innerText,
                form && form.getAttribute("aria-label")
            ];
            return bits.filter(Boolean).join("\n").slice(0, 2000);
        }})()"#
    );
    Ok(tab
        .evaluate(&js, false)?
        .value
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default())
}

fn automation_profile() -> Result<PathBuf> {
    let profile = std::env::current_dir()?.join(PROFILE_DIR);
    fs::create_dir_all(&profile)?;
    Ok(profile)
}

fn headless() -> bool {
    !matches!(
        std::env::var("HEADLESS").as_deref(),
        Ok("0" | "false" | "False" | "FALSE")
    )
}

fn task_from_request(request: &mut Request) -> Result<String> {
    let mut body = String::new();
    request.as_reader().read_to_string(&mut body)?;
    if body.trim().is_empty() {
        bail!(
            "POST /task expects a JSON body like {{\"task\":\"goto https://example.com; title\"}}"
        );
    }

    Ok(serde_json::from_str::<TaskRequest>(&body)
        .map(|request| request.task)
        .unwrap_or(body))
}

fn json_response(status: StatusCode, body: Value) -> Response<std::io::Cursor<Vec<u8>>> {
    let body = serde_json::to_vec_pretty(&body).unwrap_or_else(|_| b"{}".to_vec());
    Response::from_data(body)
        .with_status_code(status)
        .with_header(Header::from_bytes("Content-Type", "application/json").unwrap())
}

fn env_string(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn env_u16(name: &str, default: u16) -> u16 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn display_num() -> u16 {
    std::env::var("DISPLAY_NUM")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| {
            (99..200)
                .find(|display| display_is_free(*display))
                .unwrap_or(99)
        })
}

fn display_is_free(display: u16) -> bool {
    !PathBuf::from(format!("/tmp/.X{display}-lock")).exists()
        && !PathBuf::from(format!("/tmp/.X11-unix/X{display}")).exists()
}

fn screen_spec() -> String {
    std::env::var("SCREEN")
        .ok()
        .filter(|screen| !screen.trim().is_empty())
        .unwrap_or_else(|| detected_screen().unwrap_or_else(|| "1280x720x24".to_owned()))
}

fn screen_dimensions(screen: &str) -> Option<(u32, u32)> {
    let mut parts = screen.split('x');
    let width = parts.next()?.parse().ok()?;
    let height = parts.next()?.parse().ok()?;
    Some((width, height))
}

fn detected_screen() -> Option<String> {
    let output = Command::new("xdpyinfo").output().ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let dimensions = stdout.lines().find_map(|line| {
        line.trim()
            .strip_prefix("dimensions:")
            .and_then(|rest| rest.split_whitespace().next())
    })?;

    Some(format!("{dimensions}x24"))
}

fn session_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("{:x}-{nanos:x}", std::process::id())
}

fn usage() -> &'static str {
    r#"Usage:
  cargo run -- serve
  cargo run -- 'goto https://example.com; title'
  cargo run -- 'goto https://example.com; text body'
  cargo run -- 'goto https://example.com; click "a"; text body'
  cargo run -- 'goto https://example.com; eval document.title'

Commands:
  goto|open <url>      navigate to a page
  click <css>          click the first matching element
  type <css> <text>    focus an element and type text
  press <key>          press a key, for example Enter
  wait <css>           wait for an element
  title                return the page title
  text [css]           return innerText, defaulting to body
  html [css]           return innerHTML, defaulting to body
  eval <javascript>    evaluate JavaScript and return its JSON value"#
}

#[cfg(test)]
mod tests {
    use super::is_sensitive_click;

    #[test]
    fn blocks_purchase_actions() {
        assert!(is_sensitive_click("Place order"));
        assert!(is_sensitive_click("Proceed to payment"));
        assert!(is_sensitive_click("Confirm purchase"));
    }

    #[test]
    fn allows_cart_building_actions() {
        assert!(!is_sensitive_click("Add to basket"));
        assert!(!is_sensitive_click("Choose delivery time"));
        assert!(!is_sensitive_click("View restaurant menu"));
    }
}
