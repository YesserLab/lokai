//! Minimal OpenAI-compatible client: model list and streaming chat completions (SSE).

use crate::config::Config;
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::time::{Duration, Instant};

pub struct Client {
    agent: ureq::Agent,
    base: String,
    key: String,
}

impl Client {
    pub fn new(cfg: &Config) -> Self {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(5)))
            .timeout_per_read(Some(Duration::from_secs(cfg.idle_timeout_secs.max(5))))
            .build()
            .into();
        Self { agent, base: cfg.api_base(), key: cfg.api_key.trim().to_string() }
    }

    fn auth(&self) -> Option<String> {
        (!self.key.is_empty()).then(|| format!("Bearer {}", self.key))
    }

    /// Lists model ids and measures the round-trip time.
    pub fn models(&self) -> Result<(Vec<String>, Duration), String> {
        let start = Instant::now();
        let mut req = self.agent.get(format!("{}/models", self.base)).config().timeout_global(Some(Duration::from_secs(8))).build();
        if let Some(a) = self.auth() {
            req = req.header("Authorization", a);
        }
        let mut resp = req.call().map_err(|e| format!("Cannot reach {}: {e}", self.base))?;
        let latency = start.elapsed();
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().map_err(|e| e.to_string())?;
        if status != 200 {
            return Err(format!("Server answered HTTP {status}"));
        }
        let json: Value = serde_json::from_str(&text).map_err(|_| "Unexpected /models response".to_string())?;
        let list = json["data"].as_array().or_else(|| json["models"].as_array()).cloned().unwrap_or_default();
        let mut ids: Vec<String> =
            list.iter().filter_map(|m| m["id"].as_str().or(m["name"].as_str()).or(m["model"].as_str()).map(String::from)).collect();
        ids.sort();
        ids.dedup();
        Ok((ids, latency))
    }

    /// POSTs a streaming chat completion and calls `on_chunk` for every SSE event.
    /// `on_chunk` returns false to abort.
    pub fn chat_stream(&self, body: &Value, mut on_chunk: impl FnMut(&Value) -> bool) -> Result<(), String> {
        let mut req = self.agent.post(format!("{}/chat/completions", self.base)).header("Content-Type", "application/json");
        if let Some(a) = self.auth() {
            req = req.header("Authorization", a);
        }
        let mut resp = req.send(body.to_string()).map_err(|e| match e {
            ureq::Error::Timeout(_) => "The server did not respond in time.".to_string(),
            e => format!("Could not reach {}: {e}", self.base),
        })?;
        let status = resp.status().as_u16();
        if status != 200 {
            let text = resp.body_mut().read_to_string().unwrap_or_default();
            return Err(server_error(status, &text));
        }
        let reader = BufReader::new(resp.body_mut().as_reader());
        for line in reader.lines() {
            let line = line.map_err(|e| {
                if e.kind() == std::io::ErrorKind::TimedOut {
                    "The server stopped sending data (idle timeout).".to_string()
                } else {
                    format!("Connection lost: {e}")
                }
            })?;
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data == "[DONE]" {
                break;
            }
            if let Ok(v) = serde_json::from_str::<Value>(data)
                && !on_chunk(&v)
            {
                return Ok(());
            }
        }
        Ok(())
    }
}

fn server_error(status: u16, body: &str) -> String {
    let detail = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|j| j["error"]["message"].as_str().or(j["error"].as_str()).or(j["message"].as_str()).map(String::from))
        .unwrap_or_else(|| body.chars().take(600).collect());
    let mut msg = format!("Server error {status}: {detail}");
    let lower = detail.to_lowercase();
    if lower.contains("tool") && lower.contains("support") {
        msg.push_str("\n\nThis model does not seem to support tool calling. Pick another model or disable tools in the settings.");
    }
    msg
}

#[derive(Clone, Debug, Default)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Accumulates one streamed assistant turn.
#[derive(Default)]
pub struct Turn {
    pub content: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Value>,
    pub timings: Option<Value>,
    pub error: Option<String>,
    /// Number of chunks that carried generated text; ~ tokens for most servers.
    pub deltas: usize,
}

impl Turn {
    pub fn apply(&mut self, chunk: &Value) {
        if let Some(err) = chunk.get("error") {
            self.error = Some(err["message"].as_str().or(err.as_str()).map(String::from).unwrap_or_else(|| err.to_string()));
            return;
        }
        if chunk["usage"].is_object() {
            self.usage = Some(chunk["usage"].clone());
        }
        if chunk["timings"].is_object() {
            self.timings = Some(chunk["timings"].clone());
        }
        for choice in chunk["choices"].as_array().into_iter().flatten() {
            let d = if choice["delta"].is_object() { &choice["delta"] } else { &choice["message"] };
            let mut produced = false;
            if let Some(s) = d["content"].as_str() {
                produced |= !s.is_empty();
                self.content.push_str(s);
            }
            if let Some(s) = d["reasoning_content"].as_str().or(d["reasoning"].as_str()) {
                produced |= !s.is_empty();
                self.reasoning.push_str(s);
            }
            for tc in d["tool_calls"].as_array().into_iter().flatten() {
                produced = true;
                self.merge_tool_call(tc);
            }
            if produced {
                self.deltas += 1;
            }
        }
    }

    fn merge_tool_call(&mut self, tc: &Value) {
        let idx = match (tc["index"].as_u64(), tc["id"].as_str()) {
            (Some(i), _) => i as usize,
            (None, Some(id)) => self.tool_calls.iter().position(|c| c.id == id).unwrap_or(self.tool_calls.len()),
            (None, None) => self.tool_calls.len().saturating_sub(1),
        };
        if self.tool_calls.len() <= idx {
            self.tool_calls.resize_with(idx + 1, ToolCall::default);
        }
        let dst = &mut self.tool_calls[idx];
        if let Some(id) = tc["id"].as_str() {
            dst.id = id.to_string();
        }
        let f = &tc["function"];
        if let Some(n) = f["name"].as_str() {
            dst.name.push_str(n);
        }
        match &f["arguments"] {
            Value::String(s) => dst.arguments.push_str(s),
            Value::Null => {}
            other => dst.arguments.push_str(&other.to_string()),
        }
    }

    /// Visible answer and reasoning, with inline `<think>` blocks moved into the reasoning.
    pub fn split(&self) -> (String, String) {
        let (content, think) = split_thinking(&self.content);
        let reasoning = match (self.reasoning.trim(), think.trim()) {
            (a, "") => a.to_string(),
            ("", b) => b.to_string(),
            (a, b) => format!("{a}\n{b}"),
        };
        (content, reasoning)
    }

    /// Tokens per second as reported by the server (llama.cpp timings), if available.
    pub fn server_speed(&self) -> Option<f32> {
        self.timings.as_ref()?["predicted_per_second"].as_f64().map(|v| v as f32).filter(|v| *v > 0.0)
    }

    pub fn completion_tokens(&self) -> Option<u64> {
        self.usage.as_ref()?["completion_tokens"].as_u64()
    }

    pub fn total_tokens(&self) -> Option<u64> {
        let u = self.usage.as_ref()?;
        u["total_tokens"].as_u64().or_else(|| Some(u["prompt_tokens"].as_u64()? + u["completion_tokens"].as_u64().unwrap_or(0)))
    }
}

/// Splits `<think>…</think>` (qwen3, deepseek-r1, …) out of the content.
pub fn split_thinking(content: &str) -> (String, String) {
    let mut visible = String::new();
    let mut reasoning = String::new();
    let mut rest = content;
    // Templates that put the opening tag in the prompt only emit the closing one.
    if !rest.contains("<think>")
        && let Some(end) = rest.find("</think>")
    {
        reasoning.push_str(&rest[..end]);
        rest = &rest[end + 8..];
    }
    while let Some(start) = rest.find("<think>") {
        visible.push_str(&rest[..start]);
        let after = &rest[start + 7..];
        match after.find("</think>") {
            Some(end) => {
                reasoning.push_str(&after[..end]);
                rest = &after[end + 8..];
            }
            None => {
                reasoning.push_str(after);
                rest = "";
            }
        }
    }
    visible.push_str(rest);
    (visible.trim_start().to_string(), reasoning.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn streamed_tool_call_and_thinking() {
        let mut t = Turn::default();
        t.apply(&json!({"choices":[{"delta":{"content":"<think>hmm"}}]}));
        t.apply(&json!({"choices":[{"delta":{"content":"</think>Hello"}}]}));
        t.apply(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"run_","arguments":"{\"comm"}}]}}]}));
        t.apply(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"command","arguments":"and\":\"ls\"}"}}]}}]}));
        t.apply(&json!({"choices":[],"usage":{"total_tokens":42,"completion_tokens":5},"timings":{"predicted_per_second":33.5}}));
        assert_eq!(t.tool_calls[0].name, "run_command");
        assert_eq!(t.tool_calls[0].arguments, r#"{"command":"ls"}"#);
        assert_eq!(t.split(), ("Hello".to_string(), "hmm".to_string()));
        assert_eq!(t.total_tokens(), Some(42));
        assert_eq!(t.server_speed(), Some(33.5));
        assert_eq!(t.deltas, 4);
    }

    #[test]
    fn ollama_style_object_arguments() {
        let mut t = Turn::default();
        t.apply(&json!({"choices":[{"delta":{"tool_calls":[{"id":"x","function":{"name":"read_file","arguments":{"path":"/etc/hostname"}}}]}}]}));
        assert_eq!(t.tool_calls[0].arguments, r#"{"path":"/etc/hostname"}"#);
    }

    #[test]
    fn thinking_variants() {
        assert_eq!(split_thinking("reasoned</think>\n\nAnswer"), ("Answer".into(), "reasoned".into()));
        assert_eq!(split_thinking("<think>still going"), ("".into(), "still going".into()));
        assert_eq!(split_thinking("plain"), ("plain".into(), "".into()));
    }
}
