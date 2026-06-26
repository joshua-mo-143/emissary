# Emissary — agent guide

Emissary is a minimal Rust assistant harness with a browser-use tool. One binary runs the LLM chat loop and a managed headed Chrome stack (Xvfb, noVNC, websockify, Chromium). The browser session, payment vault, guardrails, and human handoff logic live in-process — not in a separate long-lived daemon the user must babysit.

## Layout

```text
src/
  main.rs      Thin CLI dispatch: chat | setup | stop | run | schema
  args.rs      Clap derive parser and CLI argument normalization
  harness.rs   Venice AI chat loop + browser tool dispatch
  daemon.rs    ManagedDaemon: display stack, Chrome, lock file, shutdown
  actions.rs   JSON browser actions (RunRequest / Action enum)
  payment.rs   1Password credential vault + payment/address field injection
  review.rs    Basket/total review text + screenshots (no payment fields)
```

Runtime data (gitignored):

- `automation-profile/` — persistent Chrome profile
- `.agent-runtime/` — `daemon.lock`, review/page screenshot PNGs

## Commands

| Command | Purpose |
|---------|---------|
| `cargo run -- chat` | Start harness + daemon; primary entry point |
| `cargo run -- stop` | Clean stale lock/processes after a crash |
| `cargo run -- run [file.json]` | One-shot headless action batch (testing) |
| `cargo run -- schema` | Print browser tool JSON schema |

## Environment

| Variable | Required | Default |
|----------|----------|---------|
| `VENICE_API_KEY` | for `chat` | — |
| `VENICE_BASE_URL` | no | `https://api.venice.ai/api/v1` |
| `VENICE_MODEL` | no | `deepseek-v4-flash` |
| `CHROME` | no | auto-detect Chromium |
| `PAYMENT_1PASSWORD_ITEM` | no; only for checkout payment automation | — |
| `PAYMENT_1PASSWORD_ITEMS` | no; only for multiple checkout payment profiles | — |
| `PAYMENT_1PASSWORD_ADDRESS_ITEM` | no | — |
| `PAYMENT_1PASSWORD_BILLING_ADDRESS_ITEM` | no | — |
| `PAYMENT_1PASSWORD_SHIPPING_ADDRESS_ITEM` | no | — |
| `PAYMENT_1PASSWORD_VAULT` | no | — |
| `EMISSARY_RUNTIME_DIR` | no | `.agent-runtime` |
| `EMISSARY_IMAGE_DISPLAY` | no | `auto` |

Venice is OpenAI-compatible; the harness calls `/chat/completions` with tool calling.

## Architecture rules

1. **Single Rust crate.** Do not add a TypeScript/Node harness or split runtimes without an explicit request.
2. **Daemon lifetime = chat lifetime.** `ManagedDaemon` starts in `chat`, shuts down on `exit`, normal return, or Ctrl+C. Do not reintroduce a standalone `serve` workflow as the default path.
3. **Browser tool is the capability boundary.** The LLM sends whitelisted JSON actions only (`Action` enum). No arbitrary JS eval from the model beyond the explicit `eval` action.
4. **Secrets stay out of the LLM.** Credit card and shipping/billing address data must come only from 1Password-backed `PaymentVault` keys (`fillPayment`, `fillAddress`), never from local JSON files, tool arguments, or responses. Redact visible address/contact text from browser results, and strip screenshot base64 before returning tool results to the model.
5. **Handoff is intentional.** Final purchase clicks and bank 2FA pause automation and surface `needs_human` with basket review or `handoff_url`.
6. **Review excludes payment UI.** `review` captures order summary regions only; do not screenshot card fields.

## Idiomatic Rust

Follow these conventions when editing this repo.

### Structure

- Keep modules focused: `actions` = browser DSL, `daemon` = process lifecycle, `harness` = LLM loop, `payment` / `review` = domain logic.
- Keep `main.rs` thin. Put Clap derive types, command parsing, and CLI argument normalization in `args.rs`; `main.rs` should mostly dispatch parsed commands.
- Prefer `Result<T>` with `anyhow::Context` for CLI/harness errors; attach context at boundaries (`?` with `.context("…")`).
- Use `serde` with `#[serde(tag = "op", rename_all = "camelCase")]` for externally visible JSON enums.
- Put tests next to the module they exercise (`#[cfg(test)] mod tests` in the same file).

### Error handling

- Return early; avoid deep nesting.
- Use `bail!` for user-facing CLI errors; use `anyhow!` for wrapping.
- Do not panics in library paths except for truly impossible states (prefer `expect` only with a clear invariant message).

### Ownership and lifecycle

- `ManagedDaemon` owns child processes via `ChildGuard` with `Drop` that kills and waits.
- Use `Option<Runtime>` inside `ManagedDaemon` so `shutdown()` can drop the runtime explicitly before process exit (Ctrl+C handler relies on this).
- Write `.agent-runtime/daemon.lock` on start; reclaim stale locks before spawning new processes.

### I/O and HTTP

- Use `reqwest::blocking` for Venice and reserve `tiny_http` for optional HTTP serving only.
- Use `serde_json::Value` at LLM message boundaries; use typed structs (`RunRequest`, `Action`) for browser actions.

### Strings and JSON

- Raw strings containing `"#` must use `r##"…"##` (or more `#`), not `r#"…"#`, because `"#` terminates the literal.
- Serialize API responses with `serde_json`; redact secrets and base64 before logging or sending to the LLM.

### Style

- Run `cargo fmt` and `cargo check` (and `cargo test`) before finishing.
- Fix all compiler warnings in touched code.
- Minimize scope: match existing naming, error style, and module boundaries.
- Avoid new dependencies unless they clearly reduce complexity; prefer std + existing crates (`anyhow`, `serde`, `reqwest`, `headless_chrome`).

### Testing

- Unit-test pure logic (`is_sensitive_submit`, JSON parsing, field refs, summary sanitization).
- Browser/CDP integration tests are optional; do not add flaky live-browser tests without a fixture or mock strategy.

## Common tasks

**Add a browser action:** extend `Action` in `actions.rs`, handle it in `execute_action`, update `tool_schema()`, README, and this file.

**Payment field filling:** review basket/order summaries before payment when possible, then prefer `autoFillPaymentAndContinue` once card fields are visible. That action fills detected fields from the vault and clicks only guarded non-final continue/next/checkout controls without LLM-selected payment fields or buttons. Keep `observe` -> `fillPaymentRefs`, `fillPayment`, and `fillPaymentField` as fallbacks; the LLM maps refs to vault credential IDs such as `default:card_number` or `default:cvc` and must never send card values, CVV values, or other payment secret values in tool arguments.

**Browser interaction style:** prefer `observe` -> `clickRef` / `typeRef` for normal browsing. `observe` assigns stable `data-emissary-ref` IDs to visible controls and returns them as `elements`, which avoids brittle model-invented CSS selectors. Keep direct `click` / `type` for simple known selectors or tests.

**Image previews:** use `screenshot` for product/page previews before checkout and `review` for basket/order-summary captures. Product/page screenshots must refuse to run when visible payment fields are present; order review screenshots must stay scoped away from payment UI. Terminal inline rendering is a default Cargo feature (`terminal-images`) with saved PNG path fallback.

**Change LLM provider:** edit `LlmConfig` in `harness.rs`; keep OpenAI-compatible chat completions + tools unless the provider requires otherwise.

**Change handoff behavior:** edit `review.rs` (detection, screenshots) and sensitive-click logic in `actions.rs` / `payment.rs`.

## Verification

```sh
cargo fmt --check
cargo check
cargo test
```

Fix any errors and warnings introduced by your changes before marking work complete.
