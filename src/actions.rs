use crate::browser_dom::FRAME_HELPERS;
use crate::payment::{
    PaymentFieldMapping, PaymentVault, block_type_on_credential_field, is_sensitive_submit,
};
use crate::review::{self, HandoffPayload, HandoffReason, capture_order_review};
use crate::search::duckduckgo_instant_answer;
use anyhow::{Result, bail};
use headless_chrome::Tab;
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{sync::Arc, thread, time::Duration};

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum Action {
    /// Search DuckDuckGo Instant Answer for facts and entities without using the browser.
    WebSearch {
        query: String,
    },
    /// Navigate the browser to a URL.
    Navigate {
        url: String,
    },
    /// Return visible page text and stable refs for clickable/input elements.
    Observe,
    /// Click a visible element by CSS selector, or XPath when the selector starts with //.
    Click {
        selector: String,
    },
    /// Click an element ref returned by observe/current page elements.
    ClickRef {
        #[serde(rename = "refId")]
        ref_id: String,
    },
    /// Click a button or link by visible label text.
    ClickText {
        text: String,
    },
    /// Type text into a selector. Do not use for payment fields.
    Type {
        selector: String,
        text: String,
    },
    /// Type text into an observed element ref. Do not use for payment fields.
    TypeRef {
        #[serde(rename = "refId")]
        ref_id: String,
        text: String,
    },
    /// Press a keyboard key such as Enter or Tab.
    Press {
        key: String,
    },
    /// Wait for a CSS selector or XPath to become visible.
    Wait {
        selector: String,
    },
    /// Return the current page title.
    Title,
    /// Return innerText for a selector, defaulting to body.
    Text {
        #[serde(default = "default_body_selector")]
        selector: String,
    },
    /// Return innerHTML for a selector, defaulting to body.
    Html {
        #[serde(default = "default_body_selector")]
        selector: String,
    },
    /// Evaluate JavaScript in the page.
    Eval {
        expression: String,
    },
    /// Legacy automatic payment form fill by profile key. Prefer observe -> fillPaymentRefs.
    FillPayment {
        profile: String,
    },
    /// Fill payment fields by observed element refs and vault credential IDs.
    FillPaymentRefs {
        #[schemars(length(min = 1))]
        fields: Vec<PaymentFieldMapping>,
    },
    /// Fill one payment field by selector and vault credential ID.
    FillPaymentField {
        selector: String,
        field: String,
    },
    /// Fill detected payment fields and guardedly continue to the next checkout step.
    AutoFillPaymentAndContinue {
        profile: String,
    },
    FillAddress {
        profile: String,
        #[serde(default)]
        kind: Option<String>,
    },
    FillAddressField {
        selector: String,
        field: String,
    },
    Screenshot {
        #[serde(default)]
        selector: Option<String>,
    },
    /// Capture an order-summary review, scoped away from payment UI.
    Review,
    /// Pause automation and return handoff_url.
    Handoff,
    /// Resume automation after human handoff.
    Resume,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunRequest {
    /// Ordered browser actions to run in the current session.
    #[schemars(length(min = 1))]
    pub actions: Vec<Action>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowserToolArguments {
    /// Ordered browser actions to run in the current session. Execution stops on the first handoff or error.
    #[schemars(length(min = 1))]
    pub actions: Vec<Action>,
}

#[derive(Debug, Serialize)]
pub struct RunSuccess {
    pub status: &'static str,
    pub completed: usize,
    pub results: Vec<ActionResult>,
    #[serde(flatten)]
    pub current_page: PageSnapshot,
}

#[derive(Debug, Serialize)]
pub struct PageSnapshot {
    pub title: String,
    #[serde(rename = "pageText")]
    pub page_text: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub elements: Vec<ElementRef>,
    #[serde(rename = "pageState", skip_serializing_if = "Option::is_none")]
    pub page_state: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ElementRef {
    #[serde(rename = "ref")]
    pub ref_id: String,
    pub kind: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ActionResult {
    pub index: usize,
    pub op: String,
    pub ok: Value,
}

#[derive(Debug)]
pub enum RunOutcome {
    Success(RunSuccess),
    Failed(RunFailure),
    NeedsHuman {
        completed: usize,
        results: Vec<ActionResult>,
        handoff: HandoffPayload,
    },
}

#[derive(Debug, Serialize)]
pub struct RunFailure {
    pub status: &'static str,
    pub completed: usize,
    pub failed_at: usize,
    pub error: String,
    pub results: Vec<ActionResult>,
    #[serde(flatten)]
    pub current_page: PageSnapshot,
}

pub struct RunContext<'a> {
    pub tab: &'a Arc<Tab>,
    pub payment: &'a PaymentVault,
    pub paused: Option<&'a mut bool>,
    pub session_id: &'a str,
    pub handoff_url: &'a str,
}

pub fn tool_schema() -> Value {
    let mut parameters = serde_json::to_value(schema_for!(BrowserToolArguments))
        .expect("browser tool argument schema serializes to JSON");
    if let Some(object) = parameters.as_object_mut() {
        object.remove("$schema");
        object.remove("title");
    }

    json!({
        "name": "browser",
        "description": "Browser-use tool for the Emissary harness. Controls a persistent local Chrome session, with payment vault injection, basket review screenshots, and human handoff for final submits or bank 2FA.",
        "parameters": parameters,
        "responses": {
            "ok": {
                "status": "ok",
                "completed": 3,
                "title": "Uber Eats",
                "pageText": "Visible page text snapshot (truncated)…",
                "elements": [{ "ref": "e1", "kind": "button", "label": "Search", "frame": "iframe:1" }],
                "results": [{ "index": 0, "op": "screenshot", "ok": { "screenshot_scope": "page_viewport", "screenshot_base64": "..." } }]
            },
            "needs_human": {
                "status": "needs_human",
                "completed": 2,
                "mode": "review",
                "review": { "order_summary": "...", "screenshot_base64": "..." },
                "handoff_url": "http://127.0.0.1:6080/vnc.html?autoconnect=true&resize=scale"
            },
            "error": {
                "status": "error",
                "completed": 1,
                "failed_at": 1,
                "error": "action 1 (click selector=\"#missing\"): no visible element matched",
                "title": "Uber Eats",
                "pageText": "Visible page text snapshot (truncated)…",
                "results": [{ "index": 0, "op": "navigate", "ok": { "title": "Example" } }]
            }
        }
    })
}

pub fn run_actions(context: &mut RunContext<'_>, request: &RunRequest) -> Result<RunOutcome> {
    if request.actions.is_empty() {
        return Ok(RunOutcome::Failed(RunFailure {
            status: "error",
            completed: 0,
            failed_at: 0,
            error: "actions must contain at least one item".to_owned(),
            results: Vec::new(),
            current_page: current_page_snapshot(context.tab.as_ref()),
        }));
    }

    let mut results = Vec::new();

    for (index, action) in request.actions.iter().enumerate() {
        match execute_action(context, action) {
            Ok(ok) => results.push(ActionResult {
                index,
                op: op_name(action).to_owned(),
                ok,
            }),
            Err(error) => {
                if let Some(reason) = review::handoff_reason(&error) {
                    let handoff = review::handoff_payload(
                        context.tab.as_ref(),
                        context.session_id,
                        context.handoff_url,
                        reason,
                    );
                    if let Some(paused) = context.paused.as_deref_mut() {
                        *paused = true;
                    }
                    return Ok(RunOutcome::NeedsHuman {
                        completed: index,
                        results,
                        handoff,
                    });
                }
                let message = error.to_string();
                return Ok(RunOutcome::Failed(RunFailure {
                    status: "error",
                    completed: index,
                    failed_at: index,
                    error: format!("action {index} ({}): {message}", action_detail(action)),
                    results,
                    current_page: current_page_snapshot(context.tab.as_ref()),
                }));
            }
        }
    }

    if review::page_needs_bot_challenge(context.tab.as_ref()).unwrap_or(false) {
        if let Some(paused) = context.paused.as_deref_mut() {
            *paused = true;
        }
        return Ok(RunOutcome::NeedsHuman {
            completed: results.len(),
            results,
            handoff: review::handoff_payload(
                context.tab.as_ref(),
                context.session_id,
                context.handoff_url,
                HandoffReason::bot_challenge(),
            ),
        });
    }

    Ok(RunOutcome::Success(RunSuccess {
        status: "ok",
        completed: results.len(),
        results,
        current_page: current_page_snapshot(context.tab.as_ref()),
    }))
}

fn execute_action(context: &mut RunContext<'_>, action: &Action) -> Result<Value> {
    let tab = context.tab.as_ref();
    match action {
        Action::WebSearch { query } => Ok(json!(duckduckgo_instant_answer(query)?)),
        Action::Navigate { url } => {
            tab.navigate_to(url)?;
            let wait_result = tab.wait_until_navigated();
            if review::page_needs_bot_challenge(tab).unwrap_or(false) {
                review::ensure_not_bot_challenge(tab)?;
            }
            if let Err(error) = wait_result {
                if review::page_needs_bot_challenge(tab).unwrap_or(false) {
                    review::ensure_not_bot_challenge(tab)?;
                }
                return Err(error);
            }
            review::ensure_not_bot_challenge(tab)?;
            Ok(json!({ "url": url, "title": tab.get_title()? }))
        }
        Action::Observe => Ok(json!(current_page_snapshot(tab))),
        Action::Click { selector } => {
            let element = wait_for_target(tab, selector)
                .map_err(|error| enrich_selector_error(selector, error))?;
            block_sensitive_click(tab, selector)?;
            element.click()?;
            thread::sleep(Duration::from_millis(750));
            Ok(json!({ "selector": selector }))
        }
        Action::ClickRef { ref_id } => {
            let details = element_ref_details(tab, ref_id)?;
            if is_sensitive_submit(&details) {
                return Err(review::handoff_required(HandoffReason::sensitive_submit(
                    details,
                )));
            }
            click_by_ref(tab, ref_id)
        }
        Action::ClickText { text } => {
            if is_sensitive_submit(text) {
                return Err(review::handoff_required(HandoffReason::sensitive_submit(
                    text.clone(),
                )));
            }
            click_by_visible_text(tab, text)
        }
        Action::Type { selector, text } => {
            let element = wait_for_target(tab, selector)
                .map_err(|error| enrich_selector_error(selector, error))?;
            block_type_on_credential_field(tab, selector)?;
            element.click()?;
            tab.type_str(text)?;
            Ok(json!({ "selector": selector, "typed": text }))
        }
        Action::TypeRef { ref_id, text } => {
            if element_ref_is_credential_field(tab, ref_id)? {
                bail!("credential fields must use fillPayment/fillAddress vault actions");
            }
            type_by_ref(tab, ref_id, text)
        }
        Action::Press { key } => {
            tab.press_key(key)?;
            Ok(json!({ "key": key }))
        }
        Action::Wait { selector } => {
            wait_for_target(tab, selector)
                .map_err(|error| enrich_selector_error(selector, error))?;
            Ok(json!({ "selector": selector }))
        }
        Action::Title => Ok(json!(tab.get_title()?)),
        Action::Text { selector } => selector_value(tab, selector, "innerText"),
        Action::Html { selector } => selector_value(tab, selector, "innerHTML"),
        Action::Eval { expression } => {
            Ok(tab.evaluate(expression, true)?.value.unwrap_or(Value::Null))
        }
        Action::FillPayment { profile } => {
            PaymentVault::fill_payment(tab, context.payment, profile)
        }
        Action::FillPaymentRefs { fields } => {
            PaymentVault::fill_payment_refs(tab, context.payment, fields)
        }
        Action::FillPaymentField { selector, field } => {
            PaymentVault::fill_payment_field(tab, context.payment, selector, field)
        }
        Action::FillAddress { profile, kind } => {
            PaymentVault::fill_address(tab, context.payment, profile, kind.as_deref())
        }
        Action::FillAddressField { selector, field } => {
            PaymentVault::fill_address_field(tab, context.payment, selector, field)
        }
        Action::AutoFillPaymentAndContinue { profile } => {
            let result = PaymentVault::fill_payment_and_continue(tab, context.payment, profile)
                .map_err(|_| review::handoff_required(HandoffReason::manual()))?;
            if review::page_needs_interactive_auth(tab)? {
                return Err(review::handoff_required(HandoffReason::auth_challenge()));
            }
            Ok(result)
        }
        Action::Screenshot { selector } => Ok(json!(review::capture_safe_page_screenshot(
            tab,
            selector.as_deref()
        )?)),
        Action::Review => Ok(json!(capture_order_review(tab)?)),
        Action::Handoff => {
            if let Some(paused) = context.paused.as_deref_mut() {
                *paused = true;
            }
            Err(review::handoff_required(HandoffReason::manual()))
        }
        Action::Resume => {
            let Some(paused) = context.paused.as_deref_mut() else {
                bail!("resume is only available in a managed browser session");
            };
            if !*paused {
                bail!("automation is not paused");
            }
            *paused = false;
            Ok(json!({ "resumed": true, "session_id": context.session_id }))
        }
    }
}

fn op_name(action: &Action) -> &'static str {
    match action {
        Action::WebSearch { .. } => "webSearch",
        Action::Navigate { .. } => "navigate",
        Action::Observe => "observe",
        Action::Click { .. } => "click",
        Action::ClickRef { .. } => "clickRef",
        Action::ClickText { .. } => "clickText",
        Action::Type { .. } => "type",
        Action::TypeRef { .. } => "typeRef",
        Action::Press { .. } => "press",
        Action::Wait { .. } => "wait",
        Action::Title => "title",
        Action::Text { .. } => "text",
        Action::Html { .. } => "html",
        Action::Eval { .. } => "eval",
        Action::FillPayment { .. } => "fillPayment",
        Action::FillPaymentRefs { .. } => "fillPaymentRefs",
        Action::FillPaymentField { .. } => "fillPaymentField",
        Action::FillAddress { .. } => "fillAddress",
        Action::FillAddressField { .. } => "fillAddressField",
        Action::AutoFillPaymentAndContinue { .. } => "autoFillPaymentAndContinue",
        Action::Screenshot { .. } => "screenshot",
        Action::Review => "review",
        Action::Handoff => "handoff",
        Action::Resume => "resume",
    }
}

fn default_body_selector() -> String {
    "body".to_owned()
}

fn action_detail(action: &Action) -> String {
    match action {
        Action::WebSearch { query } => format!("webSearch query={query:?}"),
        Action::Navigate { url } => format!("navigate url={url}"),
        Action::Observe => "observe".to_owned(),
        Action::Click { selector } => format!("click selector={selector:?}"),
        Action::ClickRef { ref_id } => format!("clickRef refId={ref_id:?}"),
        Action::ClickText { text } => format!("clickText text={text:?}"),
        Action::Type { selector, text } => format!("type selector={selector:?} text={text:?}"),
        Action::TypeRef { ref_id, text } => format!("typeRef refId={ref_id:?} text={text:?}"),
        Action::Press { key } => format!("press key={key}"),
        Action::Wait { selector } => format!("wait selector={selector:?}"),
        Action::Text { selector } => format!("text selector={selector:?}"),
        Action::Html { selector } => format!("html selector={selector:?}"),
        Action::Eval { expression } => format!("eval expression={expression:?}"),
        Action::FillPayment { profile } => format!("fillPayment profile={profile}"),
        Action::FillPaymentRefs { fields } => {
            let mappings = fields
                .iter()
                .map(|field| format!("{}={}", field.ref_id, field.field))
                .collect::<Vec<_>>()
                .join(", ");
            format!("fillPaymentRefs fields=[{mappings}]")
        }
        Action::FillPaymentField { selector, field } => {
            format!("fillPaymentField selector={selector:?} field={field}")
        }
        Action::FillAddress { profile, kind } => {
            format!("fillAddress profile={profile} kind={kind:?}")
        }
        Action::FillAddressField { selector, field } => {
            format!("fillAddressField selector={selector:?} field={field}")
        }
        Action::AutoFillPaymentAndContinue { profile } => {
            format!("autoFillPaymentAndContinue profile={profile}")
        }
        Action::Screenshot { selector } => match selector {
            Some(selector) => format!("screenshot selector={selector:?}"),
            None => "screenshot".to_owned(),
        },
        Action::Title => "title".to_owned(),
        Action::Review => "review".to_owned(),
        Action::Handoff => "handoff".to_owned(),
        Action::Resume => "resume".to_owned(),
    }
}

fn wait_for_target<'tab>(tab: &'tab Tab, selector: &str) -> Result<headless_chrome::Element<'tab>> {
    let selector = selector.trim();
    if is_xpath_selector(selector) {
        let xpath = selector.strip_prefix("xpath:").unwrap_or(selector).trim();
        tab.wait_for_xpath(xpath)
    } else {
        tab.wait_until_visible(selector)
    }
}

fn is_xpath_selector(selector: &str) -> bool {
    selector.starts_with("//") || selector.starts_with("xpath:")
}

fn enrich_selector_error(selector: &str, error: anyhow::Error) -> anyhow::Error {
    let message = error.to_string();
    if message.contains("DOM Error while querying") || message.contains("-32000") {
        error.context(format!(
            "invalid CSS selector {selector:?}; use standard document.querySelector syntax (no :contains, :text, or Playwright locators), clickText for visible labels, or an XPath starting with //"
        ))
    } else if message.contains("No element found") || message.contains("NoElementFound") {
        error.context(format!(
            "no visible element matched {selector:?}; wait for the page to settle or inspect with text/title first"
        ))
    } else {
        error.context(format!("selector {selector:?}"))
    }
}

fn click_by_visible_text(tab: &Tab, text: &str) -> Result<Value> {
    let needle = serde_json::to_string(text)?;
    let js = format!(
        r#"{FRAME_HELPERS}
        (() => {{
            const needle = {needle}.trim().toLowerCase();
            if (!needle) return {{ clicked: false, reason: "empty text" }};
            const selectors = [
                "button",
                "a[href]",
                "[role='button']",
                "[role='link']",
                "input[type='submit']",
                "input[type='button']",
                "[onclick]",
                "label",
            ];
            const seen = new Set();
            for (const ctx of emissaryFrameDocuments()) {{
                if (ctx.frameElement && !emissaryVisible(ctx.frameElement)) continue;
                for (const sel of selectors) {{
                    for (const el of ctx.doc.querySelectorAll(sel)) {{
                    if (seen.has(el) || !emissaryVisible(el)) continue;
                    seen.add(el);
                    const hay = [
                        el.innerText,
                        el.textContent,
                        el.getAttribute("aria-label"),
                        el.getAttribute("title"),
                        el.getAttribute("value"),
                    ]
                        .filter(Boolean)
                        .join(" ")
                        .toLowerCase();
                    if (!hay.includes(needle)) continue;
                    emissaryScrollIntoView(ctx, el);
                    el.click();
                    return JSON.stringify({{
                        clicked: true,
                        text: {needle},
                        tag: el.tagName,
                        frame: emissaryFrameName(ctx.path),
                        label: (el.innerText || el.getAttribute("aria-label") || "").trim().slice(0, 200),
                    }});
                    }}
                }}
            }}
            return JSON.stringify({{ clicked: false, text: {needle}, reason: "no matching clickable element" }});
        }})()"#
    );
    let value = tab.evaluate(&js, true)?.value.unwrap_or(Value::Null);
    let value = parse_json_string(value)?;
    if value
        .get("clicked")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        thread::sleep(Duration::from_millis(750));
        Ok(value)
    } else {
        bail!(
            "clickText found no clickable element containing {text:?}; inspect the page with text/title first"
        )
    }
}

fn selector_value(tab: &Tab, selector: &str, property: &str) -> Result<Value> {
    let selector_json = serde_json::to_string(selector)?;
    let js = format!(
        r#"(() => {{
            const el = document.querySelector({selector_json});
            return el ? el.{property} : null;
        }})()"#
    );
    Ok(tab.evaluate(&js, false)?.value.unwrap_or(Value::Null))
}

fn current_page_snapshot(tab: &Tab) -> PageSnapshot {
    PageSnapshot {
        title: tab.get_title().unwrap_or_default(),
        page_text: page_text_snapshot(tab)
            .ok()
            .map(truncate_page_text)
            .unwrap_or_default(),
        elements: observe_elements(tab).unwrap_or_default(),
        page_state: review::page_state_label(tab).map(str::to_owned),
    }
}

fn page_text_snapshot(tab: &Tab) -> Result<String> {
    let js = [
        FRAME_HELPERS,
        r##"(() => {
            const parts = [];
            for (const ctx of emissaryFrameDocuments()) {
                const text = emissaryClean(ctx.doc.body && ctx.doc.body.innerText);
                if (!text) continue;
                const frame = emissaryFrameName(ctx.path);
                parts.push(frame ? `[${frame}]\n${text}` : text);
            }
            return parts.join("\n\n");
        })()"##,
    ]
    .join("\n");
    Ok(value_to_string(
        tab.evaluate(&js, false)?.value.unwrap_or(Value::Null),
    ))
}

fn observe_elements(tab: &Tab) -> Result<Vec<ElementRef>> {
    let js = [
        FRAME_HELPERS,
        r##"(() => {
        let nextId = Number(document.documentElement.dataset.emissaryNextRef || "1");
        const selector = [
            "button",
            "a[href]",
            "input:not([type='hidden'])",
            "textarea",
            "select",
            "[role='button']",
            "[role='link']",
            "[contenteditable='true']",
            "[onclick]",
            "label"
        ].join(",");

        function textFor(el) {
            return [
                el.innerText,
                el.getAttribute("aria-label"),
                el.getAttribute("placeholder"),
                el.getAttribute("title"),
                el.getAttribute("value"),
                el.name,
                el.id
            ].filter(Boolean).join(" ").replace(/\s+/g, " ").trim();
        }

        function kindFor(el) {
            const tag = el.tagName.toLowerCase();
            const type = (el.getAttribute("type") || "").toLowerCase();
            if (tag === "input" || tag === "textarea" || el.isContentEditable) return "input";
            if (tag === "select") return "select";
            if (tag === "a") return "link";
            if (tag === "button" || type === "button" || type === "submit" || el.getAttribute("role") === "button") return "button";
            return "clickable";
        }

        const out = [];
        const seen = new Set();
        for (const ctx of emissaryFrameDocuments()) {
            if (ctx.frameElement && !emissaryVisible(ctx.frameElement)) continue;
            for (const el of ctx.doc.querySelectorAll(selector)) {
                if (seen.has(el) || !emissaryVisible(el)) continue;
                seen.add(el);
                const label = textFor(el);
                const kind = kindFor(el);
                if (!label && kind !== "input" && kind !== "select") continue;
                if (!el.getAttribute("data-emissary-ref")) {
                    el.setAttribute("data-emissary-ref", `e${nextId++}`);
                }
                out.push({
                    ref: el.getAttribute("data-emissary-ref"),
                    kind,
                    label: (label || `${el.tagName.toLowerCase()} ${el.name || el.id || ""}`).slice(0, 180),
                    tag: el.tagName.toLowerCase(),
                    frame: emissaryFrameName(ctx.path)
                });
                if (out.length >= 80) break;
            }
            if (out.length >= 80) break;
        }
        document.documentElement.dataset.emissaryNextRef = String(nextId);
        return JSON.stringify(out);
    })()"##,
    ]
    .join("\n");

    let value = tab.evaluate(&js, true)?.value.unwrap_or(Value::Null);
    Ok(serde_json::from_str(&value_to_string(value)).unwrap_or_default())
}

fn element_ref_details(tab: &Tab, ref_id: &str) -> Result<String> {
    let ref_json = serde_json::to_string(ref_id)?;
    let js = format!(
        r##"{FRAME_HELPERS}
        (() => {{
            const refId = {ref_json};
            const match = emissaryFindRef(refId);
            if (!match) return "";
            const el = match.el;
            const form = el.closest("form");
            return [
                el.innerText,
                el.textContent,
                el.getAttribute("aria-label"),
                el.getAttribute("placeholder"),
                el.getAttribute("title"),
                el.getAttribute("value"),
                el.id,
                el.name,
                form && form.innerText,
                form && form.getAttribute("aria-label")
            ].filter(Boolean).join("\n").slice(0, 2000);
        }})()"##
    );
    Ok(tab
        .evaluate(&js, false)?
        .value
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default())
}

fn click_by_ref(tab: &Tab, ref_id: &str) -> Result<Value> {
    let ref_json = serde_json::to_string(ref_id)?;
    let js = format!(
        r##"{FRAME_HELPERS}
        (() => {{
            const refId = {ref_json};
            const match = emissaryFindRef(refId);
            if (!match) return JSON.stringify({{ clicked: false, ref: refId, reason: "unknown element ref; call observe again" }});
            const el = match.el;
            emissaryScrollIntoView(match, el);
            el.click();
            return JSON.stringify({{
                clicked: true,
                ref: refId,
                frame: emissaryFrameName(match.path),
                label: (el.innerText || el.getAttribute("aria-label") || el.getAttribute("placeholder") || "").trim().slice(0, 200),
                tag: el.tagName.toLowerCase()
            }});
        }})()"##
    );
    let value = tab.evaluate(&js, true)?.value.unwrap_or(Value::Null);
    let value = parse_json_string(value)?;
    if value
        .get("clicked")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        thread::sleep(Duration::from_millis(750));
        Ok(value)
    } else {
        bail!(
            "{}",
            value
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("clickRef failed")
        )
    }
}

fn type_by_ref(tab: &Tab, ref_id: &str, text: &str) -> Result<Value> {
    let ref_json = serde_json::to_string(ref_id)?;
    let text_json = serde_json::to_string(text)?;
    let js = format!(
        r##"{FRAME_HELPERS}
        (() => {{
            const refId = {ref_json};
            const text = {text_json};
            const match = emissaryFindRef(refId);
            if (!match) return JSON.stringify({{ typed: false, ref: refId, reason: "unknown element ref; call observe again" }});
            const el = match.el;
            emissaryScrollIntoView(match, el);
            el.focus();
            if ("value" in el) {{
                el.value = text;
                el.dispatchEvent(new InputEvent("input", {{ bubbles: true, inputType: "insertText", data: text }}));
                el.dispatchEvent(new Event("change", {{ bubbles: true }}));
            }} else {{
                el.textContent = text;
                el.dispatchEvent(new InputEvent("input", {{ bubbles: true, inputType: "insertText", data: text }}));
            }}
            return JSON.stringify({{ typed: true, ref: refId, frame: emissaryFrameName(match.path), tag: el.tagName.toLowerCase() }});
        }})()"##
    );
    let value = tab.evaluate(&js, true)?.value.unwrap_or(Value::Null);
    let value = parse_json_string(value)?;
    if value.get("typed").and_then(Value::as_bool).unwrap_or(false) {
        Ok(value)
    } else {
        bail!(
            "{}",
            value
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("typeRef failed")
        )
    }
}

fn element_ref_is_credential_field(tab: &Tab, ref_id: &str) -> Result<bool> {
    let ref_json = serde_json::to_string(ref_id)?;
    let js = format!(
        r##"{FRAME_HELPERS}
        (() => {{
            const refId = {ref_json};
            const match = emissaryFindRef(refId);
            if (!match) return false;
            const el = match.el;
            const autocomplete = (el.getAttribute("autocomplete") || "").toLowerCase();
            if (/^(cc-|shipping |billing )/.test(autocomplete)) return true;
            const hay = [
                el.getAttribute("name"),
                el.getAttribute("id"),
                el.getAttribute("aria-label"),
                el.getAttribute("placeholder")
            ].filter(Boolean).join(" ").toLowerCase();
            return /cc-|card|cvc|cvv|security code|expiry|expiration|postal|postcode|zip|address|street|city|state|province|country|phone|email/.test(hay);
        }})()"##
    );
    Ok(tab
        .evaluate(&js, false)?
        .value
        .and_then(|value| value.as_bool())
        .unwrap_or(false))
}

fn value_to_string(value: Value) -> String {
    match value {
        Value::String(text) => text,
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn parse_json_string(value: Value) -> Result<Value> {
    let text = value_to_string(value);
    Ok(serde_json::from_str(&text)?)
}

fn truncate_page_text(text: String) -> String {
    const MAX_CHARS: usize = 12_000;
    if text.len() <= MAX_CHARS {
        return text;
    }

    let mut end = MAX_CHARS;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [truncated, {} chars total]", &text[..end], text.len())
}

fn block_sensitive_click(tab: &Tab, selector: &str) -> Result<()> {
    let details = clickable_details(tab, selector)?;
    if is_sensitive_submit(&details) {
        return Err(review::handoff_required(HandoffReason::sensitive_submit(
            details,
        )));
    }
    Ok(())
}

fn clickable_details(tab: &Tab, selector: &str) -> Result<String> {
    let selector_json = serde_json::to_string(selector)?;
    let js = format!(
        r#"(() => {{
            const el = document.querySelector({selector_json});
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

pub fn outcome_to_json(outcome: RunOutcome) -> Value {
    match outcome {
        RunOutcome::Success(success) => json!(success),
        RunOutcome::Failed(failure) => json!(failure),
        RunOutcome::NeedsHuman {
            completed,
            results,
            handoff,
        } => {
            let mut handoff = serde_json::to_value(handoff).unwrap_or(Value::Null);
            if let Some(object) = handoff.as_object_mut() {
                object.insert("completed".to_owned(), json!(completed));
                object.insert(
                    "results".to_owned(),
                    serde_json::to_value(results).unwrap_or(Value::Null),
                );
            }
            handoff
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Action, RunRequest, enrich_selector_error, truncate_page_text};

    #[test]
    fn truncates_long_page_text() {
        let text = "a".repeat(13_000);
        let truncated = truncate_page_text(text.clone());
        assert!(truncated.len() < text.len());
        assert!(truncated.contains("truncated"));
    }

    #[test]
    fn enriches_dom_query_errors() {
        let error = enrich_selector_error(
            "button:contains('Go')",
            anyhow::anyhow!("Method call error -32000: DOM Error while querying"),
        );
        assert!(error.to_string().contains("invalid CSS selector"));
    }

    #[test]
    fn parses_click_text_action() {
        let request: RunRequest =
            serde_json::from_str(r#"{"actions":[{"op":"clickText","text":"Chinese"}]}"#).unwrap();
        assert!(matches!(request.actions[0], Action::ClickText { .. }));
    }

    #[test]
    fn parses_ref_actions() {
        let request: RunRequest = serde_json::from_str(
            r#"{"actions":[{"op":"observe"},{"op":"clickRef","refId":"e1"},{"op":"typeRef","refId":"e2","text":"NG10 4HE"}]}"#,
        )
        .unwrap();
        assert!(matches!(request.actions[0], Action::Observe));
        assert!(matches!(request.actions[1], Action::ClickRef { .. }));
        assert!(matches!(request.actions[2], Action::TypeRef { .. }));
    }

    #[test]
    fn parses_payment_ref_fill_action() {
        let request: RunRequest = serde_json::from_str(
            r#"{"actions":[{"op":"fillPaymentRefs","fields":[{"refId":"e2","field":"default:card_number"},{"refId":"e3","field":"default:cvc"}]}]}"#,
        )
        .unwrap();
        assert!(matches!(request.actions[0], Action::FillPaymentRefs { .. }));
    }

    #[test]
    fn parses_auto_payment_continue_action() {
        let request: RunRequest = serde_json::from_str(
            r#"{"actions":[{"op":"autoFillPaymentAndContinue","profile":"default"}]}"#,
        )
        .unwrap();
        assert!(matches!(
            request.actions[0],
            Action::AutoFillPaymentAndContinue { .. }
        ));
    }

    #[test]
    fn parses_web_search_action() {
        let request: RunRequest =
            serde_json::from_str(r#"{"actions":[{"op":"webSearch","query":"Ada Lovelace"}]}"#)
                .unwrap();
        assert!(matches!(request.actions[0], Action::WebSearch { .. }));
    }

    #[test]
    fn parses_screenshot_action() {
        let request: RunRequest =
            serde_json::from_str(r#"{"actions":[{"op":"screenshot","selector":"img.product"}]}"#)
                .unwrap();
        assert!(matches!(request.actions[0], Action::Screenshot { .. }));
    }

    #[test]
    fn parses_address_fill_actions() {
        let request: RunRequest = serde_json::from_str(
            r##"{"actions":[{"op":"fillAddress","profile":"default","kind":"billing"},{"op":"fillAddressField","selector":"#zip","field":"default:shipping.postal_code"}]}"##,
        )
        .unwrap();
        assert!(matches!(request.actions[0], Action::FillAddress { .. }));
        assert!(matches!(
            request.actions[1],
            Action::FillAddressField { .. }
        ));
    }

    #[test]
    fn parses_json_actions() {
        let request: RunRequest = serde_json::from_str(
            r##"{
                "actions": [
                    { "op": "navigate", "url": "https://example.com" },
                    { "op": "click", "selector": "#checkout" },
                    { "op": "fillPayment", "profile": "default" },
                    { "op": "autoFillPaymentAndContinue", "profile": "default" },
                    { "op": "review" }
                ]
            }"##,
        )
        .unwrap();
        assert_eq!(request.actions.len(), 5);
        assert!(matches!(request.actions[1], Action::Click { .. }));
    }
}
