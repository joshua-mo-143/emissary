use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

const DUCKDUCKGO_API: &str = "https://api.duckduckgo.com/";

pub fn duckduckgo_instant_answer(query: &str) -> Result<DuckDuckGoSearchResult> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent("emissary-agent/0.1 (+https://github.com/joshua-mo-143)")
        .build()
        .context("failed to build DuckDuckGo HTTP client")?;

    let payload: DuckDuckGoResponse = client
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

    Ok(DuckDuckGoSearchResult::from_response(query, payload))
}

#[derive(Debug, Deserialize)]
struct DuckDuckGoResponse {
    #[serde(rename = "Heading", default)]
    heading: String,
    #[serde(rename = "Answer", default)]
    answer: String,
    #[serde(rename = "AbstractText", default)]
    abstract_text: String,
    #[serde(rename = "AbstractURL", default)]
    abstract_url: String,
    #[serde(rename = "Definition", default)]
    definition: String,
    #[serde(rename = "DefinitionURL", default)]
    definition_url: String,
    #[serde(rename = "Type", default)]
    type_field: String,
    #[serde(rename = "Results", default)]
    results: Vec<DuckDuckGoTopic>,
    #[serde(rename = "RelatedTopics", default)]
    related_topics: Vec<DuckDuckGoTopic>,
}

#[derive(Debug, Deserialize)]
struct DuckDuckGoTopic {
    #[serde(rename = "Text", default)]
    text: String,
    #[serde(rename = "FirstURL", default)]
    first_url: String,
    #[serde(rename = "Topics", default)]
    topics: Vec<DuckDuckGoTopic>,
}

#[derive(Debug, Serialize)]
pub struct DuckDuckGoSearchResult {
    pub provider: &'static str,
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heading: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    #[serde(rename = "abstract", skip_serializing_if = "Option::is_none")]
    pub abstract_text: Option<String>,
    #[serde(rename = "abstractUrl", skip_serializing_if = "Option::is_none")]
    pub abstract_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
    #[serde(rename = "definitionUrl", skip_serializing_if = "Option::is_none")]
    pub definition_url: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub type_field: Option<String>,
    pub results: Vec<SearchTopic>,
    pub related: Vec<SearchTopic>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct SearchTopic {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

impl DuckDuckGoSearchResult {
    fn from_response(query: &str, response: DuckDuckGoResponse) -> Self {
        Self {
            provider: "duckduckgo_instant_answer",
            query: query.to_owned(),
            heading: non_empty(response.heading),
            answer: non_empty(response.answer),
            abstract_text: non_empty(response.abstract_text),
            abstract_url: non_empty(response.abstract_url),
            definition: non_empty(response.definition),
            definition_url: non_empty(response.definition_url),
            type_field: non_empty(response.type_field),
            results: response
                .results
                .iter()
                .filter_map(SearchTopic::from_ddg_topic)
                .take(5)
                .collect(),
            related: related_topics(&response.related_topics, 8),
        }
    }
}

impl SearchTopic {
    fn from_ddg_topic(topic: &DuckDuckGoTopic) -> Option<Self> {
        let text = non_empty(topic.text.clone())?;
        Some(Self {
            text,
            url: non_empty(topic.first_url.clone()),
        })
    }
}

fn related_topics(topics: &[DuckDuckGoTopic], limit: usize) -> Vec<SearchTopic> {
    let mut out = Vec::new();
    collect_topics(topics, &mut out, limit);
    out
}

fn collect_topics(topics: &[DuckDuckGoTopic], out: &mut Vec<SearchTopic>, limit: usize) {
    if out.len() >= limit {
        return;
    }

    for topic in topics {
        if out.len() >= limit {
            break;
        }
        if let Some(item) = SearchTopic::from_ddg_topic(topic) {
            out.push(item);
        }
        if !topic.topics.is_empty() {
            collect_topics(&topic.topics, out, limit);
        }
    }
}

fn non_empty(value: String) -> Option<String> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::{DuckDuckGoResponse, DuckDuckGoSearchResult};

    #[test]
    fn compacts_nested_related_topics() {
        let payload: DuckDuckGoResponse = serde_json::from_str(
            r#"{
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
            }"#,
        )
        .unwrap();

        let compact = DuckDuckGoSearchResult::from_response("rust", payload);
        assert_eq!(compact.heading.as_deref(), Some("Rust"));
        assert_eq!(compact.related.len(), 2);
    }
}
