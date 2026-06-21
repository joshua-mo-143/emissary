use anyhow::{Context, Result, anyhow, bail};
use headless_chrome::Tab;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::HashMap, env, process::Command};

const ONEPASSWORD_ITEM_ENV: &str = "PAYMENT_1PASSWORD_ITEM";
const ONEPASSWORD_ITEMS_ENV: &str = "PAYMENT_1PASSWORD_ITEMS";
const ONEPASSWORD_PROFILE_ENV: &str = "PAYMENT_1PASSWORD_PROFILE";
const ONEPASSWORD_ADDRESS_ITEM_ENV: &str = "PAYMENT_1PASSWORD_ADDRESS_ITEM";
const ONEPASSWORD_SHIPPING_ADDRESS_ITEM_ENV: &str = "PAYMENT_1PASSWORD_SHIPPING_ADDRESS_ITEM";
const ONEPASSWORD_BILLING_ADDRESS_ITEM_ENV: &str = "PAYMENT_1PASSWORD_BILLING_ADDRESS_ITEM";
const ONEPASSWORD_VAULT_ENV: &str = "PAYMENT_1PASSWORD_VAULT";
const ONEPASSWORD_CLI_ENV: &str = "OP_CLI";
const DEFAULT_PAYMENT_PROFILE: &str = "default";

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
    #[serde(default)]
    billing_address: Option<AddressProfile>,
    #[serde(default)]
    shipping_address: Option<AddressProfile>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct AddressProfile {
    #[serde(default)]
    full_name: Option<String>,
    #[serde(default)]
    first_name: Option<String>,
    #[serde(default)]
    last_name: Option<String>,
    #[serde(default)]
    organization: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    phone: Option<String>,
    #[serde(default)]
    address_line1: Option<String>,
    #[serde(default)]
    address_line2: Option<String>,
    #[serde(default)]
    city: Option<String>,
    #[serde(default)]
    region: Option<String>,
    #[serde(default)]
    postal_code: Option<String>,
    #[serde(default)]
    country: Option<String>,
}

#[derive(Default)]
pub struct PaymentVault {
    profiles: HashMap<String, PaymentProfile>,
}

impl PaymentVault {
    pub fn load() -> Result<Self> {
        Self::load_from_1password(onepassword_config()?)
    }

    fn load_from_1password(config: OnePasswordConfig) -> Result<Self> {
        let mut profiles = HashMap::new();
        for (profile_key, item_refs) in &config.items {
            let raw = run_onepassword_item_get(&config, &item_refs.payment)?;
            let parsed = parse_onepassword_item_json(&raw, &item_refs.payment)?;
            let mut profile = parsed.into_payment_profile(&item_refs.payment)?;

            if let Some(item_ref) = &item_refs.billing_address {
                let raw = run_onepassword_item_get(&config, item_ref)?;
                profile.billing_address = parse_onepassword_item_json(&raw, item_ref)?
                    .address_for(AddressKind::Billing)
                    .or_else(|| profile.billing_address.clone());
            }
            if let Some(item_ref) = &item_refs.shipping_address {
                let raw = run_onepassword_item_get(&config, item_ref)?;
                profile.shipping_address = parse_onepassword_item_json(&raw, item_ref)?
                    .address_for(AddressKind::Shipping)
                    .or_else(|| profile.shipping_address.clone());
            }

            if profiles.insert(profile_key.clone(), profile).is_some() {
                bail!("duplicate 1Password payment profile `{profile_key}`");
            }
        }
        Ok(Self { profiles })
    }

    pub fn configuration_hint() -> String {
        format!("set {ONEPASSWORD_ITEM_ENV} or {ONEPASSWORD_ITEMS_ENV}")
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

    pub fn fill_address(
        tab: &Tab,
        vault: &PaymentVault,
        profile_key: &str,
        kind: Option<&str>,
    ) -> Result<Value> {
        let kind = parse_address_kind(kind)?;
        let address = vault.address(profile_key, kind)?;
        let mut filled = Vec::new();

        fill_address_value(
            tab,
            kind,
            AddressField::FullName,
            full_name(address).as_deref(),
            &mut filled,
        )?;
        fill_address_value(
            tab,
            kind,
            AddressField::FirstName,
            address.first_name.as_deref(),
            &mut filled,
        )?;
        fill_address_value(
            tab,
            kind,
            AddressField::LastName,
            address.last_name.as_deref(),
            &mut filled,
        )?;
        fill_address_value(
            tab,
            kind,
            AddressField::Organization,
            address.organization.as_deref(),
            &mut filled,
        )?;
        fill_address_value(
            tab,
            kind,
            AddressField::Email,
            address.email.as_deref(),
            &mut filled,
        )?;
        fill_address_value(
            tab,
            kind,
            AddressField::Phone,
            address.phone.as_deref(),
            &mut filled,
        )?;
        fill_address_value(
            tab,
            kind,
            AddressField::AddressLine1,
            address.address_line1.as_deref(),
            &mut filled,
        )?;
        fill_address_value(
            tab,
            kind,
            AddressField::AddressLine2,
            address.address_line2.as_deref(),
            &mut filled,
        )?;
        fill_address_value(
            tab,
            kind,
            AddressField::City,
            address.city.as_deref(),
            &mut filled,
        )?;
        fill_address_value(
            tab,
            kind,
            AddressField::Region,
            address.region.as_deref(),
            &mut filled,
        )?;
        fill_address_value(
            tab,
            kind,
            AddressField::PostalCode,
            address.postal_code.as_deref(),
            &mut filled,
        )?;
        fill_address_value(
            tab,
            kind,
            AddressField::Country,
            address.country.as_deref(),
            &mut filled,
        )?;

        if filled.is_empty() {
            bail!("address form missing fillable fields for {}", kind.as_str());
        }

        Ok(json!({
            "filled_address": profile_key,
            "kind": kind.as_str(),
            "fields": filled,
        }))
    }

    pub fn fill_address_field(
        tab: &Tab,
        vault: &PaymentVault,
        css: &str,
        field_ref: &str,
    ) -> Result<Value> {
        let (profile_key, kind, field_name) = parse_address_field_ref(field_ref)?;
        let value = vault.address_secret(profile_key, kind, field_name)?;
        tab.wait_for_element(css)?;
        inject_into_selector(tab, css, value.expose())?;
        Ok(json!({
            "filled": format!("{profile_key}:{}.{}", kind.as_str(), field_name),
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

    fn address(&self, profile_key: &str, kind: AddressKind) -> Result<&AddressProfile> {
        let profile = self
            .profiles
            .get(profile_key)
            .with_context(|| format!("unknown payment profile `{profile_key}`"))?;
        let address = match kind {
            AddressKind::Billing => &profile.billing_address,
            AddressKind::Shipping => &profile.shipping_address,
        };
        address.as_ref().ok_or_else(|| {
            anyhow!(
                "payment profile `{profile_key}` has no {} address",
                kind.as_str()
            )
        })
    }

    fn address_secret(
        &self,
        profile_key: &str,
        kind: AddressKind,
        field_name: &str,
    ) -> Result<SecretString> {
        let address = self.address(profile_key, kind)?;
        let value = address_field_value(address, field_name).ok_or_else(|| {
            anyhow!(
                "payment profile `{profile_key}` has no {} address field `{field_name}`",
                kind.as_str()
            )
        })?;
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

pub fn block_type_on_credential_field(tab: &Tab, css: &str) -> Result<()> {
    if element_is_credential_field(tab, css)? {
        bail!("credential fields must use fill_payment/fill_address vault actions");
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

fn parse_address_field_ref(field_ref: &str) -> Result<(&str, AddressKind, &str)> {
    let (profile_key, field_ref) = parse_field_ref(field_ref)?;
    let Some((kind, field_name)) = field_ref.split_once('.') else {
        bail!("address field must be kind.field, e.g. default:shipping.postal_code");
    };
    let kind = parse_address_kind(Some(kind))?;
    if field_name.is_empty() {
        bail!("address field name cannot be empty");
    }
    Ok((profile_key, kind, field_name))
}

fn parse_address_kind(kind: Option<&str>) -> Result<AddressKind> {
    match kind.map(str::trim).filter(|kind| !kind.is_empty()) {
        None | Some("shipping") | Some("delivery") => Ok(AddressKind::Shipping),
        Some("billing") => Ok(AddressKind::Billing),
        Some(other) => bail!("unknown address kind `{other}`; expected `shipping` or `billing`"),
    }
}

struct OnePasswordConfig {
    cli: String,
    vault: Option<String>,
    items: Vec<(String, OnePasswordProfileRefs)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OnePasswordProfileRefs {
    payment: String,
    billing_address: Option<String>,
    shipping_address: Option<String>,
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
    address: AddressProfile,
    billing_address: AddressProfile,
    shipping_address: AddressProfile,
}

#[derive(Clone, Copy)]
enum AddressKind {
    Billing,
    Shipping,
}

impl AddressKind {
    fn as_str(self) -> &'static str {
        match self {
            AddressKind::Billing => "billing",
            AddressKind::Shipping => "shipping",
        }
    }
}

#[derive(Clone, Copy)]
enum AddressField {
    FullName,
    FirstName,
    LastName,
    Organization,
    Email,
    Phone,
    AddressLine1,
    AddressLine2,
    City,
    Region,
    PostalCode,
    Country,
}

impl AddressField {
    fn name(self) -> &'static str {
        match self {
            AddressField::FullName => "full_name",
            AddressField::FirstName => "first_name",
            AddressField::LastName => "last_name",
            AddressField::Organization => "organization",
            AddressField::Email => "email",
            AddressField::Phone => "phone",
            AddressField::AddressLine1 => "address_line1",
            AddressField::AddressLine2 => "address_line2",
            AddressField::City => "city",
            AddressField::Region => "region",
            AddressField::PostalCode => "postal_code",
            AddressField::Country => "country",
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OnePasswordProfileSpec {
    Item(String),
    Detailed {
        #[serde(default)]
        item: Option<String>,
        #[serde(default)]
        payment: Option<String>,
        #[serde(default)]
        card: Option<String>,
        #[serde(default)]
        address: Option<String>,
        #[serde(default)]
        billing: Option<String>,
        #[serde(default, rename = "billingAddress")]
        billing_address_camel: Option<String>,
        #[serde(default)]
        billing_address: Option<String>,
        #[serde(default)]
        shipping: Option<String>,
        #[serde(default, rename = "shippingAddress")]
        shipping_address_camel: Option<String>,
        #[serde(default)]
        shipping_address: Option<String>,
    },
}

fn onepassword_config() -> Result<OnePasswordConfig> {
    let items = parse_onepassword_items(
        env::var(ONEPASSWORD_ITEMS_ENV).ok().as_deref(),
        env::var(ONEPASSWORD_ITEM_ENV).ok().as_deref(),
        env::var(ONEPASSWORD_PROFILE_ENV).ok().as_deref(),
        env::var(ONEPASSWORD_ADDRESS_ITEM_ENV).ok().as_deref(),
        env::var(ONEPASSWORD_BILLING_ADDRESS_ITEM_ENV)
            .ok()
            .as_deref(),
        env::var(ONEPASSWORD_SHIPPING_ADDRESS_ITEM_ENV)
            .ok()
            .as_deref(),
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
    single_address_item: Option<&str>,
    single_billing_address_item: Option<&str>,
    single_shipping_address_item: Option<&str>,
) -> Result<Vec<(String, OnePasswordProfileRefs)>> {
    match (items_json, single_item) {
        (Some(_), Some(_)) => {
            bail!("set only one of {ONEPASSWORD_ITEMS_ENV} or {ONEPASSWORD_ITEM_ENV}")
        }
        (Some(raw), None) => {
            let map = serde_json::from_str::<HashMap<String, OnePasswordProfileSpec>>(raw).with_context(|| {
                format!(
                    "{ONEPASSWORD_ITEMS_ENV} must be a JSON object of profile keys to 1Password item refs"
                )
            })?;
            let mut items = Vec::new();
            for (profile_key, spec) in map {
                let profile_key = profile_key.trim().to_owned();
                if profile_key.is_empty() {
                    bail!("{ONEPASSWORD_ITEMS_ENV} cannot contain empty profile keys");
                }
                items.push((profile_key, profile_refs_from_spec(spec)?));
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
            let address = trim_optional(single_address_item);
            let billing_address =
                trim_optional(single_billing_address_item).or_else(|| address.clone());
            let shipping_address =
                trim_optional(single_shipping_address_item).or_else(|| address.clone());
            Ok(vec![(
                profile_key.to_owned(),
                OnePasswordProfileRefs {
                    payment: item_ref.to_owned(),
                    billing_address,
                    shipping_address,
                },
            )])
        }
        (None, None) => {
            bail!("{ONEPASSWORD_ITEM_ENV} or {ONEPASSWORD_ITEMS_ENV} is required")
        }
    }
}

fn profile_refs_from_spec(spec: OnePasswordProfileSpec) -> Result<OnePasswordProfileRefs> {
    match spec {
        OnePasswordProfileSpec::Item(item_ref) => {
            let payment = require_item_ref(Some(item_ref), "payment item")?;
            Ok(OnePasswordProfileRefs {
                payment,
                billing_address: None,
                shipping_address: None,
            })
        }
        OnePasswordProfileSpec::Detailed {
            item,
            payment,
            card,
            address,
            billing,
            billing_address_camel,
            billing_address,
            shipping,
            shipping_address_camel,
            shipping_address,
        } => {
            let payment = require_item_ref(item.or(payment).or(card), "payment item")?;
            let address = trim_owned_optional(address);
            Ok(OnePasswordProfileRefs {
                payment,
                billing_address: trim_owned_optional(billing)
                    .or_else(|| trim_owned_optional(billing_address_camel))
                    .or_else(|| trim_owned_optional(billing_address))
                    .or_else(|| address.clone()),
                shipping_address: trim_owned_optional(shipping)
                    .or_else(|| trim_owned_optional(shipping_address_camel))
                    .or_else(|| trim_owned_optional(shipping_address))
                    .or(address),
            })
        }
    }
}

fn require_item_ref(item_ref: Option<String>, label: &str) -> Result<String> {
    trim_owned_optional(item_ref).ok_or_else(|| anyhow!("{label} cannot be empty"))
}

fn trim_optional(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn trim_owned_optional(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
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

fn fill_address_value(
    tab: &Tab,
    kind: AddressKind,
    field: AddressField,
    value: Option<&str>,
    filled: &mut Vec<&'static str>,
) -> Result<()> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(());
    };
    let selectors = address_field_selectors(kind, field);
    if let Some(selector) = find_visible_selector_owned(tab, &selectors)? {
        inject_into_selector(tab, &selector, value)?;
        filled.push(field.name());
    }
    Ok(())
}

fn full_name(address: &AddressProfile) -> Option<String> {
    if let Some(full_name) = &address.full_name {
        return Some(full_name.clone());
    }
    let mut parts = Vec::new();
    if let Some(first_name) = &address.first_name {
        parts.push(first_name.as_str());
    }
    if let Some(last_name) = &address.last_name {
        parts.push(last_name.as_str());
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

fn address_field_value(address: &AddressProfile, field_name: &str) -> Option<String> {
    match field_name {
        "full_name" | "name" => full_name(address),
        "first_name" => address.first_name.clone(),
        "last_name" => address.last_name.clone(),
        "organization" | "company" => address.organization.clone(),
        "email" => address.email.clone(),
        "phone" => address.phone.clone(),
        "address_line1" | "line1" => address.address_line1.clone(),
        "address_line2" | "line2" => address.address_line2.clone(),
        "city" => address.city.clone(),
        "region" | "state" | "province" => address.region.clone(),
        "postal_code" | "zip" | "postcode" => address.postal_code.clone(),
        "country" => address.country.clone(),
        _ => None,
    }
}

fn address_field_selectors(kind: AddressKind, field: AddressField) -> Vec<String> {
    let scope = kind.as_str();
    let (autocomplete_tokens, fallback_selectors): (&[&str], &[&str]) = match field {
        AddressField::FullName => (
            &["name"],
            &[
                r#"input[name*="full" i][name*="name" i]"#,
                r#"input[id*="full" i][id*="name" i]"#,
                r#"input[name="name" i]"#,
            ],
        ),
        AddressField::FirstName => (
            &["given-name"],
            &[
                r#"input[name*="first" i][name*="name" i]"#,
                r#"input[id*="first" i][id*="name" i]"#,
            ],
        ),
        AddressField::LastName => (
            &["family-name"],
            &[
                r#"input[name*="last" i][name*="name" i]"#,
                r#"input[id*="last" i][id*="name" i]"#,
            ],
        ),
        AddressField::Organization => (
            &["organization"],
            &[
                r#"input[name*="company" i]"#,
                r#"input[id*="company" i]"#,
                r#"input[name*="organization" i]"#,
            ],
        ),
        AddressField::Email => (
            &["email"],
            &[r#"input[type="email"]"#, r#"input[name*="email" i]"#],
        ),
        AddressField::Phone => (
            &["tel"],
            &[
                r#"input[type="tel"]"#,
                r#"input[name*="phone" i]"#,
                r#"input[name*="mobile" i]"#,
            ],
        ),
        AddressField::AddressLine1 => (
            &["address-line1", "street-address"],
            &[
                r#"input[name*="address1" i]"#,
                r#"input[name*="address_1" i]"#,
                r#"input[name*="line1" i]"#,
                r#"input[name*="street" i]"#,
            ],
        ),
        AddressField::AddressLine2 => (
            &["address-line2"],
            &[
                r#"input[name*="address2" i]"#,
                r#"input[name*="address_2" i]"#,
                r#"input[name*="line2" i]"#,
                r#"input[name*="apt" i]"#,
            ],
        ),
        AddressField::City => (
            &["address-level2"],
            &[
                r#"input[name*="city" i]"#,
                r#"input[id*="city" i]"#,
                r#"input[name*="town" i]"#,
            ],
        ),
        AddressField::Region => (
            &["address-level1"],
            &[
                r#"input[name*="state" i]"#,
                r#"select[name*="state" i]"#,
                r#"input[name*="province" i]"#,
                r#"select[name*="province" i]"#,
            ],
        ),
        AddressField::PostalCode => (
            &["postal-code"],
            &[
                r#"input[name*="postal" i]"#,
                r#"input[name*="postcode" i]"#,
                r#"input[name*="zip" i]"#,
            ],
        ),
        AddressField::Country => (
            &["country", "country-name"],
            &[
                r#"input[name*="country" i]"#,
                r#"select[name*="country" i]"#,
                r#"input[id*="country" i]"#,
                r#"select[id*="country" i]"#,
            ],
        ),
    };

    let mut selectors = Vec::new();
    for token in autocomplete_tokens {
        selectors.push(format!(r#"[autocomplete="{scope} {token}"]"#));
        selectors.push(format!(r#"[autocomplete="{token}"]"#));
    }
    for selector in fallback_selectors {
        selectors.push(scoped_selector(scope, selector));
    }
    selectors.extend(
        fallback_selectors
            .iter()
            .map(|selector| (*selector).to_owned()),
    );
    selectors
}

fn scoped_selector(scope: &str, selector: &str) -> String {
    format!(
        r#"{selector}[name*="{scope}" i], {selector}[id*="{scope}" i], {selector}[autocomplete*="{scope}" i]"#
    )
}

fn parse_onepassword_item_json(raw: &str, item_ref: &str) -> Result<PartialPaymentProfile> {
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
                "postalcode" | "postcode" | "zip" | "zipcode" => {
                    set_if_missing(&mut profile.postal_code, value.trim().to_owned());
                    apply_profile_address_field(&mut profile, &key, value.trim().to_owned());
                }
                "billingzip" | "billingpostalcode" => {
                    set_if_missing(&mut profile.postal_code, value.trim().to_owned());
                    apply_profile_address_field(&mut profile, &key, value.trim().to_owned());
                }
                _ => {
                    apply_profile_address_field(&mut profile, &key, value.trim().to_owned());
                }
            }
        }
    }

    Ok(profile)
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

fn apply_profile_address_field(profile: &mut PartialPaymentProfile, key: &str, value: String) {
    let Some((kind, field_key)) = address_key_parts(key) else {
        return;
    };
    match kind {
        Some(AddressKind::Billing) => {
            apply_address_field(&mut profile.billing_address, &field_key, value)
        }
        Some(AddressKind::Shipping) => {
            apply_address_field(&mut profile.shipping_address, &field_key, value)
        }
        None => apply_address_field(&mut profile.address, &field_key, value),
    }
}

fn address_key_parts(key: &str) -> Option<(Option<AddressKind>, String)> {
    for prefix in ["billingaddress", "billing"] {
        if let Some(rest) = key.strip_prefix(prefix)
            && !rest.is_empty()
        {
            return Some((Some(AddressKind::Billing), rest.to_owned()));
        }
    }
    for prefix in ["shippingaddress", "shipping", "deliveryaddress", "delivery"] {
        if let Some(rest) = key.strip_prefix(prefix)
            && !rest.is_empty()
        {
            return Some((Some(AddressKind::Shipping), rest.to_owned()));
        }
    }
    if is_address_field_key(key) {
        return Some((None, key.to_owned()));
    }
    None
}

fn is_address_field_key(key: &str) -> bool {
    matches!(
        key,
        "fullname"
            | "name"
            | "firstname"
            | "givenname"
            | "lastname"
            | "familyname"
            | "organization"
            | "company"
            | "email"
            | "emailaddress"
            | "phone"
            | "telephone"
            | "tel"
            | "mobile"
            | "address"
            | "street"
            | "streetaddress"
            | "addressline1"
            | "line1"
            | "address1"
            | "addressline2"
            | "line2"
            | "address2"
            | "city"
            | "town"
            | "addresslevel2"
            | "state"
            | "province"
            | "region"
            | "county"
            | "addresslevel1"
            | "postalcode"
            | "postcode"
            | "zip"
            | "zipcode"
            | "country"
            | "countryname"
    )
}

fn apply_address_field(address: &mut AddressProfile, key: &str, value: String) {
    if value.trim().is_empty() {
        return;
    }
    match key {
        "fullname" | "name" => set_if_missing(&mut address.full_name, value),
        "firstname" | "givenname" => set_if_missing(&mut address.first_name, value),
        "lastname" | "familyname" => set_if_missing(&mut address.last_name, value),
        "organization" | "company" => set_if_missing(&mut address.organization, value),
        "email" | "emailaddress" => set_if_missing(&mut address.email, value),
        "phone" | "telephone" | "tel" | "mobile" => set_if_missing(&mut address.phone, value),
        "address" | "street" | "streetaddress" => set_multiline_address(address, &value),
        "addressline1" | "line1" | "address1" => set_if_missing(&mut address.address_line1, value),
        "addressline2" | "line2" | "address2" => set_if_missing(&mut address.address_line2, value),
        "city" | "town" | "addresslevel2" => set_if_missing(&mut address.city, value),
        "state" | "province" | "region" | "county" | "addresslevel1" => {
            set_if_missing(&mut address.region, value)
        }
        "postalcode" | "postcode" | "zip" | "zipcode" => {
            set_if_missing(&mut address.postal_code, value)
        }
        "country" | "countryname" => set_if_missing(&mut address.country, value),
        _ => {}
    }
}

fn set_multiline_address(address: &mut AddressProfile, value: &str) {
    let lines = value
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    match lines.as_slice() {
        [] => {}
        [line1] => set_if_missing(&mut address.address_line1, (*line1).to_owned()),
        [line1, line2, ..] => {
            set_if_missing(&mut address.address_line1, (*line1).to_owned());
            set_if_missing(&mut address.address_line2, (*line2).to_owned());
        }
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
        let billing_address = self.address_for(AddressKind::Billing);
        let shipping_address = self.address_for(AddressKind::Shipping);
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
            billing_address,
            shipping_address,
        })
    }

    fn address_for(&self, kind: AddressKind) -> Option<AddressProfile> {
        let mut address = match kind {
            AddressKind::Billing => self.billing_address.clone(),
            AddressKind::Shipping => self.shipping_address.clone(),
        };
        address.merge_missing_from(&self.address);
        if address.is_empty() {
            None
        } else {
            Some(address)
        }
    }
}

impl AddressProfile {
    fn merge_missing_from(&mut self, other: &AddressProfile) {
        merge_missing(&mut self.full_name, &other.full_name);
        merge_missing(&mut self.first_name, &other.first_name);
        merge_missing(&mut self.last_name, &other.last_name);
        merge_missing(&mut self.organization, &other.organization);
        merge_missing(&mut self.email, &other.email);
        merge_missing(&mut self.phone, &other.phone);
        merge_missing(&mut self.address_line1, &other.address_line1);
        merge_missing(&mut self.address_line2, &other.address_line2);
        merge_missing(&mut self.city, &other.city);
        merge_missing(&mut self.region, &other.region);
        merge_missing(&mut self.postal_code, &other.postal_code);
        merge_missing(&mut self.country, &other.country);
    }

    fn is_empty(&self) -> bool {
        self.full_name.is_none()
            && self.first_name.is_none()
            && self.last_name.is_none()
            && self.organization.is_none()
            && self.email.is_none()
            && self.phone.is_none()
            && self.address_line1.is_none()
            && self.address_line2.is_none()
            && self.city.is_none()
            && self.region.is_none()
            && self.postal_code.is_none()
            && self.country.is_none()
    }
}

fn merge_missing(target: &mut Option<String>, source: &Option<String>) {
    if target.is_none() {
        *target = source.clone();
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

fn find_visible_selector(tab: &Tab, selectors: &[&str]) -> Result<Option<String>> {
    let selectors_json = serde_json::to_string(selectors)?;
    find_visible_selector_json(tab, &selectors_json)
}

fn find_visible_selector_owned(tab: &Tab, selectors: &[String]) -> Result<Option<String>> {
    let selectors_json = serde_json::to_string(selectors)?;
    find_visible_selector_json(tab, &selectors_json)
}

fn find_visible_selector_json(tab: &Tab, selectors_json: &str) -> Result<Option<String>> {
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

fn element_is_credential_field(tab: &Tab, css: &str) -> Result<bool> {
    let css_json = serde_json::to_string(css)?;
    let js = format!(
        r#"(() => {{
            const el = document.querySelector({css_json});
            if (!el) return false;
            const autocomplete = (el.getAttribute("autocomplete") || "").toLowerCase();
            if (/^(cc-|shipping |billing )/.test(autocomplete)) return true;
            const haystack = [
                el.getAttribute("name"),
                el.getAttribute("id"),
                el.getAttribute("placeholder"),
                el.getAttribute("aria-label")
            ].filter(Boolean).join(" ").toLowerCase();
            return /card|cvc|cvv|exp|security code|postal|postcode|zip|address|street|city|state|province|country|phone|email/.test(haystack);
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
        AddressKind, OnePasswordProfileRefs, is_sensitive_submit, parse_address_field_ref,
        parse_field_ref, parse_onepassword_item_json, parse_onepassword_items,
    };

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
        let (profile, kind, field) = parse_address_field_ref("home:billing.postal_code").unwrap();
        assert_eq!(profile, "home");
        assert_eq!(kind.as_str(), "billing");
        assert_eq!(field, "postal_code");
    }

    #[test]
    fn parses_onepassword_single_item_config() {
        let items = parse_onepassword_items(
            None,
            Some("Personal Visa"),
            Some("primary"),
            Some("Home Identity"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            items,
            vec![(
                "primary".to_owned(),
                OnePasswordProfileRefs {
                    payment: "Personal Visa".to_owned(),
                    billing_address: Some("Home Identity".to_owned()),
                    shipping_address: Some("Home Identity".to_owned()),
                }
            )]
        );

        let default_items =
            parse_onepassword_items(None, Some("Personal Visa"), None, None, None, None).unwrap();
        assert_eq!(
            default_items,
            vec![(
                "default".to_owned(),
                OnePasswordProfileRefs {
                    payment: "Personal Visa".to_owned(),
                    billing_address: None,
                    shipping_address: None,
                }
            )]
        );
    }

    #[test]
    fn parses_onepassword_multi_item_config() {
        let items = parse_onepassword_items(
            Some(
                r#"{
                    "backup": "Backup Mastercard",
                    "default": {
                        "card": "Personal Visa",
                        "billingAddress": "Billing Identity",
                        "shippingAddress": "Shipping Identity"
                    }
                }"#,
            ),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            items,
            vec![
                (
                    "backup".to_owned(),
                    OnePasswordProfileRefs {
                        payment: "Backup Mastercard".to_owned(),
                        billing_address: None,
                        shipping_address: None,
                    }
                ),
                (
                    "default".to_owned(),
                    OnePasswordProfileRefs {
                        payment: "Personal Visa".to_owned(),
                        billing_address: Some("Billing Identity".to_owned()),
                        shipping_address: Some("Shipping Identity".to_owned()),
                    }
                )
            ]
        );
    }

    #[test]
    fn parses_standard_onepassword_credit_card_item() {
        let parsed = parse_onepassword_item_json(
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
        let profile = parsed.into_payment_profile("Personal Visa").unwrap();

        assert_eq!(profile.card_number, "4242424242424242");
        assert_eq!(profile.exp_month, "12");
        assert_eq!(profile.exp_year, "28");
        assert_eq!(profile.cvc, "123");
        assert_eq!(profile.name.as_deref(), Some("Jane Doe"));
        assert_eq!(profile.postal_code.as_deref(), Some("94107"));
    }

    #[test]
    fn parses_custom_onepassword_payment_fields() {
        let parsed = parse_onepassword_item_json(
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
        let profile = parsed.into_payment_profile("Custom Card").unwrap();

        assert_eq!(profile.card_number, "4242424242424242");
        assert_eq!(profile.exp_month, "7");
        assert_eq!(profile.exp_year, "2029");
        assert_eq!(profile.cvc, "321");
        assert_eq!(profile.name.as_deref(), Some("Jane Doe"));
        assert_eq!(profile.postal_code.as_deref(), Some("94107"));
    }

    #[test]
    fn parses_onepassword_address_fields() {
        let parsed = parse_onepassword_item_json(
            r#"{
                "fields": [
                    { "label": "full name", "value": "Jane Doe" },
                    { "label": "shipping address line 1", "value": "1 Market St" },
                    { "label": "shipping address line 2", "value": "Apt 4" },
                    { "label": "shipping city", "value": "San Francisco" },
                    { "label": "shipping state", "value": "CA" },
                    { "label": "shipping postal code", "value": "94107" },
                    { "label": "billing address line 1", "value": "9 Billing Rd" },
                    { "label": "billing postal code", "value": "10001" },
                    { "label": "email", "value": "jane@example.com" },
                    { "label": "phone", "value": "+15550100" }
                ]
            }"#,
            "Home Identity",
        )
        .unwrap();

        let shipping = parsed.address_for(AddressKind::Shipping).unwrap();
        assert_eq!(shipping.full_name.as_deref(), Some("Jane Doe"));
        assert_eq!(shipping.address_line1.as_deref(), Some("1 Market St"));
        assert_eq!(shipping.address_line2.as_deref(), Some("Apt 4"));
        assert_eq!(shipping.city.as_deref(), Some("San Francisco"));
        assert_eq!(shipping.region.as_deref(), Some("CA"));
        assert_eq!(shipping.postal_code.as_deref(), Some("94107"));
        assert_eq!(shipping.email.as_deref(), Some("jane@example.com"));
        assert_eq!(shipping.phone.as_deref(), Some("+15550100"));

        let billing = parsed.address_for(AddressKind::Billing).unwrap();
        assert_eq!(billing.address_line1.as_deref(), Some("9 Billing Rd"));
        assert_eq!(billing.postal_code.as_deref(), Some("10001"));
        assert_eq!(billing.email.as_deref(), Some("jane@example.com"));
    }
}
