use crate::actions::{Action, RunContext, RunOutcome, RunRequest, run_actions, tool_schema};
use crate::payment::PaymentVault;
use crate::review::handoff_payload;
use anyhow::{Context, Result, anyhow, bail};
use headless_chrome::{Browser, LaunchOptionsBuilder, Tab};
use serde_json::{Value, json};
use std::{
    ffi::OsStr,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Arc,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

const PROFILE_DIR: &str = "automation-profile";
const RUNTIME_DIR: &str = ".agent-runtime";
const LOCK_FILE: &str = "daemon.lock";

pub struct ManagedDaemon {
    runtime: Option<Runtime>,
    lock_path: PathBuf,
}

pub struct BrowserResponse {
    status: u16,
    kind: BrowserResponseKind,
    body: Value,
}

enum BrowserResponseKind {
    Success,
    Error,
    NeedsHuman,
}

impl BrowserResponse {
    fn success(body: Value) -> Self {
        Self {
            status: 200,
            kind: BrowserResponseKind::Success,
            body,
        }
    }

    fn error(body: Value) -> Self {
        Self {
            status: 200,
            kind: BrowserResponseKind::Error,
            body,
        }
    }

    fn needs_human(body: Value) -> Self {
        Self {
            status: 409,
            kind: BrowserResponseKind::NeedsHuman,
            body,
        }
    }

    pub fn status(&self) -> u16 {
        self.status
    }

    pub fn body(&self) -> &Value {
        &self.body
    }

    pub fn into_body(self) -> Value {
        self.body
    }

    pub fn is_error(&self) -> bool {
        matches!(self.kind, BrowserResponseKind::Error)
    }

    pub fn needs_human_handoff(&self) -> bool {
        matches!(self.kind, BrowserResponseKind::NeedsHuman)
    }
}

impl ManagedDaemon {
    pub fn start() -> Result<Self> {
        let runtime_dir = runtime_dir()?;
        fs::create_dir_all(&runtime_dir)?;
        reclaim_stale_daemon(&runtime_dir)?;

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

        // SAFETY: daemon startup is single-threaded until Chrome launches.
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

        let payment = PaymentVault::load()?;
        let browser = launch_browser(false, screen_dimensions(&screen))
            .context("failed to launch headed Chrome for chat")?;
        let tab = browser.new_tab().context("failed to open browser tab")?;
        tab.set_default_timeout(action_timeout());
        let handoff_url =
            format!("http://127.0.0.1:{novnc_port}/vnc.html?autoconnect=true&resize=scale");

        let lock_path = runtime_dir.join(LOCK_FILE);
        write_lock(
            &lock_path,
            &DaemonLock {
                owner_pid: std::process::id(),
                child_pids: vec![xvfb.pid(), x11vnc.pid(), novnc.pid()],
                api_addr: api_addr.clone(),
                started_at: session_id(),
            },
        )?;

        let runtime = Runtime {
            _xvfb: xvfb,
            _x11vnc: x11vnc,
            _novnc: novnc,
            _browser: browser,
            tab,
            payment,
            paused: false,
            session_id: session_id(),
            handoff_url,
            api_addr,
            screen,
        };

        Ok(Self {
            runtime: Some(runtime),
            lock_path,
        })
    }

    pub fn stop_stale() -> Result<()> {
        let runtime_dir = runtime_dir()?;
        let had_lock = runtime_dir.join(LOCK_FILE).exists();
        reclaim_stale_daemon(&runtime_dir)?;
        if runtime_dir.join(LOCK_FILE).exists() {
            bail!("Emissary daemon is still running");
        }
        if had_lock {
            println!("stale Emissary daemon cleaned up");
        } else {
            println!("no Emissary daemon running");
        }
        Ok(())
    }

    pub fn api_addr(&self) -> &str {
        &self.runtime().api_addr
    }

    pub fn status(&self) -> Value {
        self.runtime().status()
    }

    pub fn schema(&self) -> Value {
        tool_schema()
    }

    pub fn payment_keys(&self) -> Vec<String> {
        self.runtime().payment.keys()
    }

    pub fn run(&mut self, actions: Vec<Action>) -> Result<BrowserResponse> {
        let runtime = self
            .runtime
            .as_mut()
            .context("Emissary daemon is not running")?;
        let run_request = RunRequest { actions };
        if runtime.paused
            && !run_request
                .actions
                .iter()
                .all(|action| matches!(action, Action::Resume))
        {
            return Ok(BrowserResponse::needs_human(json!(handoff_payload(
                &runtime.tab,
                &runtime.session_id,
                &runtime.handoff_url,
                "human handoff is active",
            ))));
        }

        let mut context = RunContext {
            tab: &runtime.tab,
            payment: &runtime.payment,
            paused: Some(&mut runtime.paused),
            session_id: &runtime.session_id,
            handoff_url: &runtime.handoff_url,
        };

        match run_actions(&mut context, &run_request)? {
            RunOutcome::Success(success) => Ok(BrowserResponse::success(json!(success))),
            RunOutcome::Failed(failure) => Ok(BrowserResponse::error(json!(failure))),
            RunOutcome::NeedsHuman { handoff, .. } => {
                Ok(BrowserResponse::needs_human(json!(handoff)))
            }
        }
    }

    pub fn handoff(&mut self, reason: &str) -> Value {
        let runtime = self.runtime.as_mut().expect("daemon running");
        runtime.paused = true;
        json!(handoff_payload(
            &runtime.tab,
            &runtime.session_id,
            &runtime.handoff_url,
            reason,
        ))
    }

    pub fn resume(&mut self) {
        if let Some(runtime) = self.runtime.as_mut() {
            runtime.paused = false;
        }
    }

    pub fn shutdown(&mut self) {
        let _ = fs::remove_file(&self.lock_path);
        self.runtime.take();
    }

    fn runtime(&self) -> &Runtime {
        self.runtime
            .as_ref()
            .expect("Emissary daemon is not running")
    }
}

impl Drop for ManagedDaemon {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct DaemonLock {
    owner_pid: u32,
    child_pids: Vec<u32>,
    api_addr: String,
    started_at: String,
}

fn reclaim_stale_daemon(runtime_dir: &Path) -> Result<()> {
    let lock_path = runtime_dir.join(LOCK_FILE);
    if !lock_path.exists() {
        return Ok(());
    }

    let lock = read_lock(&lock_path)?;
    if process_alive(lock.owner_pid) {
        bail!(
            "Emissary daemon already running (pid {} on {})",
            lock.owner_pid,
            lock.api_addr
        );
    }

    eprintln!(
        "cleaning stale Emissary daemon (pid {}, started {})",
        lock.owner_pid, lock.started_at
    );
    for pid in lock.child_pids {
        kill_pid(pid);
    }
    kill_pid(lock.owner_pid);
    fs::remove_file(&lock_path)?;
    Ok(())
}

fn read_lock(path: &Path) -> Result<DaemonLock> {
    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(serde_json::from_str(&raw)?)
}

fn write_lock(path: &Path, lock: &DaemonLock) -> Result<()> {
    fs::write(path, serde_json::to_string_pretty(lock)?)?;
    Ok(())
}

fn process_alive(pid: u32) -> bool {
    PathBuf::from(format!("/proc/{pid}")).exists()
}

fn kill_pid(pid: u32) {
    if process_alive(pid) {
        let _ = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        thread::sleep(Duration::from_millis(200));
        if process_alive(pid) {
            let _ = Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

pub fn install_shutdown_handler(daemon: Arc<std::sync::Mutex<ManagedDaemon>>) -> Result<()> {
    ctrlc::set_handler(move || {
        if let Ok(mut guard) = daemon.lock() {
            guard.shutdown();
        }
        let _ = writeln!(io::stderr(), "\nEmissary stopped.");
        std::process::exit(0);
    })?;
    Ok(())
}

struct Runtime {
    _xvfb: ChildGuard,
    _x11vnc: ChildGuard,
    _novnc: ChildGuard,
    _browser: Browser,
    tab: Arc<Tab>,
    payment: PaymentVault,
    paused: bool,
    session_id: String,
    handoff_url: String,
    api_addr: String,
    screen: String,
}

impl Runtime {
    fn status(&self) -> Value {
        json!({
            "status": if self.paused { "needs_human" } else { "ready" },
            "session_id": self.session_id,
            "handoff_url": self.handoff_url,
            "api_addr": self.api_addr,
            "screen": self.screen,
            "tool": "/schema",
        })
    }
}

struct ChildGuard {
    name: &'static str,
    child: Child,
}

impl ChildGuard {
    fn spawn<I, S>(name: &'static str, command: impl AsRef<OsStr>, args: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let child = Command::new(command)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("failed to spawn {name}"))?;
        let mut guard = Self { name, child };
        thread::sleep(Duration::from_millis(100));
        if let Some(status) = guard.child.try_wait()? {
            bail!("{name} exited during startup with {status}");
        }
        Ok(guard)
    }

    fn pid(&self) -> u32 {
        self.child.id()
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

pub fn launch_browser(headless: bool, window_size: Option<(u32, u32)>) -> Result<Browser> {
    launch_browser_with_profile(headless, window_size, automation_profile()?)
}

/// One-shot `run` sessions use a separate profile so they do not block chat's
/// persistent `automation-profile/` when both would otherwise share Chrome's lock.
pub fn launch_browser_ephemeral(
    headless: bool,
    window_size: Option<(u32, u32)>,
) -> Result<Browser> {
    let profile = std::env::current_dir()?.join("automation-profile-run");
    fs::create_dir_all(&profile)?;
    launch_browser_with_profile(headless, window_size, profile)
}

fn launch_browser_with_profile(
    headless: bool,
    window_size: Option<(u32, u32)>,
    profile: PathBuf,
) -> Result<Browser> {
    let chrome = chrome_path()?;
    let chrome_display = chrome_path_display(&chrome);
    let mut options = LaunchOptionsBuilder::default();
    options
        .headless(headless)
        .idle_browser_timeout(idle_browser_timeout())
        .user_data_dir(Some(profile))
        .path(Some(chrome));
    if let Some(window_size) = window_size {
        options.window_size(Some(window_size));
    }
    if !headless {
        options.args(vec![
            OsStr::new("--ozone-platform=x11"),
            OsStr::new("--window-position=0,0"),
            OsStr::new("--disable-blink-features=AutomationControlled"),
        ]);
    } else {
        options.args(vec![OsStr::new(
            "--disable-blink-features=AutomationControlled",
        )]);
    }

    Browser::new(options.build().map_err(|error| anyhow!("{error}"))?)
        .with_context(|| format!("failed to start Chrome at {chrome_display}"))
}

fn idle_browser_timeout() -> Duration {
    std::env::var("IDLE_BROWSER_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(3600))
}

fn action_timeout() -> Duration {
    std::env::var("BROWSER_ACTION_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(30))
}

fn chrome_path() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("CHROME") {
        let path = PathBuf::from(&path);
        if path.is_file() {
            return Ok(path);
        }
        bail!(
            "CHROME is set to {} but that file does not exist; unset it for auto-detection or set CHROME to your browser binary (e.g. /snap/bin/chromium)",
            path.display()
        );
    }

    detect_chrome().context(
        "could not find Chrome or Chromium; install chromium or set CHROME to the browser binary",
    )
}

fn detect_chrome() -> Result<PathBuf> {
    for name in [
        "chromium",
        "chromium-browser",
        "google-chrome",
        "google-chrome-stable",
    ] {
        if let Some(path) = which_command(name) {
            return Ok(path);
        }
    }

    for path in [
        "/snap/bin/chromium",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
    ] {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
    }

    bail!("no Chrome or Chromium binary found on PATH or common install locations")
}

fn which_command(name: &str) -> Option<PathBuf> {
    Command::new("which")
        .arg(name)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|stdout| stdout.trim().to_owned())
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
}

fn chrome_path_display(path: &Path) -> String {
    path.display().to_string()
}

pub fn parse_run_request(body: &str) -> Result<RunRequest> {
    if body.trim().is_empty() {
        bail!(
            "expected JSON like {{\"actions\":[{{\"op\":\"navigate\",\"url\":\"https://example.com\"}}]}}"
        );
    }

    serde_json::from_str::<RunRequest>(body).context("invalid run request JSON")
}

fn automation_profile() -> Result<PathBuf> {
    let profile = std::env::current_dir()?.join(PROFILE_DIR);
    fs::create_dir_all(&profile)?;
    Ok(profile)
}

pub fn runtime_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("EMISSARY_RUNTIME_DIR") {
        return Ok(PathBuf::from(dir));
    }
    Ok(std::env::current_dir()?.join(RUNTIME_DIR))
}

pub fn headless() -> bool {
    !matches!(
        std::env::var("HEADLESS").as_deref(),
        Ok("0" | "false" | "False" | "FALSE")
    )
}

pub fn session_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("{:x}-{nanos:x}", std::process::id())
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

#[allow(dead_code)]
pub fn serve_blocking() -> Result<()> {
    let mut daemon = ManagedDaemon::start()?;
    let ready = daemon.status();
    println!("{}", serde_json::to_string_pretty(&ready)?);

    let api_addr = daemon.api_addr().to_owned();
    let server = Server::http(&api_addr).map_err(|error| anyhow!("{error}"))?;

    for request in server.incoming_requests() {
        if let Err(error) = handle_request(request, &mut daemon) {
            eprintln!("{error:#}");
        }
    }

    Ok(())
}

fn handle_request(mut request: Request, daemon: &mut ManagedDaemon) -> Result<()> {
    let method = request.method().clone();
    let path = request.url().split('?').next().unwrap_or("/").to_owned();

    let response = match (method, path.as_str()) {
        (Method::Get, "/status") | (Method::Get, "/") => {
            json_response(StatusCode(200), daemon.status())
        }
        (Method::Get, "/schema") => json_response(StatusCode(200), daemon.schema()),
        (Method::Get, "/payment/keys") => {
            json_response(StatusCode(200), json!({ "keys": daemon.payment_keys() }))
        }
        (Method::Post, "/handoff") => {
            json_response(StatusCode(200), daemon.handoff("manual handoff requested"))
        }
        (Method::Post, "/resume") => {
            daemon.resume();
            json_response(
                StatusCode(200),
                json!({ "status": "ready", "session_id": daemon.status()["session_id"].clone() }),
            )
        }
        (Method::Post, "/run") | (Method::Post, "/task") => {
            let body = read_request_body(&mut request)?;
            let run_request = parse_run_request(&body)?;
            let response = daemon.run(run_request.actions)?;
            json_response(StatusCode(response.status()), response.into_body())
        }
        _ => json_response(
            StatusCode(404),
            json!({ "status": "error", "error": "not found" }),
        ),
    };

    request.respond(response)?;
    Ok(())
}

fn read_request_body(request: &mut Request) -> Result<String> {
    let mut body = String::new();
    request.as_reader().read_to_string(&mut body)?;
    Ok(body)
}

fn json_response(status: StatusCode, body: Value) -> Response<std::io::Cursor<Vec<u8>>> {
    let body = serde_json::to_vec_pretty(&body).unwrap_or_else(|_| b"{}".to_vec());
    Response::from_data(body)
        .with_status_code(status)
        .with_header(Header::from_bytes("Content-Type", "application/json").unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_browser_timeout_defaults_long_enough_for_llm() {
        assert!(idle_browser_timeout() >= Duration::from_secs(300));
    }

    #[test]
    #[ignore = "requires Chrome installed"]
    fn browser_tab_survives_llm_idle_gap() {
        let browser = launch_browser(true, None).expect("launch browser");
        let tab = browser.new_tab().expect("open tab");
        thread::sleep(Duration::from_secs(35));
        tab.navigate_to("https://example.com")
            .expect("navigate after idle gap");
    }
}
