# Telephone Agent

A small Rust CLI agent that drives Chrome through the Chrome DevTools Protocol and prints the final task result as JSON.

The agent always reuses one local Chrome profile at `automation-profile/`, so cookies, local storage, and other login state persist across runs.

## System Packages

For hidden headed browser handoff, install:

```sh
sudo apt install xvfb x11vnc novnc websockify chromium
```

## Run

Install Chrome or Chromium first. If it is not auto-detected, pass the executable path with `CHROME`:

```sh
CHROME=/usr/bin/chromium cargo run -- 'goto https://example.com; title'
```

To preset login details, run once with a visible browser, log in manually, then close Chrome:

```sh
HEADLESS=0 CHROME=/usr/bin/chromium cargo run -- 'goto https://example.com; title'
```

After that, normal headless runs reuse the same `automation-profile/` session.

```sh
cargo run -- 'goto https://example.com; title'
cargo run -- 'goto https://example.com; text body'
cargo run -- 'goto https://example.com; eval document.title'
```

Tasks are semicolon-separated steps. Quote CSS selectors or text when they contain spaces:

```sh
cargo run -- 'goto https://duckduckgo.com; type "input[name=q]" "rust cdp"; press Enter; text body'
```

## Persistent Browser Handoff

Start a local daemon with headed Chrome hidden inside Xvfb:

```sh
CHROME=/usr/bin/chromium cargo run -- serve
```

By default this starts:

- API: `http://127.0.0.1:8787`
- Xvfb display: first free display from `:99`, sized to your current display when detectable
- VNC: `127.0.0.1:5900`
- noVNC: `http://127.0.0.1:6080/vnc.html?autoconnect=true&resize=scale`

Send tasks to the existing browser session:

```sh
curl -s http://127.0.0.1:8787/task \
  -H 'Content-Type: application/json' \
  -d '{"task":"goto https://example.com; title"}'
```

Pause automation and take over manually:

```sh
curl -s -X POST http://127.0.0.1:8787/handoff
```

Open the returned `handoff_url`, complete login/auth/payment manually, then resume:

```sh
curl -s -X POST http://127.0.0.1:8787/resume
```

You can override runtime defaults with `DISPLAY_NUM`, `SCREEN`, `VNC_PORT`, `NOVNC_PORT`, `API_ADDR`, `NOVNC_WEB`, and `WEBSOCKIFY`.

For example:

```sh
SCREEN=1920x1080x24 CHROME=/snap/bin/chromium cargo run -- serve
```

## Uber Eats Safety Boundary

Use the agent to browse, search, choose items, and build a cart. The runtime blocks clicks whose target looks like checkout or payment UI, such as `Checkout`, `Place order`, `Pay`, `Buy now`, or `Proceed to payment`, and returns a `needs_human` response with the noVNC handoff URL.

The intended flow is:

1. Agent builds the cart.
2. Runtime blocks checkout/payment action and pauses.
3. Human opens noVNC, reviews restaurant, address, items, fees, tip, and total.
4. Human completes payment/order manually.
5. Human resumes the agent if more browsing is needed.

## Commands

- `goto|open <url>` navigates to a page.
- `click <css>` clicks the first matching element.
- `type <css> <text>` focuses an element and types text.
- `press <key>` presses a key such as `Enter`.
- `wait <css>` waits for an element.
- `title` returns the page title.
- `text [css]` returns `innerText`, defaulting to `body`.
- `html [css]` returns `innerHTML`, defaulting to `body`.
- `eval <javascript>` evaluates JavaScript and returns its JSON value.
