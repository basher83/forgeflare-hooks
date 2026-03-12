use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("API error: {0}")]
    Api(#[from] reqwest::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("HTTP {status}: {body}")]
    HttpError {
        status: u16,
        retry_after: Option<u64>,
        body: String,
    },

    #[error("Stream error (transient): {0}")]
    StreamTransient(String),

    #[error("Stream parse error: {0}")]
    StreamParse(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ErrorClass {
    Transient,
    Permanent,
}

pub fn classify_error(e: &AgentError) -> ErrorClass {
    match e {
        AgentError::HttpError { status, .. } => match *status {
            429 | 503 | 529 => ErrorClass::Transient,
            s if s >= 500 => ErrorClass::Transient,
            _ => ErrorClass::Permanent,
        },
        AgentError::StreamTransient(_) => ErrorClass::Transient,
        AgentError::StreamParse(_) => ErrorClass::Permanent,
        AgentError::Api(e) => {
            if e.is_timeout() || e.is_connect() {
                ErrorClass::Transient
            } else {
                ErrorClass::Permanent
            }
        }
        AgentError::Json(_) => ErrorClass::Permanent,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum StopReason {
    #[serde(rename = "end_turn")]
    EndTurn,
    #[serde(rename = "max_tokens")]
    MaxTokens,
    #[serde(rename = "tool_use")]
    ToolUse,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

pub struct AnthropicClient {
    client: Client,
    api_url: String,
    api_key: Option<String>,
}

impl AnthropicClient {
    pub fn new(api_url: &str) -> Self {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(300))
            .build()
            .expect("failed to build HTTP client");

        let api_key = std::env::var("ANTHROPIC_API_KEY").ok();

        Self {
            client,
            api_url: api_url.trim_end_matches('/').to_string(),
            api_key,
        }
    }

    pub fn api_url(&self) -> &str {
        &self.api_url
    }

    pub fn has_api_key(&self) -> bool {
        self.api_key.is_some()
    }

    pub async fn send_message(
        &self,
        model: &str,
        max_tokens: u32,
        system: &str,
        messages: &[Message],
        tools: &[serde_json::Value],
        stream_callback: &mut dyn FnMut(&str),
    ) -> Result<(Vec<ContentBlock>, StopReason, Usage), AgentError> {
        let url = format!("{}/v1/messages", self.api_url);

        let mut body = serde_json::json!({
            "model": model,
            "max_tokens": max_tokens,
            "system": [{
                "type": "text",
                "text": system,
                "cache_control": {"type": "ephemeral"}
            }],
            "messages": messages,
            "stream": true,
        });

        if !tools.is_empty() {
            let mut cached_tools = tools.to_vec();
            if let Some(last) = cached_tools.last_mut() {
                last["cache_control"] = serde_json::json!({"type": "ephemeral"});
            }
            body["tools"] = serde_json::Value::Array(cached_tools);
        }

        let mut req = self
            .client
            .post(&url)
            .header("anthropic-version", "2023-06-01");

        if let Some(ref key) = self.api_key {
            req = req.header("x-api-key", key);
        }

        let resp: reqwest::Response = req.json(&body).send().await?;

        let status = resp.status();
        if !status.is_success() {
            let retry_after: Option<u64> = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok());
            let body_text = resp.text().await.unwrap_or_default();
            return Err(AgentError::HttpError {
                status: status.as_u16(),
                retry_after,
                body: body_text,
            });
        }

        let stream = resp.bytes_stream();
        parse_sse_stream(stream, stream_callback).await
    }
}

/// Parse SSE stream into content blocks, stop reason, and usage.
///
/// We collect content_block_start events to initialize blocks, then
/// content_block_delta events to append text or accumulate tool input JSON,
/// message_start for input usage, and message_delta for stop_reason + output usage.
async fn parse_sse_stream<S>(
    stream: S,
    callback: &mut dyn FnMut(&str),
) -> Result<(Vec<ContentBlock>, StopReason, Usage), AgentError>
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
{
    let mut content_blocks: Vec<ContentBlock> = Vec::new();
    let mut stop_reason: Option<StopReason> = None;
    let mut usage = Usage::default();
    let mut buffer = String::new();

    // Track in-progress tool_use input JSON accumulation per block index
    let mut tool_input_bufs: std::collections::HashMap<usize, String> =
        std::collections::HashMap::new();

    futures_util::pin_mut!(stream);

    while let Some(chunk_result) = stream.next().await {
        let chunk = chunk_result.map_err(|e| AgentError::StreamTransient(e.to_string()))?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));

        // Process complete SSE lines
        while let Some(pos) = buffer.find("\n\n") {
            {
                let event_block = &buffer[..pos];

                for line in event_block.lines() {
                    if let Some(data) = line.strip_prefix("data: ") {
                        if data == "[DONE]" {
                            continue;
                        }

                        let parsed: serde_json::Value = match serde_json::from_str(data) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };

                        let event_type = parsed["type"].as_str().unwrap_or("");

                        match event_type {
                            "message_start" => {
                                if let Some(u) = parsed.get("message").and_then(|m| m.get("usage"))
                                {
                                    usage.input_tokens = u["input_tokens"].as_u64().unwrap_or(0);
                                    usage.cache_creation_input_tokens =
                                        u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
                                    usage.cache_read_input_tokens =
                                        u["cache_read_input_tokens"].as_u64().unwrap_or(0);
                                }
                            }
                            "content_block_start" => {
                                let cb = &parsed["content_block"];
                                match cb["type"].as_str() {
                                    Some("text") => {
                                        content_blocks.push(ContentBlock::Text {
                                            text: String::new(),
                                        });
                                    }
                                    Some("tool_use") => {
                                        let idx = content_blocks.len();
                                        content_blocks.push(ContentBlock::ToolUse {
                                        id: cb["id"].as_str().unwrap_or("").to_string(),
                                        name: cb["name"].as_str().unwrap_or("").to_string(),
                                        input: serde_json::Value::Object(serde_json::Map::new()),
                                    });
                                        tool_input_bufs.insert(idx, String::new());
                                    }
                                    _ => {}
                                }
                            }
                            "content_block_delta" => {
                                let index = parsed["index"].as_u64().unwrap_or(0) as usize;
                                let delta = &parsed["delta"];

                                match delta["type"].as_str() {
                                    Some("text_delta") => {
                                        if let Some(text) = delta["text"].as_str() {
                                            callback(text);
                                            if let Some(ContentBlock::Text { text: ref mut t }) =
                                                content_blocks.get_mut(index)
                                            {
                                                t.push_str(text);
                                            }
                                        }
                                    }
                                    Some("input_json_delta") => {
                                        if let Some(partial) = delta["partial_json"].as_str() {
                                            if let Some(buf) = tool_input_bufs.get_mut(&index) {
                                                buf.push_str(partial);
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            "content_block_stop" => {
                                let index = parsed["index"].as_u64().unwrap_or(0) as usize;
                                // Finalize tool_use input JSON
                                if let Some(json_str) = tool_input_bufs.remove(&index) {
                                    if let Some(ContentBlock::ToolUse { ref mut input, .. }) =
                                        content_blocks.get_mut(index)
                                    {
                                        match serde_json::from_str::<serde_json::Value>(&json_str) {
                                            Ok(v) => *input = v,
                                            Err(e) => {
                                                eprintln!(
                                                    "[error] Failed to parse tool input JSON \
                                                     for block {index}: {e}"
                                                );
                                                // Set to Null so run_pre_dispatch's null-input
                                                // check catches it and produces a clean error
                                                *input = serde_json::Value::Null;
                                            }
                                        }
                                    }
                                }
                            }
                            "message_delta" => {
                                if let Some(sr) = parsed["delta"]["stop_reason"].as_str() {
                                    stop_reason = match sr {
                                        "end_turn" => Some(StopReason::EndTurn),
                                        "max_tokens" => Some(StopReason::MaxTokens),
                                        "tool_use" => Some(StopReason::ToolUse),
                                        _ => None,
                                    };
                                }
                                if let Some(u) = parsed.get("usage") {
                                    usage.output_tokens = u["output_tokens"].as_u64().unwrap_or(0);
                                }
                            }
                            "error" => {
                                let err_type =
                                    parsed["error"]["type"].as_str().unwrap_or("unknown");
                                let err_msg = parsed["error"]["message"]
                                    .as_str()
                                    .unwrap_or("unknown error");
                                match err_type {
                                    "invalid_request_error" => {
                                        return Err(AgentError::StreamParse(format!(
                                            "{err_type}: {err_msg}"
                                        )));
                                    }
                                    _ => {
                                        // Unknown/absent error types default to transient —
                                        // server-side errors are usually temporary (spec R1).
                                        return Err(AgentError::StreamTransient(format!(
                                            "{err_type}: {err_msg}"
                                        )));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            buffer.drain(..pos + 2);
        }
    }

    let stop = stop_reason.ok_or_else(|| {
        AgentError::StreamTransient(
            "stream ended without stop_reason (connection drop)".to_string(),
        )
    })?;

    Ok((content_blocks, stop, usage))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_reason_serialization() {
        assert_eq!(
            serde_json::to_string(&StopReason::EndTurn).unwrap(),
            "\"end_turn\""
        );
        assert_eq!(
            serde_json::to_string(&StopReason::ToolUse).unwrap(),
            "\"tool_use\""
        );
        assert_eq!(
            serde_json::to_string(&StopReason::MaxTokens).unwrap(),
            "\"max_tokens\""
        );
    }

    #[test]
    fn content_block_text_roundtrip() {
        let block = ContentBlock::Text {
            text: "hello".to_string(),
        };
        let json = serde_json::to_string(&block).unwrap();
        assert!(json.contains("\"type\":\"text\""));
        assert!(json.contains("\"text\":\"hello\""));
    }

    #[test]
    fn content_block_tool_use_roundtrip() {
        let block = ContentBlock::ToolUse {
            id: "id123".to_string(),
            name: "Bash".to_string(),
            input: serde_json::json!({"command": "ls"}),
        };
        let json = serde_json::to_string(&block).unwrap();
        assert!(json.contains("\"type\":\"tool_use\""));
        assert!(json.contains("\"name\":\"Bash\""));
    }

    #[test]
    fn content_block_tool_result_roundtrip() {
        let block = ContentBlock::ToolResult {
            tool_use_id: "id123".to_string(),
            content: "output".to_string(),
            is_error: None,
        };
        let json = serde_json::to_string(&block).unwrap();
        assert!(json.contains("\"type\":\"tool_result\""));
        assert!(!json.contains("is_error")); // skipped when None
    }

    #[test]
    fn message_roundtrip() {
        let msg = Message {
            role: "user".to_string(),
            content: vec![ContentBlock::Text {
                text: "hi".to_string(),
            }],
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.role, "user");
    }

    #[tokio::test]
    async fn parse_sse_text_response() {
        let sse_data = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"role\":\"assistant\"}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello world\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

        let stream = futures_util::stream::iter(vec![Ok::<_, reqwest::Error>(bytes::Bytes::from(
            &sse_data[..],
        ))]);

        let mut streamed = String::new();
        let (blocks, stop, _usage) = parse_sse_stream(stream, &mut |text| {
            streamed.push_str(text);
        })
        .await
        .unwrap();

        assert_eq!(stop, StopReason::EndTurn);
        assert_eq!(blocks.len(), 1);
        assert_eq!(streamed, "Hello world");

        if let ContentBlock::Text { text } = &blocks[0] {
            assert_eq!(text, "Hello world");
        } else {
            panic!("expected Text block");
        }
    }

    #[tokio::test]
    async fn parse_sse_tool_use_response() {
        let sse_data = concat!(
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu_1\",\"name\":\"Read\",\"input\":{}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"file_path\\\": \\\"/tmp/test\\\"}\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n",
        );

        let stream =
            futures_util::stream::iter(vec![Ok::<_, reqwest::Error>(bytes::Bytes::from(sse_data))]);

        let (blocks, stop, _usage) = parse_sse_stream(stream, &mut |_| {}).await.unwrap();

        assert_eq!(stop, StopReason::ToolUse);
        assert_eq!(blocks.len(), 1);

        if let ContentBlock::ToolUse { id, name, input } = &blocks[0] {
            assert_eq!(id, "tu_1");
            assert_eq!(name, "Read");
            assert_eq!(input["file_path"], "/tmp/test");
        } else {
            panic!("expected ToolUse block");
        }
    }

    #[tokio::test]
    async fn parse_sse_error_event_transient() {
        let sse_data = concat!(
            "event: error\n",
            "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
        );

        let stream =
            futures_util::stream::iter(vec![Ok::<_, reqwest::Error>(bytes::Bytes::from(sse_data))]);

        let err = parse_sse_stream(stream, &mut |_| {}).await.unwrap_err();
        assert!(matches!(err, AgentError::StreamTransient(_)));
    }

    #[tokio::test]
    async fn parse_sse_error_event_permanent() {
        let sse_data = concat!(
            "event: error\n",
            "data: {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"Bad request\"}}\n\n",
        );

        let stream =
            futures_util::stream::iter(vec![Ok::<_, reqwest::Error>(bytes::Bytes::from(sse_data))]);

        let err = parse_sse_stream(stream, &mut |_| {}).await.unwrap_err();
        assert!(matches!(err, AgentError::StreamParse(_)));
    }

    #[tokio::test]
    async fn parse_sse_unknown_error_type_is_transient() {
        // Unknown error types from the API should be transient (retryable),
        // not permanent — server-side errors are usually temporary.
        let sse_data = concat!(
            "event: error\n",
            "data: {\"type\":\"error\",\"error\":{\"type\":\"some_future_error\",\"message\":\"Something new\"}}\n\n",
        );

        let stream =
            futures_util::stream::iter(vec![Ok::<_, reqwest::Error>(bytes::Bytes::from(sse_data))]);

        let err = parse_sse_stream(stream, &mut |_| {}).await.unwrap_err();
        assert!(
            matches!(err, AgentError::StreamTransient(_)),
            "unknown error type should be transient, got: {err}"
        );
    }

    #[tokio::test]
    async fn parse_sse_missing_stop_reason() {
        let sse_data = concat!(
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
        );

        let stream =
            futures_util::stream::iter(vec![Ok::<_, reqwest::Error>(bytes::Bytes::from(sse_data))]);

        let err = parse_sse_stream(stream, &mut |_| {}).await.unwrap_err();
        assert!(matches!(err, AgentError::StreamTransient(_)));
    }

    #[test]
    fn client_construction_with_url() {
        // Just verify it doesn't panic
        let client = AnthropicClient::new("https://example.com");
        assert_eq!(client.api_url(), "https://example.com");
    }

    #[test]
    fn client_strips_trailing_slash() {
        let client = AnthropicClient::new("https://example.com/");
        assert_eq!(client.api_url(), "https://example.com");
    }

    #[test]
    fn classify_http_429_transient() {
        let e = AgentError::HttpError {
            status: 429,
            retry_after: Some(5),
            body: "rate limited".to_string(),
        };
        assert_eq!(classify_error(&e), ErrorClass::Transient);
    }

    #[test]
    fn classify_http_503_transient() {
        let e = AgentError::HttpError {
            status: 503,
            retry_after: None,
            body: "overloaded".to_string(),
        };
        assert_eq!(classify_error(&e), ErrorClass::Transient);
    }

    #[test]
    fn classify_http_529_transient() {
        let e = AgentError::HttpError {
            status: 529,
            retry_after: None,
            body: "overloaded".to_string(),
        };
        assert_eq!(classify_error(&e), ErrorClass::Transient);
    }

    #[test]
    fn classify_http_500_transient() {
        let e = AgentError::HttpError {
            status: 500,
            retry_after: None,
            body: "internal error".to_string(),
        };
        assert_eq!(classify_error(&e), ErrorClass::Transient);
    }

    #[test]
    fn classify_http_400_permanent() {
        let e = AgentError::HttpError {
            status: 400,
            retry_after: None,
            body: "bad request".to_string(),
        };
        assert_eq!(classify_error(&e), ErrorClass::Permanent);
    }

    #[test]
    fn classify_http_401_permanent() {
        let e = AgentError::HttpError {
            status: 401,
            retry_after: None,
            body: "unauthorized".to_string(),
        };
        assert_eq!(classify_error(&e), ErrorClass::Permanent);
    }

    #[test]
    fn classify_http_403_permanent() {
        let e = AgentError::HttpError {
            status: 403,
            retry_after: None,
            body: "forbidden".to_string(),
        };
        assert_eq!(classify_error(&e), ErrorClass::Permanent);
    }

    #[test]
    fn classify_stream_transient() {
        let e = AgentError::StreamTransient("overloaded_error: Overloaded".to_string());
        assert_eq!(classify_error(&e), ErrorClass::Transient);
    }

    #[test]
    fn classify_stream_parse_permanent() {
        let e = AgentError::StreamParse("invalid_request_error: Bad request".to_string());
        assert_eq!(classify_error(&e), ErrorClass::Permanent);
    }

    #[test]
    fn classify_json_permanent() {
        let e: AgentError = serde_json::from_str::<serde_json::Value>("not json")
            .unwrap_err()
            .into();
        assert_eq!(classify_error(&e), ErrorClass::Permanent);
    }

    #[tokio::test]
    async fn parse_sse_usage_from_message_start_and_delta() {
        let sse_data = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"role\":\"assistant\",\"usage\":{\"input_tokens\":1500,\"cache_creation_input_tokens\":200,\"cache_read_input_tokens\":800}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":350}}\n\n",
        );

        let stream =
            futures_util::stream::iter(vec![Ok::<_, reqwest::Error>(bytes::Bytes::from(sse_data))]);

        let (_blocks, _stop, usage) = parse_sse_stream(stream, &mut |_| {}).await.unwrap();

        assert_eq!(usage.input_tokens, 1500);
        assert_eq!(usage.output_tokens, 350);
        assert_eq!(usage.cache_creation_input_tokens, 200);
        assert_eq!(usage.cache_read_input_tokens, 800);
    }

    #[test]
    fn usage_default_is_zeros() {
        let u = Usage::default();
        assert_eq!(u.input_tokens, 0);
        assert_eq!(u.output_tokens, 0);
        assert_eq!(u.cache_creation_input_tokens, 0);
        assert_eq!(u.cache_read_input_tokens, 0);
    }

    #[test]
    fn system_prompt_sent_as_cached_content_block_array() {
        let system = "You are a coding assistant";
        let body = serde_json::json!({
            "system": [{
                "type": "text",
                "text": system,
                "cache_control": {"type": "ephemeral"}
            }]
        });

        let system_val = &body["system"];
        assert!(system_val.is_array(), "system must be an array");
        let arr = system_val.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["type"], "text");
        assert_eq!(arr[0]["text"], system);
        assert_eq!(arr[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn cache_control_added_to_last_tool_only() {
        let tools = vec![
            serde_json::json!({"name": "Read", "description": "Read files"}),
            serde_json::json!({"name": "Bash", "description": "Run commands"}),
            serde_json::json!({"name": "Grep", "description": "Search files"}),
        ];

        let mut cached_tools = tools.clone();
        if let Some(last) = cached_tools.last_mut() {
            last["cache_control"] = serde_json::json!({"type": "ephemeral"});
        }

        // Original tools unmodified
        assert!(tools[0].get("cache_control").is_none());
        assert!(tools[1].get("cache_control").is_none());
        assert!(tools[2].get("cache_control").is_none());

        // Only last cached tool has cache_control
        assert!(cached_tools[0].get("cache_control").is_none());
        assert!(cached_tools[1].get("cache_control").is_none());
        assert_eq!(cached_tools[2]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn cache_control_with_single_tool() {
        let tools = vec![serde_json::json!({"name": "Read", "description": "Read files"})];

        let mut cached_tools = tools.to_vec();
        if let Some(last) = cached_tools.last_mut() {
            last["cache_control"] = serde_json::json!({"type": "ephemeral"});
        }

        assert_eq!(cached_tools[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn cache_control_with_empty_tools() {
        let tools: Vec<serde_json::Value> = vec![];
        let mut body = serde_json::json!({"model": "test"});

        // Mirrors send_message logic — no tools means no tools key
        if !tools.is_empty() {
            let mut cached_tools = tools.to_vec();
            if let Some(last) = cached_tools.last_mut() {
                last["cache_control"] = serde_json::json!({"type": "ephemeral"});
            }
            body["tools"] = serde_json::Value::Array(cached_tools);
        }

        assert!(body.get("tools").is_none());
    }

    #[tokio::test]
    async fn parse_sse_malformed_tool_input_sets_null() {
        // Simulate truncated/malformed tool input JSON: partial JSON that
        // doesn't close properly. On content_block_stop, the parse should
        // fail and set input to Value::Null instead of silently leaving {}.
        let sse_data = concat!(
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu_bad\",\"name\":\"Read\",\"input\":{}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"file_path\\\": \\\"/tmp/te\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n",
        );

        let stream =
            futures_util::stream::iter(vec![Ok::<_, reqwest::Error>(bytes::Bytes::from(sse_data))]);

        let (blocks, _stop, _usage) = parse_sse_stream(stream, &mut |_| {}).await.unwrap();

        assert_eq!(blocks.len(), 1);
        if let ContentBlock::ToolUse { id, input, .. } = &blocks[0] {
            assert_eq!(id, "tu_bad");
            assert!(
                input.is_null(),
                "malformed JSON should set input to Null, got: {input}"
            );
        } else {
            panic!("expected ToolUse block");
        }
    }
}
