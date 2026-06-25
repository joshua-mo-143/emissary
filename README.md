# Emissary

Emissary is a minimal assistant harness with a built-in browser-use tool. One command starts the LLM chat and the headed Chrome daemon together, and stopping Emissary tears everything down.

Cookies and login state persist in `automation-profile/`. Chat conversations persist separately under `.agent-runtime/conversations/`.

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

Emissary can browse sites without any 1Password setup. Checkout credential automation is optional and, when enabled, loads credentials only from 1Password.

### Optional 1Password Setup

Configure 1Password only if you want Emissary to fill checkout payment or address fields.

1. Install the 1Password CLI and sign in:

```sh
op account list
op signin
```

2. Create or choose a 1Password **Credit Card** item for the card Emissary should use. Standard card fields work out of the box.

3. Optional: create or choose an **Identity** item for checkout addresses. Use one shared address item, or separate billing and shipping items when they differ.

4. Run the setup wizard and enter item titles or IDs when prompted:

```sh
cargo run -- setup
```

The setup wizard writes `.agent-runtime/1password.json`, which is gitignored. It stores only 1Password item references, not decrypted card or address values:

```json
{
  "vault": "Private",
  "card": "Personal Visa",
  "address": "Home Address"
}
```

Then start Emissary, or restart it if it is already running:

```sh
cargo run -- chat
```

For separate billing and shipping addresses, leave the shared address blank or provide item refs for the separate prompts. The saved config will look like:

```json
{
  "card": "Personal Visa",
  "billingAddress": "Billing Address",
  "shippingAddress": "Shipping Address"
}
```

For multiple payment profiles or scripts, use env vars instead of the setup file. Env vars take precedence:

```sh
export PAYMENT_1PASSWORD_ITEMS='{
  "default": {
    "card": "Personal Visa",
    "billingAddress": "Billing Address",
    "shippingAddress": "Shipping Address"
  },
  "backup": "Backup Mastercard"
}'
```

The single-profile env var form is still supported:

```sh
export PAYMENT_1PASSWORD_VAULT=Private               # optional when item names are unique
export PAYMENT_1PASSWORD_ITEM="Personal Visa"
export PAYMENT_1PASSWORD_ADDRESS_ITEM="Home Address" # optional shared billing + shipping address
```

Before running a checkout, you can confirm the CLI can read the items:

```sh
op item get "Personal Visa" --format json --reveal
op item get "Home Address" --format json
```

Do not commit secrets or decrypted item JSON. Emissary reads secrets directly from `op` when payment profiles are configured and keeps card/address values out of LLM prompts and tool results.

### Persistent Conversations

`cargo run -- chat` resumes the most recent conversation when one exists, or creates a new conversation otherwise. Use `cargo run -- chat --new` to force a fresh transcript, or `cargo run -- chat --resume <session-id>` to resume a specific session.

Pass a prompt to `chat` to run one turn and exit:

```sh
cargo run -- chat --new "Find the current weather in London"
```

For scripts, add `--print` (`-p`) to keep the final response on stdout as formatted JSON:

```sh
cargo run -- chat --new --print "Find the current weather in London"
```

```json
{
  "conversationId": "emissary-...",
  "output": "..."
}
```

Conversation transcripts are append-only JSONL files in `.agent-runtime/conversations/` by default. Each line stores one replayable chat message. Emissary prepends a fresh system prompt on every startup so current browser session and payment-profile status stay accurate, then replays the saved non-system messages. Browser screenshots and raw payment data are not persisted in transcripts; screenshot image files remain under the runtime directory and payment/address secrets stay in 1Password.

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
| `EMISSARY_RUNTIME_DIR` | `.agent-runtime` | lock file, conversation transcripts, and review screenshots |
| `EMISSARY_IMAGE_DISPLAY` | `auto` | image preview mode: `auto`, `inline`, `path`, or `off` |
| `PAYMENT_1PASSWORD_ITEM` | unset | 1Password item title/ID to load as the `default` payment profile |
| `PAYMENT_1PASSWORD_ITEMS` | unset | JSON object of profile keys to 1Password item specs |
| `PAYMENT_1PASSWORD_PROFILE` | `default` | profile key for `PAYMENT_1PASSWORD_ITEM` |
| `PAYMENT_1PASSWORD_ADDRESS_ITEM` | unset | optional Identity/address item used for both billing and shipping |
| `PAYMENT_1PASSWORD_BILLING_ADDRESS_ITEM` | unset | optional billing Identity/address item |
| `PAYMENT_1PASSWORD_SHIPPING_ADDRESS_ITEM` | unset | optional shipping Identity/address item |
| `PAYMENT_1PASSWORD_VAULT` | unset | optional 1Password vault passed to `op item get --vault` |
| `OP_CLI` | `op` | 1Password CLI executable |
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

1. `observe` returns visible page text plus `elements` like `{ "ref": "e1", "kind": "button", "label": "Search" }`; accessible iframe controls include a `frame` label but use the same ref actions.
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

When the basket/order summary has already been reviewed and the flow reaches card entry, prefer the guarded runtime-owned payment continuation:

```json
{
  "actions": [
    { "op": "autoFillPaymentAndContinue", "profile": "default" }
  ]
}
```

This fills detected payment fields from the vault and clicks only a clearly non-final continue/next/checkout control. Final submit controls such as `Pay now`, `Place order`, or `Complete purchase` still trigger human handoff.

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

Payment and address secrets stay in the vault. Wrong card details are tolerable; blocked submits and bank 2FA are not.

## Credential vault

Emissary does not require a credential vault for browsing-only automation. If checkout payment or address filling is needed, it does not support local JSON files for those credentials; configure one of:

- `PAYMENT_1PASSWORD_ITEM` for a single default profile.
- `PAYMENT_1PASSWORD_ITEMS` for multiple profile keys.

Credit card items can use standard 1Password Credit Card fields. Custom payment fields are also supported when named `card_number`, `exp_month`, `exp_year`, `cvc`, `name`, and `postal_code`; a combined `exp`/`expiry` value such as `12/2028` can be used instead of separate month and year fields.

Shipping and billing address data can live in the same item or in separate Identity/address items. Supported address field names include `full_name`, `first_name`, `last_name`, `organization`, `email`, `phone`, `address_line1`, `address_line2`, `city`, `region`/`state`, `postal_code`, and `country`; prefix with `shipping_` or `billing_` to scope a field.

## Commands

| command | purpose |
|---|---|
| `chat` | start harness + daemon; accepts an optional one-shot prompt |
| `setup` | configure gitignored 1Password item references |
| `stop` | clean stale daemon lock/processes |
| `schema` | print browser tool schema |
| `run` | one-shot headless action batch |
