# Emissary

Emissary is a minimal assistant harness with a built-in browser-use tool. One command starts the LLM chat and the headed Chrome daemon together, and stopping Emissary tears everything down.

Cookies and login state persist in `automation-profile/`.

## Architecture

```text
cargo run -- chat
  ├─ harness (LLM loop)          src/harness.rs
  └─ ManagedDaemon               src/daemon.rs
       ├─ Xvfb + noVNC + Chrome
       ├─ payment vault + guardrails
       └─ shutdown on exit / Ctrl+C
```

There is no separate long-lived `serve` step. The browser daemon lifetime matches the Emissary session.

## Quick start

```sh
sudo apt install xvfb x11vnc novnc websockify chromium

export VENICE_API_KEY=...
cargo run -- chat
```

Emissary auto-detects Chromium/Chrome on `PATH` (including `/snap/bin/chromium`). Override with `CHROME=/path/to/browser` if needed.

On first run, Emissary creates `.agent-runtime/payment.json` with a starter `default` profile (Stripe-style test placeholders). Edit it with your real card details before checkout. The file is created with mode `600`.

To seed manually instead:

```sh
mkdir -p .agent-runtime
cp examples/payment.json.example .agent-runtime/payment.json
chmod 600 .agent-runtime/payment.json
```

Type `exit` or press Ctrl+C to stop. Xvfb, VNC, websockify, and Chrome are stopped with the harness.

If a previous run crashed and left processes behind:

```sh
cargo run -- stop
```

## Environment

| variable | default | purpose |
|---|---|---|
| `VENICE_API_KEY` | required | Venice AI API key |
| `VENICE_BASE_URL` | `https://api.venice.ai/api/v1` | Venice API base URL |
| `VENICE_MODEL` | `deepseek-v4-flash` | chat model |
| `VENICE_TIMEOUT_SECS` | `300` | total timeout for each Venice chat completion request |
| `EMISSARY_RUNTIME_DIR` | `.agent-runtime` | lock file + review screenshots |
| `EMISSARY_IMAGE_DISPLAY` | `auto` | image preview mode: `auto`, `inline`, `path`, or `off` |
| `PAYMENT_FILE` | `.agent-runtime/payment.json` | payment vault |
| `CHROME` | auto-detect | Chromium/Chrome binary path |
| `IDLE_BROWSER_TIMEOUT_SECS` | `3600` | CDP idle timeout; headless_chrome defaults to 30s, which breaks chat while waiting on the LLM |
| `VNC_PORT`, `NOVNC_PORT`, `SCREEN`, … | see daemon | display stack tuning |

## Browser tool

The harness calls the browser in-process (no HTTP hop during chat). Actions are JSON:

```json
{
  "actions": [
    { "op": "webSearch", "query": "Ada Lovelace" },
    { "op": "navigate", "url": "https://example.com" },
    { "op": "observe" },
    { "op": "clickRef", "refId": "e1" }
  ]
}
```

Schema: `cargo run -- schema`

`webSearch` uses DuckDuckGo Instant Answer for lightweight fact/entity lookup without driving the browser.

Prefer the ref flow for dynamic consumer sites:

1. `observe` returns visible page text plus `elements` like `{ "ref": "e1", "kind": "button", "label": "Search" }`.
2. Use `clickRef` / `typeRef` with those refs instead of guessing CSS selectors.
3. Fall back to `click`, `type`, or `html` only when refs are insufficient.

For payment forms, use the same ref flow without exposing card values:

```json
{
  "actions": [
    { "op": "observe" },
    {
      "op": "fillPaymentRefs",
      "fields": [
        { "refId": "e7", "field": "default:card_number" },
        { "refId": "e8", "field": "default:exp" },
        { "refId": "e9", "field": "default:cvc" }
      ]
    }
  ]
}
```

`field` is a vault credential ID, not a secret value. Supported IDs are `card_number`, `exp`, `exp_month`, `exp_year`, `cvc`, `name`, and `postal_code`, optionally prefixed with a profile such as `default:cvc`.

One-shot headless run (separate from chat, for testing):

```sh
cargo run -- run examples/checkout.json
```

## Image previews

Emissary can return product or page pictures to the user before checkout:

- `screenshot` captures the visible page by default, or a specific CSS selector such as a product image/card.
- `review` captures the basket/order summary; if no summary can be found, it falls back to a safe visible-page screenshot.
- Screenshots are saved under `EMISSARY_RUNTIME_DIR` and are stripped before tool results are sent back to the LLM.
- Page/product screenshots are refused when visible payment/card fields are present; use `review` for order-summary-only checkout captures.

Inline terminal rendering is enabled by the default `terminal-images` Cargo feature via `viuer`. Users do not need an extra image-display library, but inline previews work best in terminals with Kitty or iTerm-style image support such as Kitty, Ghostty, WezTerm, or iTerm2. When inline display is unavailable, Emissary still prints the saved PNG path.

Control display with:

```sh
EMISSARY_IMAGE_DISPLAY=auto   # default: try inline, fall back to path
EMISSARY_IMAGE_DISPLAY=path   # only print saved PNG paths
EMISSARY_IMAGE_DISPLAY=inline # warn if inline rendering fails
EMISSARY_IMAGE_DISPLAY=off    # disable inline rendering
```

Build without inline rendering if needed:

```sh
cargo build --no-default-features
```

## Handoff

When checkout needs you:

- **`mode: review`** — order summary + basket/page screenshot in terminal when supported, plus a saved PNG path; open `handoff_url` only to submit
- **`mode: interactive`** — bank/app auth (e.g. Lloyds); use `handoff_url`, then tell Emissary you're done so it sends `{ "op": "resume" }`

Payment secrets stay in the vault. Wrong card details are tolerable; blocked submits and bank 2FA are not.

## Payment vault

```json
{
  "default": {
    "card_number": "4242424242424242",
    "exp_month": "12",
    "exp_year": "2028",
    "cvc": "123",
    "name": "Jane Doe",
    "postal_code": "94107"
  }
}
```

## Commands

| command | purpose |
|---|---|
| `chat` | start harness + daemon |
| `stop` | clean stale daemon lock/processes |
| `schema` | print browser tool schema |
| `run` | one-shot headless action batch |
