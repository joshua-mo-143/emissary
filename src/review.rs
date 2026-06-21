use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use headless_chrome::{Tab, protocol::cdp::Page::CaptureScreenshotFormatOption};
use serde_json::{Value, json};

const AUTH_CHALLENGE: &str = "auth challenge detected:";
const BOT_CHALLENGE: &str = "bot challenge detected:";

const BOT_CHALLENGE_PATTERNS: &[&str] = &[
    "just a moment",
    "checking your browser",
    "verify you are human",
    "verify that you are human",
    "needs to review the security of your connection",
    "review the security of your connection",
    "performing security verification",
    "please wait while we verify",
    "ddos protection by cloudflare",
    "attention required",
    "security check",
    "cf-turnstile",
    "turnstile",
    "challenges.cloudflare.com",
    "cdn-cgi/challenge",
    "__cf_chl",
];

const AUTH_PATTERNS: &[&str] = &[
    "verify your identity",
    "verification code",
    "one-time passcode",
    "one time passcode",
    "secure code",
    "confirm in the app",
    "confirm in app",
    "approve in the app",
    "approve in app",
    "approve this payment",
    "banking app",
    "open your banking app",
    "check your mobile banking",
    "check your bank app",
    "authorise this payment",
    "authorize this payment",
    "strong customer authentication",
    "3d secure",
    "3ds",
];

const PAYMENT_LINE_PATTERNS: &[&str] = &[
    "card number",
    "name on card",
    "payment method",
    "security code",
    "cvv",
    "cvc",
    "expiry",
    "expiration",
    "debit card",
    "credit card",
    "billing address",
];

pub fn auth_challenge_prefix() -> &'static str {
    AUTH_CHALLENGE
}

pub fn bot_challenge_prefix() -> &'static str {
    BOT_CHALLENGE
}

pub fn page_needs_bot_challenge(tab: &Tab) -> Result<bool> {
    let js = r###"(() => ({
        text: ((document.body && document.body.innerText) || "").slice(0, 12000),
        title: document.title || "",
        href: location.href || "",
    }))()"###;
    let value = tab.evaluate(js, false)?.value.unwrap_or(Value::Null);
    let text = value
        .get("text")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let title = value
        .get("title")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let href = value
        .get("href")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    if is_bot_challenge_text(text) || is_bot_challenge_text(title) || is_bot_challenge_text(href) {
        return Ok(true);
    }

    let dom_js = r###"(() => !!document.querySelector(
        "#challenge-form, #cf-challenge-running, .cf-browser-verification, iframe[src*='challenges.cloudflare'], iframe[src*='turnstile'], [class*='cf-turnstile'], [id*='turnstile']"
    ))()"###;
    Ok(tab
        .evaluate(dom_js, false)?
        .value
        .and_then(|value| value.as_bool())
        .unwrap_or(false))
}

pub fn ensure_not_bot_challenge(tab: &Tab) -> Result<()> {
    if page_needs_bot_challenge(tab)? {
        bail!("{BOT_CHALLENGE} complete the Cloudflare or bot check via handoff_url, then resume");
    }
    Ok(())
}

pub fn is_bot_challenge_text(text: &str) -> bool {
    let lower = text.to_lowercase();
    BOT_CHALLENGE_PATTERNS
        .iter()
        .any(|pattern| lower.contains(pattern))
        || lower.contains("cloudflare")
            && (lower.contains("checking") || lower.contains("verify") || lower.contains("moment"))
}

pub fn page_state_label(tab: &Tab) -> Option<&'static str> {
    if page_needs_bot_challenge(tab).unwrap_or(false) {
        Some("bot_challenge")
    } else if page_needs_interactive_auth(tab).unwrap_or(false) {
        Some("bank_auth")
    } else {
        None
    }
}

pub fn page_needs_interactive_auth(tab: &Tab) -> Result<bool> {
    let js = r#"(() => {
        const text = (document.body && document.body.innerText) || "";
        return text.slice(0, 12000);
    })()"#;
    let text = tab
        .evaluate(js, false)?
        .value
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default();
    Ok(is_auth_challenge_text(&text))
}

pub fn is_auth_challenge_text(text: &str) -> bool {
    let lower = text.to_lowercase();
    AUTH_PATTERNS.iter().any(|pattern| lower.contains(pattern))
        || lower.contains("otp")
            && (lower.contains("enter") || lower.contains("code") || lower.contains("sent"))
}

pub fn capture_order_review(tab: &Tab) -> Result<Value> {
    if page_needs_interactive_auth(tab)? {
        bail!("{AUTH_CHALLENGE} complete bank or app authentication via handoff_url");
    }

    let target = find_order_review_target(tab)?;
    let Some(target) = target else {
        return Ok(json!({
            "review_scope": "order_summary",
            "order_summary": Value::Null,
            "screenshot_base64": Value::Null,
            "review_note": "could not locate basket or order summary on page",
        }));
    };

    let summary = sanitize_summary_text(&target.summary);
    let screenshot_base64 = capture_target_screenshot(tab, &target.selector)?;
    clear_review_target(tab)?;

    Ok(json!({
        "review_scope": "order_summary",
        "order_summary": summary,
        "screenshot_base64": screenshot_base64,
        "screenshot_mime": "image/png",
    }))
}

pub fn handoff_payload(
    tab: &Tab,
    session_id: &str,
    handoff_url: &str,
    reason: impl Into<String>,
) -> Value {
    let reason = reason.into();

    if page_needs_bot_challenge(tab).unwrap_or(false) {
        return json!({
            "status": "needs_human",
            "mode": "blocked",
            "reason": format!("{BOT_CHALLENGE} Cloudflare is blocking the automated browser. If the challenge does not fully load in handoff, use a normal browser or choose another site."),
            "session_id": session_id,
            "handoff_url": handoff_url,
            "resume": "POST /resume",
            "pageState": "bot_challenge",
            "retry": false,
            "guidance": "Do not keep retrying automation on this page. Ask the user whether to use another food site, continue manually in their normal browser, or try again after they pass Cloudflare outside the harness.",
        });
    }

    if page_needs_interactive_auth(tab).unwrap_or(false) {
        return json!({
            "status": "needs_human",
            "mode": "interactive",
            "reason": format!("{AUTH_CHALLENGE} complete bank or app authentication via handoff_url"),
            "session_id": session_id,
            "handoff_url": handoff_url,
            "resume": "POST /resume",
        });
    }

    match capture_order_review(tab) {
        Ok(review) => json!({
            "status": "needs_human",
            "mode": "review",
            "reason": reason,
            "session_id": session_id,
            "handoff_url": handoff_url,
            "resume": "POST /resume",
            "review": review,
        }),
        Err(error) if error.to_string().starts_with(AUTH_CHALLENGE) => json!({
            "status": "needs_human",
            "mode": "interactive",
            "reason": error.to_string(),
            "session_id": session_id,
            "handoff_url": handoff_url,
            "resume": "POST /resume",
        }),
        Err(error) if error.to_string().starts_with(BOT_CHALLENGE) => json!({
            "status": "needs_human",
            "mode": "blocked",
            "reason": error.to_string(),
            "session_id": session_id,
            "handoff_url": handoff_url,
            "resume": "POST /resume",
            "pageState": "bot_challenge",
            "retry": false,
            "guidance": "Do not keep retrying automation on this page. Ask the user whether to use another food site, continue manually in their normal browser, or try again after they pass Cloudflare outside the harness.",
        }),
        Err(error) => json!({
            "status": "needs_human",
            "mode": "review",
            "reason": reason,
            "session_id": session_id,
            "handoff_url": handoff_url,
            "resume": "POST /resume",
            "review_error": error.to_string(),
        }),
    }
}

struct ReviewTarget {
    selector: String,
    summary: String,
}

fn find_order_review_target(tab: &Tab) -> Result<Option<ReviewTarget>> {
    let js = r#"(() => {
        const paymentHeavy = /card number|cvv|cvc|expiry|expiration date|security code|name on card|payment method/i;
        const summaryHints = /subtotal|total|order summary|your order|your basket|your bag|basket|delivery fee|service fee|tip|items/i;

        let best = null;
        let bestScore = 0;

        for (const el of document.querySelectorAll("section, aside, article, div, [role='complementary'], [data-testid], [class*='cart'], [class*='basket'], [class*='summary'], [class*='order']")) {
            const text = (el.innerText || "").trim();
            if (text.length < 30 || text.length > 8000) continue;
            if (paymentHeavy.test(text)) continue;
            if (!summaryHints.test(text)) continue;
            if (el.querySelector('input[autocomplete^="cc-"], input[name*="card" i], input[name*="cvc" i], input[name*="cvv" i]')) {
                continue;
            }

            let score = 0;
            for (const hint of ["order summary", "your order", "subtotal", "total", "basket", "delivery fee", "service fee", "tip"]) {
                if (text.toLowerCase().includes(hint)) score += 10;
            }
            const rect = el.getBoundingClientRect();
            if (rect.width < 80 || rect.height < 40 || rect.bottom < 0 || rect.top > window.innerHeight) continue;
            score += Math.min(text.length / 100, 20);

            if (score > bestScore) {
                bestScore = score;
                best = el;
            }
        }

        if (!best) return null;

        best.setAttribute("data-telephone-review-target", "1");
        return {
            selector: '[data-telephone-review-target="1"]',
            summary: best.innerText.slice(0, 4000),
        };
    })()"#;

    let Some(value) = tab.evaluate(js, false)?.value else {
        return Ok(None);
    };

    if value.is_null() {
        return Ok(None);
    }

    let selector = value
        .get("selector")
        .and_then(|value| value.as_str())
        .context("review target missing selector")?
        .to_owned();
    let summary = value
        .get("summary")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_owned();

    Ok(Some(ReviewTarget { selector, summary }))
}

fn capture_target_screenshot(tab: &Tab, selector: &str) -> Result<Value> {
    let png = tab
        .wait_for_element(selector)
        .with_context(|| format!("review target `{selector}` disappeared"))?
        .capture_screenshot(CaptureScreenshotFormatOption::Png)
        .context("failed to capture order review screenshot")?;
    Ok(Value::String(BASE64.encode(png)))
}

fn clear_review_target(tab: &Tab) -> Result<()> {
    let _ = tab.evaluate(
        r#"(() => {
            const el = document.querySelector('[data-telephone-review-target="1"]');
            if (el) el.removeAttribute('data-telephone-review-target');
            return true;
        })()"#,
        false,
    )?;
    Ok(())
}

pub fn sanitize_summary_text(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !is_payment_line(line))
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_payment_line(line: &str) -> bool {
    let lower = line.to_lowercase();
    PAYMENT_LINE_PATTERNS
        .iter()
        .any(|pattern| lower.contains(pattern))
        || lower.contains("****") && lower.chars().any(|ch| ch.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::{is_auth_challenge_text, is_bot_challenge_text, sanitize_summary_text};

    #[test]
    fn detects_cloudflare_challenge_pages() {
        assert!(is_bot_challenge_text(
            "Just a moment...\nChecking your browser before accessing ubereats.com"
        ));
        assert!(is_bot_challenge_text(
            "Uber Eats\nVerify you are human by completing the action below."
        ));
    }

    #[test]
    fn ignores_regular_pages_for_bot_challenge() {
        assert!(!is_bot_challenge_text(
            "Order food online\nChinese restaurants near NG10 4HE"
        ));
    }

    #[test]
    fn detects_bank_auth_pages() {
        assert!(is_auth_challenge_text(
            "Please approve this payment in your Lloyds Banking app"
        ));
        assert!(is_auth_challenge_text(
            "Enter the verification code sent to your phone"
        ));
    }

    #[test]
    fn ignores_regular_checkout_copy() {
        assert!(!is_auth_challenge_text(
            "Order summary\nSubtotal £12.00\nTotal £14.50"
        ));
    }

    #[test]
    fn strips_payment_lines_from_summary() {
        let summary =
            sanitize_summary_text("Your order\nPizza x1\nSubtotal £10\nCard number\nTotal £12");
        assert!(summary.contains("Subtotal"));
        assert!(summary.contains("Total"));
        assert!(!summary.contains("Card number"));
    }
}
