use anyhow::{Context, Result, anyhow, bail};
use headless_chrome::Tab;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

const PAYMENT_SOURCE_ENV: &str = "PAYMENT_SOURCE";
const PAYMENT_FILE_ENV: &str = "PAYMENT_FILE";
const DEFAULT_PAYMENT_FILE: &str = ".agent-runtime/payment.json";
const PAYMENT_SOURCE_FILE: &str = "file";
const PAYMENT_SOURCE_1PASSWORD: &str = "1password";
const PAYMENT_SOURCE_ONEPASSWORD: &str = "onepassword";
const ONEPASSWORD_ITEM_ENV: &str = "PAYMENT_1PASSWORD_ITEM";
const ONEPASSWORD_ITEMS_ENV: &str = "PAYMENT_1PASSWORD_ITEMS";
const ONEPASSWORD_PROFILE_ENV: &str = "PAYMENT_1PASSWORD_PROFILE";
const ONEPASSWORD_VAULT_ENV: &str = "PAYMENT_1PASSWORD_VAULT";
const ONEPASSWORD_CLI_ENV: &str = "OP_CLI";
const DEFAULT_PAYMENT_PROFILE: &str = "default";

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

impl PaymentVault {
    pub fn load() -> Result<Self> {
        match payment_source()? {
            PaymentSource::File(path) => Self::load_from_file(path),
            PaymentSource::OnePassword(config) => Self::load_from_1password(config),
        }
    }

    fn load_from_file(path: PathBuf) -> Result<Self> {
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

    fn load_from_1password(config: OnePasswordConfig) -> Result<Self> {
        let mut profiles = HashMap::new();
        for (profile_key, item_ref) in &config.items {
            let raw = run_onepassword_item_get(&config, item_ref)?;
            let profile = parse_onepassword_item_json(&raw, item_ref)?;
            if profiles.insert(profile_key.clone(), profile).is_some() {
                bail!("duplicate 1Password payment profile `{profile_key}`");
            }
        }
        Ok(Self { profiles })
    }

    pub fn payment_file_path() -> PathBuf {
        payment_file_path()
    }

    pub fn configuration_hint() -> String {
        if onepassword_source_enabled() {
            format!("set {ONEPASSWORD_ITEM_ENV} or {ONEPASSWORD_ITEMS_ENV}")
        } else {
            format!("edit {}", payment_file_path().display())
        }
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

pub fn block_type_on_payment_field(tab: &Tab, css: &str) -> Result<()> {
    if element_is_payment_field(tab, css)? {
        bail!("payment field must use fill_payment or fill_payment_field");
    }
    Ok(())
}

fn parse_field_ref(field_ref: &str) -> Result<(&str, &str)> {
    match field_ref.split_once(':') {
        Some((profile_key, field_name)) if !profile_key.is_empty() && !field_name.is_empty() => {
            Ok((profile_key, field_name))
        }
        _ => Ok(("default", field_ref)),
    }
}

enum PaymentSource {
    File(PathBuf),
    OnePassword(OnePasswordConfig),
}

struct OnePasswordConfig {
    cli: String,
    vault: Option<String>,
    items: Vec<(String, String)>,
}

#[derive(Deserialize)]
struct OnePasswordItem {
    #[serde(default)]
    fields: Vec<OnePasswordField>,
}

#[derive(Deserialize)]
struct OnePasswordField {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    value: Value,
}

#[derive(Default)]
struct PartialPaymentProfile {
    card_number: Option<String>,
    exp_month: Option<String>,
    exp_year: Option<String>,
    cvc: Option<String>,
    name: Option<String>,
    postal_code: Option<String>,
}

fn payment_source() -> Result<PaymentSource> {
    let source = env::var(PAYMENT_SOURCE_ENV).unwrap_or_else(|_| PAYMENT_SOURCE_FILE.to_owned());
    match source.trim().to_ascii_lowercase().as_str() {
        "" | PAYMENT_SOURCE_FILE => Ok(PaymentSource::File(payment_file_path())),
        PAYMENT_SOURCE_1PASSWORD | PAYMENT_SOURCE_ONEPASSWORD => {
            Ok(PaymentSource::OnePassword(onepassword_config()?))
        }
        other => bail!(
            "unsupported {PAYMENT_SOURCE_ENV} `{other}`; expected `{PAYMENT_SOURCE_FILE}` or `{PAYMENT_SOURCE_1PASSWORD}`"
        ),
    }
}

fn onepassword_source_enabled() -> bool {
    env::var(PAYMENT_SOURCE_ENV)
        .map(|source| {
            let source = source.trim().to_ascii_lowercase();
            source == PAYMENT_SOURCE_1PASSWORD || source == PAYMENT_SOURCE_ONEPASSWORD
        })
        .unwrap_or(false)
}

fn onepassword_config() -> Result<OnePasswordConfig> {
    let items = parse_onepassword_items(
        env::var(ONEPASSWORD_ITEMS_ENV).ok().as_deref(),
        env::var(ONEPASSWORD_ITEM_ENV).ok().as_deref(),
        env::var(ONEPASSWORD_PROFILE_ENV).ok().as_deref(),
    )?;
    let cli = env::var(ONEPASSWORD_CLI_ENV).unwrap_or_else(|_| "op".to_owned());
    let vault = env::var(ONEPASSWORD_VAULT_ENV)
        .ok()
        .map(|vault| vault.trim().to_owned())
        .filter(|vault| !vault.is_empty());

    Ok(OnePasswordConfig { cli, vault, items })
}

fn parse_onepassword_items(
    items_json: Option<&str>,
    single_item: Option<&str>,
    single_profile: Option<&str>,
) -> Result<Vec<(String, String)>> {
    match (items_json, single_item) {
        (Some(_), Some(_)) => {
            bail!("set only one of {ONEPASSWORD_ITEMS_ENV} or {ONEPASSWORD_ITEM_ENV}")
        }
        (Some(raw), None) => {
            let map = serde_json::from_str::<HashMap<String, String>>(raw).with_context(|| {
                format!(
                    "{ONEPASSWORD_ITEMS_ENV} must be a JSON object of profile keys to item refs"
                )
            })?;
            let mut items = Vec::new();
            for (profile_key, item_ref) in map {
                let profile_key = profile_key.trim().to_owned();
                let item_ref = item_ref.trim().to_owned();
                if profile_key.is_empty() || item_ref.is_empty() {
                    bail!("{ONEPASSWORD_ITEMS_ENV} cannot contain empty profile keys or item refs");
                }
                items.push((profile_key, item_ref));
            }
            if items.is_empty() {
                bail!("{ONEPASSWORD_ITEMS_ENV} must contain at least one payment profile");
            }
            items.sort_by(|left, right| left.0.cmp(&right.0));
            Ok(items)
        }
        (None, Some(item_ref)) => {
            let item_ref = item_ref.trim();
            if item_ref.is_empty() {
                bail!("{ONEPASSWORD_ITEM_ENV} cannot be empty");
            }
            let profile_key = single_profile
                .map(str::trim)
                .filter(|profile| !profile.is_empty())
                .unwrap_or(DEFAULT_PAYMENT_PROFILE);
            Ok(vec![(profile_key.to_owned(), item_ref.to_owned())])
        }
        (None, None) => {
            bail!(
                "{PAYMENT_SOURCE_ENV}=1password requires {ONEPASSWORD_ITEM_ENV} or {ONEPASSWORD_ITEMS_ENV}"
            )
        }
    }
}

fn run_onepassword_item_get(config: &OnePasswordConfig, item_ref: &str) -> Result<String> {
    let mut command = Command::new(&config.cli);
    command.args(["item", "get", item_ref, "--format", "json", "--reveal"]);
    if let Some(vault) = &config.vault {
        command.args(["--vault", vault]);
    }

    let output = command
        .output()
        .with_context(|| format!("failed to run 1Password CLI `{}`", config.cli))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = stderr.trim();
        if message.is_empty() {
            bail!("1Password CLI failed to read payment item `{item_ref}`");
        }
        bail!("1Password CLI failed to read payment item `{item_ref}`: {message}");
    }

    String::from_utf8(output.stdout).context("1Password CLI returned non-UTF-8 item JSON")
}

fn parse_onepassword_item_json(raw: &str, item_ref: &str) -> Result<PaymentProfile> {
    let item = serde_json::from_str::<OnePasswordItem>(raw)
        .with_context(|| format!("failed to parse 1Password item `{item_ref}` JSON"))?;
    let mut profile = PartialPaymentProfile::default();

    for field in item.fields {
        let Some(value) = onepassword_field_value(&field) else {
            continue;
        };
        let keys = [field.id.as_deref(), field.label.as_deref()];
        for key in keys.into_iter().flatten().map(normalize_1password_key) {
            match key.as_str() {
                "cardnumber" | "ccnum" | "creditcardnumber" | "number" => {
                    set_if_missing(&mut profile.card_number, sanitize_card_number(&value));
                }
                "expirationdate" | "expirydate" | "expiration" | "expiry" | "expires" | "exp" => {
                    if let Some((month, year)) = parse_expiry_parts(&value)? {
                        set_if_missing(&mut profile.exp_month, month);
                        set_if_missing(&mut profile.exp_year, year);
                    }
                }
                "expirationmonth" | "expirymonth" | "expmonth" | "expmonth2" | "month" => {
                    set_if_missing(&mut profile.exp_month, value.trim().to_owned());
                }
                "expirationyear" | "expiryyear" | "expyear" | "year" => {
                    set_if_missing(&mut profile.exp_year, value.trim().to_owned());
                }
                "cvc" | "cvv" | "securitycode" | "verificationnumber" => {
                    set_if_missing(&mut profile.cvc, value.trim().to_owned());
                }
                "name" | "cardholder" | "cardholdername" | "nameoncard" => {
                    set_if_missing(&mut profile.name, value.trim().to_owned());
                }
                "postalcode" | "postcode" | "zip" | "zipcode" | "billingzip"
                | "billingpostalcode" => {
                    set_if_missing(&mut profile.postal_code, value.trim().to_owned());
                }
                _ => {}
            }
        }
    }

    profile.into_payment_profile(item_ref)
}

fn onepassword_field_value(field: &OnePasswordField) -> Option<String> {
    match &field.value {
        Value::String(value) => non_empty(value),
        Value::Number(value) => non_empty(&value.to_string()),
        _ => None,
    }
}

fn non_empty(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_owned())
    }
}

fn normalize_1password_key(raw: &str) -> String {
    raw.chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn set_if_missing(target: &mut Option<String>, value: String) {
    if target.is_none() && !value.trim().is_empty() {
        *target = Some(value);
    }
}

fn sanitize_card_number(value: &str) -> String {
    let digits = value
        .chars()
        .filter(|ch| ch.is_ascii_digit())
        .collect::<String>();
    if digits.is_empty() {
        value.trim().to_owned()
    } else {
        digits
    }
}

fn parse_expiry_parts(raw: &str) -> Result<Option<(String, String)>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    let parts = trimmed
        .split(|ch: char| !ch.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if parts.len() >= 2 {
        let (month, year) = if parts[0].len() == 4 {
            (parts[1], parts[0])
        } else {
            (parts[0], parts[1])
        };
        return Ok(Some((normalize_month(month)?, normalize_year(year)?)));
    }

    let digits = trimmed
        .chars()
        .filter(|ch| ch.is_ascii_digit())
        .collect::<String>();
    match digits.len() {
        4 => Ok(Some((
            normalize_month(&digits[..2])?,
            normalize_year(&digits[2..])?,
        ))),
        6 if digits.starts_with("20") || digits.starts_with("19") => Ok(Some((
            normalize_month(&digits[4..])?,
            normalize_year(&digits[..4])?,
        ))),
        6 => Ok(Some((
            normalize_month(&digits[..2])?,
            normalize_year(&digits[2..])?,
        ))),
        _ => bail!("could not parse 1Password expiry value `{trimmed}`"),
    }
}

impl PartialPaymentProfile {
    fn into_payment_profile(self, item_ref: &str) -> Result<PaymentProfile> {
        Ok(PaymentProfile {
            card_number: self
                .card_number
                .ok_or_else(|| anyhow!("1Password item `{item_ref}` has no card number field"))?,
            exp_month: self
                .exp_month
                .ok_or_else(|| anyhow!("1Password item `{item_ref}` has no expiration month"))?,
            exp_year: self
                .exp_year
                .ok_or_else(|| anyhow!("1Password item `{item_ref}` has no expiration year"))?,
            cvc: self
                .cvc
                .ok_or_else(|| anyhow!("1Password item `{item_ref}` has no CVC/CVV field"))?,
            name: self.name,
            postal_code: self.postal_code,
        })
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
        PaymentVault, is_sensitive_submit, parse_field_ref, parse_onepassword_item_json,
        parse_onepassword_items,
    };
    use std::{fs, sync::Mutex};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

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
    fn parses_onepassword_single_item_config() {
        let items = parse_onepassword_items(None, Some("Personal Visa"), Some("primary")).unwrap();
        assert_eq!(
            items,
            vec![("primary".to_owned(), "Personal Visa".to_owned())]
        );

        let default_items = parse_onepassword_items(None, Some("Personal Visa"), None).unwrap();
        assert_eq!(
            default_items,
            vec![("default".to_owned(), "Personal Visa".to_owned())]
        );
    }

    #[test]
    fn parses_onepassword_multi_item_config() {
        let items = parse_onepassword_items(
            Some(r#"{"backup":"Backup Mastercard","default":"Personal Visa"}"#),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            items,
            vec![
                ("backup".to_owned(), "Backup Mastercard".to_owned()),
                ("default".to_owned(), "Personal Visa".to_owned())
            ]
        );
    }

    #[test]
    fn parses_standard_onepassword_credit_card_item() {
        let profile = parse_onepassword_item_json(
            r#"{
                "fields": [
                    { "id": "ccnum", "label": "number", "value": "4242 4242 4242 4242" },
                    { "id": "expiry", "label": "expiry date", "value": "12/2028" },
                    { "id": "cvv", "label": "verification number", "value": "123" },
                    { "id": "cardholder", "label": "cardholder name", "value": "Jane Doe" },
                    { "id": "zip", "label": "ZIP", "value": "94107" }
                ]
            }"#,
            "Personal Visa",
        )
        .unwrap();

        assert_eq!(profile.card_number, "4242424242424242");
        assert_eq!(profile.exp_month, "12");
        assert_eq!(profile.exp_year, "28");
        assert_eq!(profile.cvc, "123");
        assert_eq!(profile.name.as_deref(), Some("Jane Doe"));
        assert_eq!(profile.postal_code.as_deref(), Some("94107"));
    }

    #[test]
    fn parses_custom_onepassword_payment_fields() {
        let profile = parse_onepassword_item_json(
            r#"{
                "fields": [
                    { "label": "card_number", "value": "4242424242424242" },
                    { "label": "exp_month", "value": "7" },
                    { "label": "exp_year", "value": "2029" },
                    { "label": "cvc", "value": "321" },
                    { "label": "name", "value": "Jane Doe" },
                    { "label": "postal_code", "value": "94107" }
                ]
            }"#,
            "Custom Card",
        )
        .unwrap();

        assert_eq!(profile.card_number, "4242424242424242");
        assert_eq!(profile.exp_month, "7");
        assert_eq!(profile.exp_year, "2029");
        assert_eq!(profile.cvc, "321");
        assert_eq!(profile.name.as_deref(), Some("Jane Doe"));
        assert_eq!(profile.postal_code.as_deref(), Some("94107"));
    }

    #[test]
    fn creates_default_payment_file_when_missing() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!(
            "emissary-payment-create-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("payment.json");

        let previous = std::env::var("PAYMENT_FILE").ok();
        let previous_source = std::env::var("PAYMENT_SOURCE").ok();
        unsafe {
            std::env::set_var("PAYMENT_SOURCE", "file");
            std::env::set_var("PAYMENT_FILE", path.to_string_lossy().to_string());
        }
        let vault = PaymentVault::load().unwrap();
        assert!(path.exists());
        assert_eq!(vault.keys(), vec!["default".to_owned()]);
        match previous {
            Some(value) => unsafe { std::env::set_var("PAYMENT_FILE", value) },
            None => unsafe { std::env::remove_var("PAYMENT_FILE") },
        }
        match previous_source {
            Some(value) => unsafe { std::env::set_var("PAYMENT_SOURCE", value) },
            None => unsafe { std::env::remove_var("PAYMENT_SOURCE") },
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn loads_payment_profiles_from_json() {
        let _guard = ENV_LOCK.lock().unwrap();
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
        let previous_source = std::env::var("PAYMENT_SOURCE").ok();
        unsafe {
            std::env::set_var("PAYMENT_SOURCE", "file");
            std::env::set_var("PAYMENT_FILE", path.to_string_lossy().to_string());
        }
        let vault = PaymentVault::load().unwrap();
        assert_eq!(vault.keys(), vec!["default".to_owned()]);
        match previous {
            Some(value) => unsafe { std::env::set_var("PAYMENT_FILE", value) },
            None => unsafe { std::env::remove_var("PAYMENT_FILE") },
        }
        match previous_source {
            Some(value) => unsafe { std::env::set_var("PAYMENT_SOURCE", value) },
            None => unsafe { std::env::remove_var("PAYMENT_SOURCE") },
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
