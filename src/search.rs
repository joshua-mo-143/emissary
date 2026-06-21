use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::time::Duration;

const DUCKDUCKGO_API: &str = "https://api.duckduckgo.com/";

pub fn duckduckgo_instant_answer(query: &str) -> Result<Value> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent("telephone-agent/0.1 (+https://github.com/joshua-mo-143)")
        .build()
        .context("failed to build DuckDuckGo HTTP client")?;

    let payload: Value = client
        .get(DUCKDUCKGO_API)
        .query(&[
            ("q", query),
            ("format", "json"),
            ("no_html", "1"),
            ("skip_disambig", "0"),
            ("no_redirect", "1"),
        ])
        .send()
        .context("DuckDuckGo request failed")?
        .error_for_status()
        .context("DuckDuckGo returned an error status")?
        .json()
        .context("DuckDuckGo returned invalid JSON")?;

    Ok(compact_instant_answer(query, &payload))
}

fn compact_instant_answer(query: &str, payload: &Value) -> Value {
    json!({
        "provider": "duckduckgo_instant_answer",
        "query": query,
        "heading": string_field(payload, "Heading"),
        "answer": string_field(payload, "Answer"),
        "abstract": string_field(payload, "AbstractText"),
        "abstractUrl": string_field(payload, "AbstractURL"),
        "definition": string_field(payload, "Definition"),
        "definitionUrl": string_field(payload, "DefinitionURL"),
        "type": string_field(payload, "Type"),
        "results": result_items(payload.get("Results")),
        "related": related_topics(payload.get("RelatedTopics"), 8),
    })
}

fn string_field(payload: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
}

fn result_items(value: Option<&Value>) -> Vec<Value> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(topic_item)
        .take(5)
        .collect()
}

fn related_topics(value: Option<&Value>, limit: usize) -> Vec<Value> {
    let mut out = Vec::new();
    collect_topics(value, &mut out, limit);
    out
}

fn collect_topics(value: Option<&Value>, out: &mut Vec<Value>, limit: usize) {
    if out.len() >= limit {
        return;
    }
    let Some(items) = value.and_then(Value::as_array) else {
        return;
    };

    for item in items {
        if out.len() >= limit {
            break;
        }
        if let Some(topic) = topic_item(item) {
            out.push(topic);
        } else if let Some(nested) = item.get("Topics") {
            collect_topics(Some(nested), out, limit);
        }
    }
}

fn topic_item(item: &Value) -> Option<Value> {
    let text = item
        .get("Text")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())?;
    Some(json!({
        "text": text,
        "url": item.get("FirstURL").and_then(Value::as_str),
    }))
}

#[cfg(test)]
mod tests {
    use super::compact_instant_answer;
    use serde_json::json;

    #[test]
    fn compacts_nested_related_topics() {
        let payload = json!({
            "Heading": "Rust",
            "AbstractText": "Rust is a programming language.",
            "AbstractURL": "https://example.com/rust",
            "RelatedTopics": [
                { "Text": "Rust - language", "FirstURL": "https://example.com/1" },
                {
                    "Name": "Nested",
                    "Topics": [
                        { "Text": "Cargo - build tool", "FirstURL": "https://example.com/2" }
                    ]
                }
            ]
        });

        let compact = compact_instant_answer("rust", &payload);
        assert_eq!(compact["heading"], "Rust");
        assert_eq!(compact["related"].as_array().unwrap().len(), 2);
    }
}
