use anyhow::{Context, Result, anyhow, bail};
use headless_chrome::Tab;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

const PAYMENT_FILE_ENV: &str = "PAYMENT_FILE";
const DEFAULT_PAYMENT_FILE: &str = ".agent-runtime/payment.json";

const DEFAULT_PAYMENT_TEMPLATE: &str = r#"{
  "default": {
    "card_number": "4242424242424242",
    "exp_month": "12",
    "exp_year": "2028",
    "cvc": "123",
    "name": "Jane Doe",
    "postal_code": "94107"
  }
}
"#;

const CARD_NUMBER_SELECTORS: &[&str] = &[
    r#"[autocomplete="cc-number"]"#,
    r#"input[name*="cardnumber" i]"#,
    r#"input[name*="card-number" i]"#,
    r#"input[id*="cardnumber" i]"#,
    r#"input[data-elements-stable-field-name="cardNumber"]"#,
];

const EXP_COMBINED_SELECTORS: &[&str] = &[
    r#"[autocomplete="cc-exp"]"#,
    r#"input[name*="exp-date" i]"#,
    r#"input[name*="expiration" i]"#,
    r#"input[placeholder*="MM" i]"#,
];

const EXP_MONTH_SELECTORS: &[&str] = &[
    r#"[autocomplete="cc-exp-month"]"#,
    r#"input[name*="exp-month" i]"#,
    r#"input[name*="ccmonth" i]"#,
];

const EXP_YEAR_SELECTORS: &[&str] = &[
    r#"[autocomplete="cc-exp-year"]"#,
    r#"input[name*="exp-year" i]"#,
    r#"input[name*="ccyear" i]"#,
];

const CVC_SELECTORS: &[&str] = &[
    r#"[autocomplete="cc-csc"]"#,
    r#"input[name*="cvc" i]"#,
    r#"input[name*="cvv" i]"#,
    r#"input[name*="security" i]"#,
    r#"input[id*="cvc" i]"#,
    r#"input[id*="cvv" i]"#,
];

const NAME_SELECTORS: &[&str] = &[
    r#"[autocomplete="cc-name"]"#,
    r#"input[name*="cardholder" i]"#,
    r#"input[name*="name-on-card" i]"#,
];

const POSTAL_SELECTORS: &[&str] = &[
    r#"[autocomplete="postal-code"]"#,
    r#"input[name*="postal" i]"#,
    r#"input[name*="zip" i]"#,
];

const SUBMIT_PATTERNS: &[&str] = &[
    "place order",
    "place your order",
    "pay now",
    "buy now",
    "order now",
    "confirm purchase",
    "complete purchase",
    "complete order",
    "submit order",
    "confirm order",
    "complete payment",
    "submit payment",
    "purchase now",
];

const NAVIGATE_PATTERNS: &[&str] = &[
    "proceed to payment",
    "proceed to checkout",
    "continue to payment",
    "continue to checkout",
    "go to checkout",
    "view checkout",
];

const SAFE_PAYMENT_CONTINUE_PATTERNS: &[&str] = &[
    "continue",
    "next",
    "checkout",
    "review order",
    "review your order",
    "confirm details",
    "use this card",
    "save card",
    "save and continue",
];

pub struct SecretString(String);

impl SecretString {
    fn new(value: String) -> Self {
        Self(value)
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        self.0.clear();
    }
}

#[derive(Clone, Deserialize)]
struct PaymentProfile {
    card_number: String,
    exp_month: String,
    exp_year: String,
    cvc: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    postal_code: Option<String>,
}

#[derive(Default)]
pub struct PaymentVault {
    profiles: HashMap<String, PaymentProfile>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct PaymentFieldMapping {
    /// Input/select element ref returned by observe/current page elements.
    #[serde(rename = "refId")]
    pub ref_id: String,
    /// Vault credential ID, e.g. default:card_number, default:exp, default:cvc, default:name, or default:postal_code.
    pub field: String,
}

#[derive(Debug, Deserialize)]
struct PaymentContinueCandidate {
    #[serde(rename = "refId")]
    ref_id: String,
    label: String,
    details: String,
    tag: String,
}

impl PaymentVault {
    pub fn load() -> Result<Self> {
        let path = payment_file_path();
        if !path.exists() {
            create_default_payment_file(&path)?;
            eprintln!(
                "created {} with a starter `default` profile (test card placeholders)",
                path.display()
            );
            eprintln!("edit it with your real card details before checkout");
        }

        warn_if_world_readable(&path)?;
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read payment file {}", path.display()))?;
        let profiles = serde_json::from_str::<HashMap<String, PaymentProfile>>(&raw)
            .with_context(|| format!("failed to parse payment file {}", path.display()))?;
        Ok(Self { profiles })
    }

    pub fn payment_file_path() -> PathBuf {
        payment_file_path()
    }

    pub fn keys(&self) -> Vec<String> {
        let mut keys = self.profiles.keys().cloned().collect::<Vec<_>>();
        keys.sort();
        keys
    }

    pub fn fill_payment(tab: &Tab, vault: &PaymentVault, profile_key: &str) -> Result<Value> {
        let profile = vault
            .profiles
            .get(profile_key)
            .with_context(|| format!("unknown payment profile `{profile_key}`"))?;

        let mut filled = Vec::new();

        if let Some(selector) = find_visible_selector(tab, CARD_NUMBER_SELECTORS)? {
            inject_into_selector(tab, &selector, &profile.card_number)?;
            filled.push("card_number");
        } else {
            bail!("payment form missing field: card_number");
        }

        if let Some(selector) = find_visible_selector(tab, EXP_COMBINED_SELECTORS)? {
            inject_into_selector(tab, &selector, &format_exp_combined(profile)?)?;
            filled.push("exp");
        } else {
            let month = find_visible_selector(tab, EXP_MONTH_SELECTORS)?;
            let year = find_visible_selector(tab, EXP_YEAR_SELECTORS)?;
            match (month, year) {
                (Some(month_selector), Some(year_selector)) => {
                    inject_into_selector(tab, &month_selector, &profile.exp_month)?;
                    inject_into_selector(tab, &year_selector, &format_exp_year(profile)?)?;
                    filled.push("exp");
                }
                _ => bail!("payment form missing field: exp"),
            }
        }

        if let Some(selector) = find_visible_selector(tab, CVC_SELECTORS)? {
            inject_into_selector(tab, &selector, &profile.cvc)?;
            filled.push("cvc");
        } else {
            bail!("payment form missing field: cvc");
        }

        if let Some(name) = &profile.name
            && let Some(selector) = find_visible_selector(tab, NAME_SELECTORS)?
        {
            inject_into_selector(tab, &selector, name)?;
            filled.push("name");
        }

        if let Some(postal_code) = &profile.postal_code
            && let Some(selector) = find_visible_selector(tab, POSTAL_SELECTORS)?
        {
            inject_into_selector(tab, &selector, postal_code)?;
            filled.push("postal_code");
        }

        Ok(json!({
            "filled_payment": profile_key,
            "fields": filled,
        }))
    }

    pub fn fill_payment_refs(
        tab: &Tab,
        vault: &PaymentVault,
        mappings: &[PaymentFieldMapping],
    ) -> Result<Value> {
        if mappings.is_empty() {
            bail!("fillPaymentRefs requires at least one field mapping");
        }

        let mut filled = Vec::new();
        for mapping in mappings {
            let (profile_key, field_name) = parse_field_ref(&mapping.field)?;
            let value = vault.secret(profile_key, field_name)?;
            let details = inject_into_ref(tab, &mapping.ref_id, value.expose())?;
            filled.push(json!({
                "refId": mapping.ref_id.as_str(),
                "field": format!("{profile_key}:{field_name}"),
                "tag": details.get("tag").cloned().unwrap_or(Value::Null),
            }));
        }

        Ok(json!({
            "filled_payment_refs": filled,
        }))
    }

    pub fn fill_payment_field(
        tab: &Tab,
        vault: &PaymentVault,
        css: &str,
        field_ref: &str,
    ) -> Result<Value> {
        let (profile_key, field_name) = parse_field_ref(field_ref)?;
        let value = vault.secret(profile_key, field_name)?;
        tab.wait_for_element(css)?;
        inject_into_selector(tab, css, value.expose())?;
        Ok(json!({
            "filled": format!("{profile_key}:{field_name}"),
            "into": css,
        }))
    }

    pub fn fill_payment_and_continue(
        tab: &Tab,
        vault: &PaymentVault,
        profile_key: &str,
    ) -> Result<Value> {
        let filled = Self::fill_payment(tab, vault, profile_key)?;
        let candidates = payment_continue_candidates(tab)?;
        let selected = candidates
            .iter()
            .find(|candidate| {
                is_safe_payment_continue(&candidate.label)
                    && !is_sensitive_submit(&candidate.details)
            })
            .ok_or_else(|| continue_selection_error(&candidates))?;

        let clicked = click_payment_continue_ref(tab, &selected.ref_id)?;
        thread::sleep(Duration::from_millis(750));

        Ok(json!({
            "filled_payment": filled.get("filled_payment").cloned().unwrap_or_else(|| json!(profile_key)),
            "fields": filled.get("fields").cloned().unwrap_or_else(|| json!([])),
            "continued": {
                "label": selected.label,
                "tag": selected.tag,
                "clicked": clicked.get("clicked").cloned().unwrap_or_else(|| json!(true)),
            }
        }))
    }
}

impl PaymentVault {
    fn secret(&self, profile_key: &str, field_name: &str) -> Result<SecretString> {
        let profile = self
            .profiles
            .get(profile_key)
            .with_context(|| format!("unknown payment profile `{profile_key}`"))?;

        let value = match field_name {
            "card_number" => profile.card_number.clone(),
            "exp_month" => profile.exp_month.clone(),
            "exp_year" => profile.exp_year.clone(),
            "exp" => format_exp_combined(profile)?,
            "cvc" => profile.cvc.clone(),
            "name" => profile
                .name
                .clone()
                .ok_or_else(|| anyhow!("payment profile `{profile_key}` has no name"))?,
            "postal_code" => profile
                .postal_code
                .clone()
                .ok_or_else(|| anyhow!("payment profile `{profile_key}` has no postal_code"))?,
            _ => bail!("unknown payment field `{field_name}`"),
        };

        Ok(SecretString::new(value))
    }
}

pub fn is_sensitive_submit(details: &str) -> bool {
    let lower = details.to_lowercase();

    if NAVIGATE_PATTERNS
        .iter()
        .any(|pattern| lower.contains(pattern))
    {
        return false;
    }

    if lower.contains("checkout")
        && !SUBMIT_PATTERNS
            .iter()
            .any(|pattern| lower.contains(pattern))
    {
        return false;
    }

    if SUBMIT_PATTERNS
        .iter()
        .any(|pattern| lower.contains(pattern))
    {
        return true;
    }

    let trimmed = lower.trim();
    trimmed == "pay" || trimmed.starts_with("pay ") || trimmed.starts_with("pay\n")
}

pub fn is_safe_payment_continue(details: &str) -> bool {
    if is_sensitive_submit(details) {
        return false;
    }

    let lower = details.to_lowercase();
    SAFE_PAYMENT_CONTINUE_PATTERNS
        .iter()
        .any(|pattern| lower.contains(pattern))
}

pub fn block_type_on_payment_field(tab: &Tab, css: &str) -> Result<()> {
    if element_is_payment_field(tab, css)? {
        bail!("payment field must use fill_payment or fill_payment_field");
    }
    Ok(())
}

fn continue_selection_error(candidates: &[PaymentContinueCandidate]) -> anyhow::Error {
    let final_labels = candidates
        .iter()
        .filter(|candidate| {
            is_sensitive_submit(&candidate.label) || is_sensitive_submit(&candidate.details)
        })
        .map(|candidate| candidate.label.trim())
        .filter(|label| !label.is_empty())
        .take(3)
        .collect::<Vec<_>>();

    if final_labels.is_empty() {
        anyhow!("payment filled, but no safe continue/next/checkout control was visible")
    } else {
        anyhow!(
            "payment filled, but only final submit controls were visible: {}",
            final_labels.join(", ")
        )
    }
}

fn payment_continue_candidates(tab: &Tab) -> Result<Vec<PaymentContinueCandidate>> {
    let js = r##"(() => {
        function visible(el) {
            const rect = el.getBoundingClientRect();
            const style = window.getComputedStyle(el);
            return rect.width > 0 &&
                rect.height > 0 &&
                rect.bottom >= 0 &&
                rect.right >= 0 &&
                rect.top <= innerHeight &&
                rect.left <= innerWidth &&
                style.visibility !== "hidden" &&
                style.display !== "none" &&
                style.opacity !== "0" &&
                !el.disabled &&
                el.getAttribute("aria-disabled") !== "true";
        }

        function clean(text) {
            return String(text || "").replace(/\s+/g, " ").trim();
        }

        function labelFor(el) {
            const bits = [
                el.innerText,
                el.textContent,
                el.getAttribute("aria-label"),
                el.getAttribute("title"),
                el.getAttribute("value"),
                el.getAttribute("name"),
                el.id
            ].map(clean).filter(Boolean);
            return bits[0] || "";
        }

        const candidates = [];
        let nextRef = 1;
        for (const el of document.querySelectorAll("button, a[href], input[type='button'], input[type='submit'], [role='button']")) {
            if (!visible(el)) continue;
            const label = labelFor(el);
            if (!label) continue;
            const refId = `pc${nextRef++}`;
            el.setAttribute("data-emissary-payment-continue-ref", refId);
            const form = el.closest("form");
            const details = [
                label,
                el.getAttribute("aria-label"),
                el.getAttribute("title"),
                el.getAttribute("value"),
                el.getAttribute("name"),
                el.id,
                form && form.innerText
            ].map(clean).filter(Boolean).join("\n").slice(0, 2000);
            candidates.push({
                refId,
                label,
                details,
                tag: el.tagName.toLowerCase()
            });
        }
        return JSON.stringify(candidates);
    })()"##;
    let raw = tab.evaluate(js, true)?.value.unwrap_or(Value::Null);
    Ok(serde_json::from_str(&value_to_string(raw))?)
}

fn click_payment_continue_ref(tab: &Tab, ref_id: &str) -> Result<Value> {
    let ref_json = serde_json::to_string(ref_id)?;
    let js = format!(
        r##"(() => {{
            const refId = {ref_json};
            const el = document.querySelector(`[data-emissary-payment-continue-ref="${{refId}}"]`);
            if (!el) {{
                return JSON.stringify({{
                    clicked: false,
                    reason: "safe payment continue control disappeared"
                }});
            }}
            el.scrollIntoView({{ block: "center", inline: "center" }});
            el.click();
            return JSON.stringify({{
                clicked: true,
                label: (el.innerText || el.textContent || el.getAttribute("value") || "").trim(),
                tag: el.tagName.toLowerCase()
            }});
        }})()"##
    );
    let raw = tab.evaluate(&js, true)?.value.unwrap_or(Value::Null);
    let details: Value = serde_json::from_str(&value_to_string(raw))?;
    if details
        .get("clicked")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        Ok(details)
    } else {
        bail!(
            "{}",
            details
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("auto payment continue failed")
        )
    }
}

fn parse_field_ref(field_ref: &str) -> Result<(&str, &str)> {
    match field_ref.split_once(':') {
        Some((profile_key, field_name)) if !profile_key.is_empty() && !field_name.is_empty() => {
            Ok((profile_key, field_name))
        }
        _ => Ok(("default", field_ref)),
    }
}

fn format_exp_combined(profile: &PaymentProfile) -> Result<String> {
    let month = normalize_month(&profile.exp_month)?;
    let year = normalize_year(&profile.exp_year)?;
    Ok(format!("{month}/{year}"))
}

fn format_exp_year(profile: &PaymentProfile) -> Result<String> {
    normalize_year(&profile.exp_year)
}

fn normalize_month(raw: &str) -> Result<String> {
    let digits = raw.trim();
    if digits.len() == 1 {
        Ok(format!("0{digits}"))
    } else if digits.len() == 2 && digits.chars().all(|ch| ch.is_ascii_digit()) {
        Ok(digits.to_owned())
    } else {
        bail!("exp_month must be one or two digits");
    }
}

fn normalize_year(raw: &str) -> Result<String> {
    let digits = raw.trim();
    match digits.len() {
        2 if digits.chars().all(|ch| ch.is_ascii_digit()) => Ok(digits.to_owned()),
        4 if digits.chars().all(|ch| ch.is_ascii_digit()) => Ok(digits[2..].to_owned()),
        _ => bail!("exp_year must be two or four digits"),
    }
}

fn value_to_string(value: Value) -> String {
    match value {
        Value::String(text) => text,
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn payment_file_path() -> PathBuf {
    std::env::var(PAYMENT_FILE_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(DEFAULT_PAYMENT_FILE)
        })
}

fn create_default_payment_file(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, DEFAULT_PAYMENT_TEMPLATE)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn warn_if_world_readable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(path)?.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            eprintln!(
                "warning: payment file {} is readable by group/others (mode {:o}); prefer chmod 600",
                path.display(),
                mode
            );
        }
    }
    Ok(())
}

fn find_visible_selector(tab: &Tab, selectors: &[&str]) -> Result<Option<String>> {
    let selectors_json = serde_json::to_string(selectors)?;
    let js = format!(
        r#"(() => {{
            const selectors = {selectors_json};
            for (const selector of selectors) {{
                const el = document.querySelector(selector);
                if (!el) continue;
                const style = window.getComputedStyle(el);
                if (style.visibility === "hidden" || style.display === "none") continue;
                if (el.disabled) continue;
                return selector;
            }}
            return null;
        }})()"#
    );
    Ok(tab
        .evaluate(&js, false)?
        .value
        .and_then(|value| value.as_str().map(str::to_owned)))
}

fn inject_into_selector(tab: &Tab, css: &str, value: &str) -> Result<()> {
    let css_json = serde_json::to_string(css)?;
    let value_json = serde_json::to_string(value)?;
    let js = format!(
        r#"(() => {{
            const el = document.querySelector({css_json});
            if (!el) return false;
            el.focus();
            el.value = {value_json};
            el.dispatchEvent(new Event("input", {{ bubbles: true }}));
            el.dispatchEvent(new Event("change", {{ bubbles: true }}));
            return true;
        }})()"#
    );
    let injected = tab
        .evaluate(&js, false)?
        .value
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    if injected {
        Ok(())
    } else {
        bail!("failed to inject into `{css}`");
    }
}

fn inject_into_ref(tab: &Tab, ref_id: &str, value: &str) -> Result<Value> {
    let ref_json = serde_json::to_string(ref_id)?;
    let value_json = serde_json::to_string(value)?;
    let js = format!(
        r##"(() => {{
            const refId = {ref_json};
            const value = {value_json};
            const el = Array.from(document.querySelectorAll("[data-emissary-ref]"))
                .find((candidate) => candidate.dataset.emissaryRef === refId);
            if (!el) {{
                return JSON.stringify({{
                    filled: false,
                    refId,
                    reason: "unknown element ref; call observe again"
                }});
            }}
            if (el.disabled || el.readOnly) {{
                return JSON.stringify({{
                    filled: false,
                    refId,
                    reason: "element is disabled or read-only"
                }});
            }}

            el.scrollIntoView({{ block: "center", inline: "center" }});
            el.focus();
            if (el.tagName.toLowerCase() === "select") {{
                const exact = Array.from(el.options).find((option) => option.value === value);
                const byText = Array.from(el.options).find((option) =>
                    option.textContent.trim().toLowerCase() === value.toLowerCase()
                );
                const option = exact || byText;
                if (!option) {{
                    return JSON.stringify({{
                        filled: false,
                        refId,
                        reason: "no matching select option"
                    }});
                }}
                el.value = option.value;
            }} else if ("value" in el) {{
                el.value = value;
            }} else if (el.isContentEditable) {{
                el.textContent = value;
            }} else {{
                return JSON.stringify({{
                    filled: false,
                    refId,
                    reason: "element cannot receive text"
                }});
            }}
            el.dispatchEvent(new InputEvent("input", {{ bubbles: true, inputType: "insertText", data: value }}));
            el.dispatchEvent(new Event("change", {{ bubbles: true }}));
            return JSON.stringify({{
                filled: true,
                refId,
                tag: el.tagName.toLowerCase()
            }});
        }})()"##
    );
    let raw = tab.evaluate(&js, true)?.value.unwrap_or(Value::Null);
    let details: Value = serde_json::from_str(&value_to_string(raw))?;
    if details
        .get("filled")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        Ok(details)
    } else {
        bail!(
            "{}",
            details
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("fillPaymentRefs failed")
        )
    }
}

fn element_is_payment_field(tab: &Tab, css: &str) -> Result<bool> {
    let css_json = serde_json::to_string(css)?;
    let js = format!(
        r#"(() => {{
            const el = document.querySelector({css_json});
            if (!el) return false;
            const autocomplete = (el.getAttribute("autocomplete") || "").toLowerCase();
            if (autocomplete.startsWith("cc-")) return true;
            const haystack = [
                el.getAttribute("name"),
                el.getAttribute("id"),
                el.getAttribute("placeholder"),
                el.getAttribute("aria-label")
            ].filter(Boolean).join(" ").toLowerCase();
            return /card|cvc|cvv|exp|security code|postal|zip/.test(haystack);
        }})()"#
    );
    Ok(tab
        .evaluate(&js, false)?
        .value
        .and_then(|value| value.as_bool())
        .unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::{
        PaymentFieldMapping, PaymentVault, is_safe_payment_continue, is_sensitive_submit,
        parse_field_ref,
    };
    use std::fs;

    #[test]
    fn blocks_final_purchase_actions() {
        assert!(is_sensitive_submit("Place order"));
        assert!(is_sensitive_submit("Pay now"));
        assert!(is_sensitive_submit("Confirm purchase"));
        assert!(is_sensitive_submit("Pay"));
    }

    #[test]
    fn allows_checkout_navigation() {
        assert!(!is_sensitive_submit("Proceed to payment"));
        assert!(!is_sensitive_submit("Go to checkout"));
        assert!(!is_sensitive_submit("Checkout"));
    }

    #[test]
    fn classifies_safe_payment_continue_controls() {
        assert!(is_safe_payment_continue("Continue"));
        assert!(is_safe_payment_continue("Next"));
        assert!(is_safe_payment_continue("Checkout"));
        assert!(is_safe_payment_continue("Continue to review order"));
        assert!(!is_safe_payment_continue("Pay now"));
        assert!(!is_safe_payment_continue("Complete purchase"));
    }

    #[test]
    fn allows_cart_building_actions() {
        assert!(!is_sensitive_submit("Add to basket"));
        assert!(!is_sensitive_submit("Choose delivery time"));
    }

    #[test]
    fn parses_field_refs() {
        assert_eq!(parse_field_ref("default:cvc").unwrap(), ("default", "cvc"));
        assert_eq!(parse_field_ref("cvc").unwrap(), ("default", "cvc"));
    }

    #[test]
    fn parses_payment_field_mapping_refs() {
        let mapping: PaymentFieldMapping =
            serde_json::from_str(r#"{"refId":"e4","field":"default:card_number"}"#).unwrap();
        assert_eq!(mapping.ref_id, "e4");
        assert_eq!(mapping.field, "default:card_number");
    }

    #[test]
    fn creates_default_payment_file_when_missing() {
        let dir = std::env::temp_dir().join(format!(
            "emissary-payment-create-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("payment.json");

        let previous = std::env::var("PAYMENT_FILE").ok();
        unsafe {
            std::env::set_var("PAYMENT_FILE", path.to_string_lossy().to_string());
        }
        let vault = PaymentVault::load().unwrap();
        assert!(path.exists());
        assert_eq!(vault.keys(), vec!["default".to_owned()]);
        match previous {
            Some(value) => unsafe { std::env::set_var("PAYMENT_FILE", value) },
            None => unsafe { std::env::remove_var("PAYMENT_FILE") },
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn loads_payment_profiles_from_json() {
        let dir =
            std::env::temp_dir().join(format!("emissary-payment-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("payment.json");
        fs::write(
            &path,
            r#"{
                "default": {
                    "card_number": "4242424242424242",
                    "exp_month": "12",
                    "exp_year": "2028",
                    "cvc": "123"
                }
            }"#,
        )
        .unwrap();

        let previous = std::env::var("PAYMENT_FILE").ok();
        unsafe {
            std::env::set_var("PAYMENT_FILE", path.to_string_lossy().to_string());
        }
        let vault = PaymentVault::load().unwrap();
        assert_eq!(vault.keys(), vec!["default".to_owned()]);
        match previous {
            Some(value) => unsafe { std::env::set_var("PAYMENT_FILE", value) },
            None => unsafe { std::env::remove_var("PAYMENT_FILE") },
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
