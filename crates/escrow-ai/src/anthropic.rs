//! The Anthropic Messages API with a forced tool call, so the answer is structured JSON rather than prose.

use crate::{
    review::{ModelError, ModelOutput, ModelPort},
    schema::tool_schema,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;

pub const DEFAULT_MODEL: &str = "claude-sonnet-5-5";

pub struct AnthropicModel {
    http: reqwest::Client,
    api_key: String,
    model: String,
    base: String,
}

impl AnthropicModel {
    pub fn new(api_key: &str, model: &str) -> Self {
        Self::with_base(api_key, model, "https://api.anthropic.com")
    }

    pub fn with_base(api_key: &str, model: &str, base: &str) -> Self {
        let http =
            reqwest::Client::builder().timeout(Duration::from_secs(120)).user_agent("skillescrow-backend").build().expect("client builds");
        Self { http, api_key: api_key.into(), model: model.into(), base: base.trim_end_matches('/').into() }
    }
}

impl std::fmt::Debug for AnthropicModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicModel").field("model", &self.model).finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct Response {
    content: Vec<Block>,
    usage: Usage,
}

#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type")]
    kind: String,
    input: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct Usage {
    input_tokens: u32,
    output_tokens: u32,
}

#[async_trait]
impl ModelPort for AnthropicModel {
    async fn complete(&self, system: &str, user: &str) -> Result<ModelOutput, ModelError> {
        let body = json!({
            "model": self.model,
            "max_tokens": 2048,
            "system": system,
            "tools": [{ "name": "submit_review", "description": "Submit the structured advisory review.", "input_schema": tool_schema() }],
            "tool_choice": { "type": "tool", "name": "submit_review" },
            "messages": [{ "role": "user", "content": user }],
        });
        let r = self
            .http
            .post(format!("{}/v1/messages", self.base))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await
            .map_err(|e| ModelError::Network(if e.is_timeout() { "timeout".into() } else { "request failed".into() }))?;
        if !r.status().is_success() {
            return Err(ModelError::Status(r.status().as_u16()));
        }
        let res: Response = r.json().await.map_err(|_| ModelError::NoToolCall)?;
        let output = res.content.into_iter().find(|b| b.kind == "tool_use").and_then(|b| b.input).ok_or(ModelError::NoToolCall)?;
        Ok(ModelOutput { output, input_tokens: res.usage.input_tokens, output_tokens: res.usage.output_tokens })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, extract::State, http::HeaderMap, routing::post};
    use std::sync::{Arc, Mutex};

    type Seen = Arc<Mutex<Vec<(HeaderMap, serde_json::Value)>>>;

    async fn serve(reply: (u16, serde_json::Value)) -> (String, Seen) {
        let seen: Seen = Arc::default();
        async fn h(
            State((seen, reply)): State<(Seen, (u16, serde_json::Value))>,
            headers: HeaderMap,
            Json(b): Json<serde_json::Value>,
        ) -> (axum::http::StatusCode, Json<serde_json::Value>) {
            seen.lock().unwrap().push((headers, b));
            (axum::http::StatusCode::from_u16(reply.0).unwrap(), Json(reply.1))
        }
        let app = Router::new().route("/v1/messages", post(h)).with_state((seen.clone(), reply));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        (url, seen)
    }

    #[tokio::test]
    async fn sends_a_forced_tool_call_and_reads_the_tool_input() {
        let (url, seen) = serve((200, json!({ "content": [{ "type": "text", "text": "thinking" }, { "type": "tool_use", "id": "t", "name": "submit_review", "input": { "verdict": "likely_meets" } }], "usage": { "input_tokens": 321, "output_tokens": 45 } }))).await;
        let m = AnthropicModel::with_base("sk-test-key-123", "claude-test", &url);
        let out = m.complete("SYS", "USER").await.unwrap();
        assert_eq!((out.output["verdict"].as_str(), out.input_tokens, out.output_tokens), (Some("likely_meets"), 321, 45));
        let (headers, body) = seen.lock().unwrap()[0].clone();
        assert_eq!(headers["x-api-key"], "sk-test-key-123");
        assert_eq!(headers["anthropic-version"], "2023-06-01");
        assert_eq!(body["model"], "claude-test");
        assert_eq!(body["system"], "SYS");
        assert_eq!(body["messages"][0]["content"], "USER");
        assert_eq!(body["tool_choice"], json!({ "type": "tool", "name": "submit_review" }));
        assert_eq!(body["tools"][0]["name"], "submit_review");
    }

    #[tokio::test]
    async fn errors_are_classified() {
        let (url, _) = serve((529, json!({ "error": "overloaded" }))).await;
        let e = AnthropicModel::with_base("k", "m", &url).complete("s", "u").await.err().unwrap();
        assert!(matches!(e, ModelError::Status(529)) && e.is_transient());
        let (url, _) =
            serve((200, json!({ "content": [{ "type": "text", "text": "no tool" }], "usage": { "input_tokens": 1, "output_tokens": 1 } })))
                .await;
        assert!(matches!(AnthropicModel::with_base("k", "m", &url).complete("s", "u").await.err().unwrap(), ModelError::NoToolCall));
        let dead = AnthropicModel::with_base("k", "m", "http://127.0.0.1:1");
        assert!(dead.complete("s", "u").await.err().unwrap().is_transient());
    }

    #[test]
    fn the_key_never_appears_in_debug_output() {
        assert!(!format!("{:?}", AnthropicModel::new("sk-super-secret", "m")).contains("sk-super-secret"));
    }
}
