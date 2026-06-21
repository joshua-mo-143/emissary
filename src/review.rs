use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use headless_chrome::{Tab, protocol::cdp::Page::CaptureScreenshotFormatOption};
use serde::Serialize;
use serde_json::Value;
use std::{error::Error, fmt};

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

#[derive(Debug, Clone, Serialize)]
pub struct HandoffPayload {
    pub status: &'static str,
    pub mode: HandoffMode,
    pub reason: String,
    pub session_id: String,
    pub handoff_url: String,
    pub resume: ResumeInstruction,
    #[serde(rename = "pageState", skip_serializing_if = "Option::is_none")]
    pub page_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guidance: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review: Option<OrderReview>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandoffReason {
    SensitiveSubmit(String),
    AuthChallenge,
    BotChallenge,
    Manual,
    AlreadyPaused,
}

impl HandoffReason {
    pub fn sensitive_submit(details: impl Into<String>) -> Self {
        Self::SensitiveSubmit(details.into())
    }

    pub fn auth_challenge() -> Self {
        Self::AuthChallenge
    }

    pub fn bot_challenge() -> Self {
        Self::BotChallenge
    }

    pub fn manual() -> Self {
        Self::Manual
    }

    pub fn already_paused() -> Self {
        Self::AlreadyPaused
    }

    pub fn message(&self) -> String {
        match self {
            Self::SensitiveSubmit(details) => format!("sensitive click blocked: {details}"),
            Self::AuthChallenge => {
                format!("{AUTH_CHALLENGE} complete bank or app authentication via handoff_url")
            }
            Self::BotChallenge => format!(
                "{BOT_CHALLENGE} Cloudflare is blocking the automated browser. If the challenge does not fully load in handoff, use a normal browser or choose another site."
            ),
            Self::Manual => "manual handoff requested".to_owned(),
            Self::AlreadyPaused => "human handoff is active".to_owned(),
        }
    }
}

#[derive(Debug)]
pub struct HandoffRequired {
    reason: HandoffReason,
}

impl HandoffRequired {
    pub fn new(reason: HandoffReason) -> Self {
        Self { reason }
    }

    pub fn reason(&self) -> &HandoffReason {
        &self.reason
    }
}

impl fmt::Display for HandoffRequired {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.reason.message())
    }
}

impl Error for HandoffRequired {}

pub fn handoff_required(reason: HandoffReason) -> anyhow::Error {
    anyhow::Error::new(HandoffRequired::new(reason))
}

pub fn handoff_reason(error: &anyhow::Error) -> Option<HandoffReason> {
    error
        .downcast_ref::<HandoffRequired>()
        .map(|handoff| handoff.reason().clone())
}

#[derive(Debug, Clone, Serialize)]
pub struct ResumeInstruction {
    pub op: &'static str,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffMode {
    Review,
    Interactive,
    Blocked,
}

#[derive(Debug, Clone, Serialize)]
pub struct OrderReview {
    pub review_scope: &'static str,
    pub order_summary: Option<String>,
    pub screenshot_base64: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screenshot_mime: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BrowserScreenshot {
    pub screenshot_scope: &'static str,
    pub screenshot_base64: String,
    pub screenshot_mime: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screenshot_note: Option<String>,
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
        return Err(handoff_required(HandoffReason::bot_challenge()));
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

fn ensure_no_visible_payment_ui(tab: &Tab) -> Result<()> {
    if page_has_visible_payment_ui(tab)? {
        bail!(
            "refusing page screenshot because visible payment fields are present; use review for an order-summary-only capture"
        );
    }
    Ok(())
}

fn page_has_visible_payment_ui(tab: &Tab) -> Result<bool> {
    let js = r#"(() => {
        const paymentField = /cc-|card|cvc|cvv|security code|expiry|expiration|cardholder|name on card|billing postal|billing zip/i;
        const paymentText = /card number|cvv|cvc|security code|expiry|expiration date|name on card/i;

        function visible(el) {
            const rect = el.getBoundingClientRect();
            const style = getComputedStyle(el);
            return rect.width > 1 &&
                rect.height > 1 &&
                rect.bottom >= 0 &&
                rect.right >= 0 &&
                rect.top <= innerHeight &&
                rect.left <= innerWidth &&
                style.visibility !== "hidden" &&
                style.display !== "none" &&
                style.opacity !== "0";
        }

        for (const el of document.querySelectorAll("input, textarea, select, [contenteditable='true']")) {
            if (!visible(el)) continue;
            const hay = [
                el.getAttribute("autocomplete"),
                el.getAttribute("name"),
                el.getAttribute("id"),
                el.getAttribute("aria-label"),
                el.getAttribute("placeholder")
            ].filter(Boolean).join(" ");
            if (paymentField.test(hay)) return true;
        }

        for (const el of document.querySelectorAll("form, section, aside, [role='form'], [data-testid], [class*='payment'], [id*='payment']")) {
            if (!visible(el)) continue;
            const text = (el.innerText || "").slice(0, 2000);
            if (paymentText.test(text) && el.querySelector("input, textarea, select, [contenteditable='true']")) {
                return true;
            }
        }

        return false;
    })()"#;
    Ok(tab
        .evaluate(js, false)?
        .value
        .and_then(|value| value.as_bool())
        .unwrap_or(false))
}

pub fn capture_order_review(tab: &Tab) -> Result<OrderReview> {
    if page_needs_interactive_auth(tab)? {
        return Err(handoff_required(HandoffReason::auth_challenge()));
    }

    let target = find_order_review_target(tab)?;
    let Some(target) = target else {
        if let Ok(screenshot) = capture_safe_page_screenshot(tab, None) {
            return Ok(OrderReview {
                review_scope: "page_viewport",
                order_summary: None,
                screenshot_base64: Some(screenshot.screenshot_base64),
                screenshot_mime: Some("image/png"),
                review_note: Some(
                    "could not locate basket or order summary; captured the visible page instead"
                        .to_owned(),
                ),
            });
        }

        return Ok(OrderReview {
            review_scope: "order_summary",
            order_summary: None,
            screenshot_base64: None,
            screenshot_mime: None,
            review_note: Some("could not locate basket or order summary on page".to_owned()),
        });
    };

    let summary = sanitize_summary_text(&target.summary);
    let screenshot_base64 = capture_target_screenshot(tab, &target.selector)?;
    clear_review_target(tab)?;

    Ok(OrderReview {
        review_scope: "order_summary",
        order_summary: Some(summary),
        screenshot_base64: Some(screenshot_base64),
        screenshot_mime: Some("image/png"),
        review_note: None,
    })
}

pub fn capture_safe_page_screenshot(
    tab: &Tab,
    selector: Option<&str>,
) -> Result<BrowserScreenshot> {
    if page_needs_interactive_auth(tab)? {
        return Err(handoff_required(HandoffReason::auth_challenge()));
    }
    ensure_no_visible_payment_ui(tab)?;

    let (png, scope, normalized_selector) = match selector.map(str::trim).filter(|s| !s.is_empty())
    {
        Some(selector) => {
            let png = tab
                .wait_for_element(selector)
                .with_context(|| format!("screenshot target `{selector}` was not found"))?
                .capture_screenshot(CaptureScreenshotFormatOption::Png)
                .context("failed to capture selected page screenshot")?;
            (png, "selected_element", Some(selector.to_owned()))
        }
        None => {
            let png = tab
                .capture_screenshot(CaptureScreenshotFormatOption::Png, None, None, true)
                .context("failed to capture page screenshot")?;
            (png, "page_viewport", None)
        }
    };

    Ok(BrowserScreenshot {
        screenshot_scope: scope,
        screenshot_base64: BASE64.encode(png),
        screenshot_mime: "image/png",
        selector: normalized_selector,
        screenshot_note: None,
    })
}

pub fn handoff_payload(
    tab: &Tab,
    session_id: &str,
    handoff_url: &str,
    reason: HandoffReason,
) -> HandoffPayload {
    if page_needs_bot_challenge(tab).unwrap_or(false) {
        return HandoffPayload::blocked(
            session_id,
            handoff_url,
            HandoffReason::bot_challenge().message(),
        );
    }

    if page_needs_interactive_auth(tab).unwrap_or(false) {
        return HandoffPayload::interactive(
            session_id,
            handoff_url,
            HandoffReason::auth_challenge().message(),
        );
    }

    match capture_order_review(tab) {
        Ok(review) => HandoffPayload::review(
            session_id,
            handoff_url,
            reason.message(),
            Some(review),
            None,
        ),
        Err(error) => match handoff_reason(&error) {
            Some(HandoffReason::AuthChallenge) => {
                HandoffPayload::interactive(session_id, handoff_url, error.to_string())
            }
            Some(HandoffReason::BotChallenge) => {
                HandoffPayload::blocked(session_id, handoff_url, error.to_string())
            }
            Some(reason) => HandoffPayload::review(
                session_id,
                handoff_url,
                reason.message(),
                None,
                Some(error.to_string()),
            ),
            None => HandoffPayload::review(
                session_id,
                handoff_url,
                reason.message(),
                None,
                Some(error.to_string()),
            ),
        },
    }
}

impl HandoffPayload {
    fn review(
        session_id: &str,
        handoff_url: &str,
        reason: String,
        review: Option<OrderReview>,
        review_error: Option<String>,
    ) -> Self {
        Self {
            status: "needs_human",
            mode: HandoffMode::Review,
            reason,
            session_id: session_id.to_owned(),
            handoff_url: handoff_url.to_owned(),
            resume: resume_instruction(),
            page_state: None,
            retry: None,
            guidance: None,
            review,
            review_error,
        }
    }

    fn interactive(session_id: &str, handoff_url: &str, reason: String) -> Self {
        Self {
            status: "needs_human",
            mode: HandoffMode::Interactive,
            reason,
            session_id: session_id.to_owned(),
            handoff_url: handoff_url.to_owned(),
            resume: resume_instruction(),
            page_state: None,
            retry: None,
            guidance: None,
            review: None,
            review_error: None,
        }
    }

    fn blocked(session_id: &str, handoff_url: &str, reason: String) -> Self {
        Self {
            status: "needs_human",
            mode: HandoffMode::Blocked,
            reason,
            session_id: session_id.to_owned(),
            handoff_url: handoff_url.to_owned(),
            resume: resume_instruction(),
            page_state: Some("bot_challenge".to_owned()),
            retry: Some(false),
            guidance: Some("Do not keep retrying automation on this page. Ask the user whether to use another food site, continue manually in their normal browser, or try again after they pass Cloudflare outside the harness.".to_owned()),
            review: None,
            review_error: None,
        }
    }
}

fn resume_instruction() -> ResumeInstruction {
    ResumeInstruction { op: "resume" }
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

        best.setAttribute("data-emissary-review-target", "1");
        return {
            selector: '[data-emissary-review-target="1"]',
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

fn capture_target_screenshot(tab: &Tab, selector: &str) -> Result<String> {
    let png = tab
        .wait_for_element(selector)
        .with_context(|| format!("review target `{selector}` disappeared"))?
        .capture_screenshot(CaptureScreenshotFormatOption::Png)
        .context("failed to capture order review screenshot")?;
    Ok(BASE64.encode(png))
}

fn clear_review_target(tab: &Tab) -> Result<()> {
    let _ = tab.evaluate(
        r#"(() => {
            const el = document.querySelector('[data-emissary-review-target="1"]');
            if (el) el.removeAttribute('data-emissary-review-target');
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
    use super::{
        BrowserScreenshot, HandoffPayload, HandoffReason, handoff_reason, handoff_required,
        is_auth_challenge_text, is_bot_challenge_text, sanitize_summary_text,
    };

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

    #[test]
    fn serializes_blocked_handoff_shape() {
        let payload = HandoffPayload::blocked(
            "session",
            "http://127.0.0.1:6080",
            "bot challenge detected: blocked".to_owned(),
        );
        let value = serde_json::to_value(payload).unwrap();
        assert_eq!(value["status"], "needs_human");
        assert_eq!(value["mode"], "blocked");
        assert_eq!(value["pageState"], "bot_challenge");
        assert_eq!(value["retry"], false);
        assert_eq!(value["resume"]["op"], "resume");
    }

    #[test]
    fn extracts_typed_handoff_reason_from_error() {
        let error = handoff_required(HandoffReason::manual());
        assert_eq!(handoff_reason(&error), Some(HandoffReason::Manual));
    }

    #[test]
    fn serializes_page_screenshot_shape() {
        let screenshot = BrowserScreenshot {
            screenshot_scope: "page_viewport",
            screenshot_base64: "abc".to_owned(),
            screenshot_mime: "image/png",
            selector: None,
            screenshot_note: None,
        };
        let value = serde_json::to_value(screenshot).unwrap();
        assert_eq!(value["screenshot_scope"], "page_viewport");
        assert_eq!(value["screenshot_mime"], "image/png");
        assert!(value.get("selector").is_none());
    }
}
