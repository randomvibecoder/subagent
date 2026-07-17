use crate::ipc::{CodedError, coded_error};
use anyhow::{Context, Result};
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};

const MAX_ATTEMPTS: u32 = 5;
const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone)]
pub struct ModelTurn {
    pub content: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Value>,
}

#[derive(Debug, Clone)]
pub enum ModelProgress {
    AttemptStarted { retry_count: u32 },
    ResponseStarted { provider_request_id: Option<String> },
    ProviderActivity,
    RetryScheduled { retry_count: u32 },
}

#[derive(Clone)]
pub struct OpenAiClient {
    client: Client,
    api_key: String,
    base_url: String,
    model: String,
}

impl OpenAiClient {
    pub fn new(api_key: String, base_url: String, model: String) -> Result<Self> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            client,
            api_key,
            base_url: base_url.trim_end_matches('/').to_string(),
            model,
        })
    }

    pub async fn complete_observed(
        &self,
        messages: &[Value],
        tools: &[Value],
        mut progress: impl FnMut(ModelProgress) -> Result<()>,
    ) -> Result<ModelTurn> {
        let body = json!({
            "model": self.model,
            "messages": messages,
            "tools": tools,
            "tool_choice": "auto",
            "stream": true,
        });
        self.complete_body_observed(body, &mut progress).await
    }

    pub async fn complete_text_observed(
        &self,
        messages: &[Value],
        mut progress: impl FnMut(ModelProgress) -> Result<()>,
    ) -> Result<ModelTurn> {
        let body = json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
        });
        self.complete_body_observed(body, &mut progress).await
    }

    async fn complete_body_observed(
        &self,
        body: Value,
        progress: &mut impl FnMut(ModelProgress) -> Result<()>,
    ) -> Result<ModelTurn> {
        let mut last_error = None;
        for attempt in 0..MAX_ATTEMPTS {
            progress(ModelProgress::AttemptStarted {
                retry_count: attempt,
            })?;
            match self.complete_once(&body, progress).await {
                Ok(turn) => return Ok(turn),
                Err(e) => {
                    let delay = retry_delay(&e, attempt);
                    last_error = Some(e);
                    if attempt + 1 >= MAX_ATTEMPTS || delay.is_none() {
                        break;
                    }
                    let delay = delay.unwrap();
                    progress(ModelProgress::RetryScheduled {
                        retry_count: attempt + 1,
                    })?;
                    tokio::time::sleep(delay).await;
                }
            }
        }
        Err(last_error.unwrap())
    }

    async fn complete_once(
        &self,
        body: &Value,
        progress: &mut impl FnMut(ModelProgress) -> Result<()>,
    ) -> Result<ModelTurn> {
        let url = format!("{}/chat/completions", self.base_url);
        let response = self
            .client
            .post(url)
            .bearer_auth(&self.api_key)
            .header("Accept", "text/event-stream")
            .json(body)
            .send()
            .await
            .map_err(|error| {
                coded_error(
                    "api_error",
                    format!("OpenAI request failed: {error}"),
                    json!({}),
                    true,
                )
            })?;
        let status = response.status();
        if !status.is_success() {
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(parse_retry_after_seconds);
            let text = response.text().await.unwrap_or_default();
            return Err(coded_error(
                "api_error",
                format!("OpenAI API returned {status}: {}", truncate(&text, 2000)),
                json!({"http_status":status.as_u16(),"retry_after_seconds":retry_after}),
                status.is_server_error() || status.as_u16() == 429,
            ));
        }

        let provider_request_id = ["x-request-id", "request-id", "openai-request-id"]
            .into_iter()
            .find_map(|name| {
                response
                    .headers()
                    .get(name)
                    .and_then(|value| value.to_str().ok())
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
            });
        progress(ModelProgress::ResponseStarted {
            provider_request_id,
        })?;

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if !content_type.contains("text/event-stream") {
            progress(ModelProgress::ProviderActivity)?;
            let value: Value = response.json().await.context("decode OpenAI response")?;
            return parse_non_streaming(&value);
        }

        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut calls: BTreeMap<usize, ToolBuilder> = BTreeMap::new();
        let mut usage = None;
        loop {
            let next = tokio::time::timeout(STREAM_IDLE_TIMEOUT, stream.next())
                .await
                .map_err(|_| {
                    coded_error(
                        "api_error",
                        "OpenAI stream produced no data for five minutes",
                        json!({"timeout_seconds":STREAM_IDLE_TIMEOUT.as_secs()}),
                        true,
                    )
                })?;
            let Some(chunk) = next else { break };
            let chunk = chunk.map_err(|error| {
                coded_error(
                    "api_error",
                    format!("OpenAI stream failed: {error}"),
                    json!({}),
                    true,
                )
            })?;
            progress(ModelProgress::ProviderActivity)?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buffer.find('\n') {
                let line = buffer[..pos].trim_end_matches('\r').to_string();
                buffer.drain(..=pos);
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data == "[DONE]" {
                    break;
                }
                if data.is_empty() {
                    continue;
                }
                let value: Value = serde_json::from_str(data).context("decode OpenAI SSE event")?;
                if value.get("usage").is_some_and(Value::is_object) {
                    usage = value.get("usage").cloned();
                }
                let Some(delta) = value.pointer("/choices/0/delta") else {
                    continue;
                };
                if let Some(s) = delta.get("content").and_then(Value::as_str) {
                    content.push_str(s);
                }
                if let Some(s) = delta
                    .get("reasoning")
                    .and_then(Value::as_str)
                    .or_else(|| delta.get("reasoning_content").and_then(Value::as_str))
                {
                    reasoning.push_str(s);
                }
                if let Some(items) = delta.get("tool_calls").and_then(Value::as_array) {
                    for item in items {
                        let idx = item.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                        let b = calls.entry(idx).or_default();
                        if let Some(s) = item.get("id").and_then(Value::as_str) {
                            b.id.push_str(s);
                        }
                        if let Some(s) = item.pointer("/function/name").and_then(Value::as_str) {
                            b.name.push_str(s);
                        }
                        if let Some(s) = item.pointer("/function/arguments").and_then(Value::as_str)
                        {
                            b.arguments.push_str(s);
                        }
                    }
                }
            }
        }
        let tool_calls = calls
            .into_values()
            .map(|b| ToolCall {
                id: b.id,
                kind: "function".into(),
                function: FunctionCall {
                    name: b.name,
                    arguments: b.arguments,
                },
            })
            .collect();
        Ok(ModelTurn {
            content,
            reasoning,
            tool_calls,
            usage,
        })
    }
}

fn retry_delay(error: &anyhow::Error, attempt: u32) -> Option<Duration> {
    let coded = error.downcast_ref::<CodedError>()?;
    if !coded.retryable {
        return None;
    }
    let seconds = coded
        .details
        .get("retry_after_seconds")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| 1u64 << attempt.min(6));
    Some(Duration::from_secs(
        seconds.clamp(1, MAX_RETRY_DELAY.as_secs()),
    ))
}

fn parse_retry_after_seconds(value: &str) -> Option<u64> {
    if let Ok(seconds) = value.trim().parse::<u64>() {
        return Some(seconds.clamp(1, MAX_RETRY_DELAY.as_secs()));
    }
    let retry_at = chrono::DateTime::parse_from_rfc2822(value.trim()).ok()?;
    let seconds = (retry_at.with_timezone(&chrono::Utc) - chrono::Utc::now())
        .num_seconds()
        .max(1) as u64;
    Some(seconds.min(MAX_RETRY_DELAY.as_secs()))
}

#[derive(Default)]
struct ToolBuilder {
    id: String,
    name: String,
    arguments: String,
}

fn parse_non_streaming(v: &Value) -> Result<ModelTurn> {
    let msg = v
        .pointer("/choices/0/message")
        .context("OpenAI response missing choices[0].message")?;
    let content = msg
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let reasoning = msg
        .get("reasoning")
        .and_then(Value::as_str)
        .or_else(|| msg.get("reasoning_content").and_then(Value::as_str))
        .unwrap_or_default()
        .to_string();
    let tool_calls = msg
        .get("tool_calls")
        .cloned()
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default();
    Ok(ModelTurn {
        content,
        reasoning,
        tool_calls,
        usage: v.get("usage").filter(|usage| usage.is_object()).cloned(),
    })
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

pub fn assistant_message(turn: &ModelTurn) -> Value {
    let calls = serde_json::to_value(&turn.tool_calls).unwrap();
    if turn.tool_calls.is_empty() {
        json!({"role":"assistant","content":turn.content})
    } else {
        json!({"role":"assistant","content": if turn.content.is_empty() { Value::Null } else { Value::String(turn.content.clone()) }, "tool_calls":calls})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_delay_respects_retryability_and_bounds() {
        let fatal = coded_error("api_error", "bad request", json!({}), false);
        assert_eq!(retry_delay(&fatal, 0), None);

        let throttled = coded_error(
            "api_error",
            "rate limited",
            json!({"retry_after_seconds":120}),
            true,
        );
        assert_eq!(retry_delay(&throttled, 0), Some(MAX_RETRY_DELAY));

        let network = coded_error("api_error", "network", json!({}), true);
        assert_eq!(retry_delay(&network, 3), Some(Duration::from_secs(8)));
    }

    #[test]
    fn retry_after_accepts_seconds_and_http_dates() {
        assert_eq!(parse_retry_after_seconds("7"), Some(7));
        assert_eq!(parse_retry_after_seconds("999"), Some(60));
        assert!(parse_retry_after_seconds("Wed, 21 Oct 2099 07:28:00 GMT").is_some());
        assert_eq!(parse_retry_after_seconds("not-a-delay"), None);
    }
}
