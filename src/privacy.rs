use serde_json::Value;

const REDACTED_ADDRESS: &str = "[redacted address]";
const REDACTED_CONTACT: &str = "[redacted contact]";

pub fn redact_value_strings(value: &mut Value) {
    match value {
        Value::String(text) => {
            *text = redact_reasoning_text(text);
        }
        Value::Array(items) => {
            for item in items {
                redact_value_strings(item);
            }
        }
        Value::Object(map) => {
            for (key, nested) in map {
                if key == "screenshot_base64" {
                    continue;
                }
                redact_value_strings(nested);
            }
        }
        _ => {}
    }
}

pub fn redact_reasoning_text(text: &str) -> String {
    if text.trim().is_empty() {
        return text.to_owned();
    }

    let mut address_context = 0usize;
    let mut lines = Vec::new();

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            lines.push(String::new());
            continue;
        }

        let lower = normalize_space_lower(line);
        if is_contact_line(line, &lower) {
            lines.push(contact_label(&lower).to_owned());
            address_context = address_context.saturating_sub(1);
            continue;
        }

        if let Some(label) = address_label(&lower) {
            lines.push(label.to_owned());
            address_context = 5;
            continue;
        }

        if address_context > 0 {
            if is_address_context_terminator(&lower) {
                address_context = 0;
                lines.push(line.to_owned());
            } else if should_redact_address_context_line(line, &lower) {
                address_context -= 1;
                push_dedup_redaction(&mut lines, REDACTED_ADDRESS);
            } else {
                address_context -= 1;
                lines.push(line.to_owned());
            }
            continue;
        }

        if is_probable_standalone_address(line, &lower) {
            push_dedup_redaction(&mut lines, REDACTED_ADDRESS);
        } else {
            lines.push(line.to_owned());
        }
    }

    lines.join("\n")
}

fn normalize_space_lower(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn push_dedup_redaction(lines: &mut Vec<String>, redaction: &str) {
    if lines.last().is_some_and(|line| line == redaction) {
        return;
    }
    lines.push(redaction.to_owned());
}

fn address_label(lower: &str) -> Option<&'static str> {
    if contains_any(
        lower,
        &[
            "shipping address",
            "ship to",
            "ships to",
            "shipping to",
            "delivery address",
            "delivery to",
            "deliver to",
            "delivering to",
            "send to",
        ],
    ) {
        return Some("Shipping address: [redacted]");
    }

    if lower.contains("billing address") {
        return Some("Billing address: [redacted]");
    }

    if lower == "address"
        || lower.starts_with("address:")
        || lower.starts_with("address ")
        || lower.contains(" address:")
        || lower.contains("street address")
        || lower.contains("postcode:")
        || lower.contains("postal code:")
        || lower.contains("zip code:")
    {
        return Some("Address: [redacted]");
    }

    None
}

fn is_contact_line(line: &str, lower: &str) -> bool {
    has_email(line)
        || digit_count(line) >= 7
            && contains_any(
                lower,
                &[
                    "phone",
                    "mobile",
                    "telephone",
                    "tel:",
                    "contact number",
                    "email:",
                    "email address",
                ],
            )
        || lower.starts_with('+') && digit_count(line) >= 10
}

fn contact_label(lower: &str) -> &'static str {
    if lower.contains("email") {
        "Email: [redacted contact]"
    } else if contains_any(
        lower,
        &["phone", "mobile", "telephone", "tel:", "contact number"],
    ) {
        "Phone: [redacted contact]"
    } else {
        REDACTED_CONTACT
    }
}

fn has_email(line: &str) -> bool {
    line.split_whitespace().any(|part| {
        let token = part.trim_matches(|ch: char| {
            ch.is_ascii_punctuation() && ch != '@' && ch != '.' && ch != '_' && ch != '-'
        });
        let Some((local, domain)) = token.split_once('@') else {
            return false;
        };
        !local.is_empty() && domain.contains('.') && domain.len() >= 3
    })
}

fn should_redact_address_context_line(line: &str, lower: &str) -> bool {
    if is_address_control_line(lower) {
        return false;
    }

    if line.chars().count() > 180 {
        return false;
    }

    true
}

fn is_address_context_terminator(lower: &str) -> bool {
    contains_any(
        lower,
        &[
            "order summary",
            "your order",
            "subtotal",
            "total",
            "payment method",
            "card number",
            "delivery fee",
            "service fee",
            "tip",
            "promo",
            "gift card",
            "shipping options",
            "delivery options",
            "place order",
            "pay now",
            "review order",
        ],
    ) || has_price(lower)
}

fn is_address_control_line(lower: &str) -> bool {
    matches!(
        lower,
        "change"
            | "edit"
            | "remove"
            | "continue"
            | "continue checkout"
            | "use this address"
            | "deliver to this address"
            | "ship to this address"
            | "add a new address"
            | "add new address"
            | "add address"
            | "select address"
            | "update location"
            | "delivery instructions"
            | "add delivery instructions"
    )
}

fn is_probable_standalone_address(line: &str, lower: &str) -> bool {
    if line.chars().count() > 180 {
        return false;
    }

    has_street_address_pattern(line, lower)
}

fn has_street_address_pattern(line: &str, lower: &str) -> bool {
    if !line.chars().any(|ch| ch.is_ascii_digit()) {
        return false;
    }

    let starts_with_digit = line
        .trim_start()
        .chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_digit());
    if !starts_with_digit && !line.contains(',') {
        return false;
    }

    let padded = format!(" {lower} ");
    contains_any(
        &padded,
        &[
            " street ",
            " st ",
            " st. ",
            " road ",
            " rd ",
            " rd. ",
            " avenue ",
            " ave ",
            " ave. ",
            " lane ",
            " ln ",
            " drive ",
            " dr ",
            " court ",
            " ct ",
            " way ",
            " close ",
            " crescent ",
            " boulevard ",
            " blvd ",
            " house ",
            " flat ",
            " apartment ",
            " apt ",
            " suite ",
            " unit ",
            " floor ",
            " place ",
            " pl ",
        ],
    )
}

fn has_price(lower: &str) -> bool {
    contains_any(lower, &["£", "$", "€"]) && lower.chars().any(|ch| ch.is_ascii_digit())
}

fn digit_count(text: &str) -> usize {
    text.chars().filter(|ch| ch.is_ascii_digit()).count()
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::{REDACTED_ADDRESS, redact_reasoning_text, redact_value_strings};
    use serde_json::json;

    #[test]
    fn redacts_amazon_style_shipping_block() {
        let text = redact_reasoning_text(
            "Checkout\nDeliver to Jane Doe\n1 Market St\nApt 4\nSan Francisco, CA 94107\nChange\nOrder summary\nSubtotal $10.00",
        );

        assert!(text.contains("Shipping address: [redacted]"));
        assert!(text.contains("Change"));
        assert!(text.contains("Subtotal $10.00"));
        assert!(!text.contains("Jane Doe"));
        assert!(!text.contains("Market"));
        assert!(!text.contains("94107"));
    }

    #[test]
    fn redacts_inline_shipping_label() {
        let text = redact_reasoning_text("Ship to Jane Doe, 1 Market St, San Francisco, CA 94107");

        assert_eq!(text, "Shipping address: [redacted]");
    }

    #[test]
    fn preserves_delivery_costs_and_options() {
        let text = redact_reasoning_text("Delivery fee $3.99\nDelivery options\nFree delivery");

        assert_eq!(text, "Delivery fee $3.99\nDelivery options\nFree delivery");
    }

    #[test]
    fn redacts_contact_values() {
        let text = redact_reasoning_text("Email: jane@example.com\nPhone: +1 555 010 1000");

        assert_eq!(text, "Email: [redacted contact]\nPhone: [redacted contact]");
    }

    #[test]
    fn recursively_redacts_json_strings() {
        let mut value = json!({
            "pageText": "Deliver to Jane Doe\n1 Market St",
            "elements": [{ "label": "Ship to Jane Doe, 1 Market St" }],
        });

        redact_value_strings(&mut value);

        assert_eq!(
            value["pageText"],
            "Shipping address: [redacted]\n[redacted address]"
        );
        assert_eq!(
            value["elements"][0]["label"],
            "Shipping address: [redacted]"
        );
    }

    #[test]
    fn redacts_standalone_street_address() {
        let text = redact_reasoning_text("Order ships from\n123 Example Road\nArrives tomorrow");

        assert!(text.contains(REDACTED_ADDRESS));
        assert!(!text.contains("Example Road"));
    }
}
