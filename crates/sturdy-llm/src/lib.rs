//! # sturdy-llm
//!
//! An LLM-backed [`Reasoner`] for the engine. It speaks the OpenAI-compatible
//! `/chat/completions` API, which means the same client drives **OpenAI, Ollama
//! (`:11434/v1`), vLLM, LM Studio, and llama.cpp** — anything with that surface.
//!
//! The model is prompted to emit a strict ReAct JSON object each turn
//! (`{"thought": ..., "action": ...}`), which is parsed straight into a
//! [`Decision`] reusing `sturdy-core`'s [`Action`] type. The prompt-building and
//! response-parsing are pure functions (tested without a network); only
//! [`ChatReasoner::next_action`] performs I/O.

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use sturdy_core::{Action, Decision, HarnessError, Reasoner, Task, Thought, Trajectory};

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("http error: {0}")]
    Http(String),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("could not parse a ReAct action from the model output: {0}")]
    Parse(String),
}

impl From<LlmError> for HarnessError {
    fn from(e: LlmError) -> Self {
        HarnessError::Reasoner(e.to_string())
    }
}

/// A tool advertised to the model in the system prompt.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

impl ToolSpec {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: serde_json::Value,
    ) -> Self {
        ToolSpec {
            name: name.into(),
            description: description.into(),
            input_schema,
        }
    }
}

/// A `Reasoner` backed by an OpenAI-compatible chat endpoint.
pub struct ChatReasoner {
    http: reqwest::Client,
    /// Base URL without a trailing slash, e.g. `http://localhost:11434/v1`.
    base_url: String,
    model: String,
    api_key: Option<String>,
    tools: Vec<ToolSpec>,
    temperature: f32,
}

impl ChatReasoner {
    /// Build a reasoner against any OpenAI-compatible endpoint.
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: Option<String>,
        tools: Vec<ToolSpec>,
    ) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .unwrap_or_default();
        ChatReasoner {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            model: model.into(),
            api_key,
            tools,
            temperature: 0.2,
        }
    }

    /// Convenience: a local Ollama server (`http://localhost:11434/v1`).
    pub fn ollama(model: impl Into<String>, tools: Vec<ToolSpec>) -> Self {
        Self::new("http://localhost:11434/v1", model, None, tools)
    }

    /// Override the sampling temperature (default 0.2).
    pub fn with_temperature(mut self, t: f32) -> Self {
        self.temperature = t;
        self
    }

    fn system_prompt(&self) -> String {
        let mut s = String::from(
            "You are an autonomous software-engineering agent running in a strict ReAct loop.\n\
             Reply on every turn with a SINGLE JSON object and nothing else:\n\
             {\"thought\": \"<concise reasoning>\", \"action\": <action>}\n\
             where <action> is exactly one of:\n\
             {\"kind\": \"tool\", \"name\": \"<tool>\", \"arguments\": { ... }}   to call a tool, or\n\
             {\"kind\": \"finish\", \"answer\": \"<final answer>\"}               when the goal is met.\n\n\
             Rules:\n\
             - Think step by step; use tools to inspect and change things before finishing.\n\
             - Use ONLY the tools listed below, with arguments matching their schema.\n\
             - Output raw JSON — no markdown, no prose around it.\n\n",
        );
        if self.tools.is_empty() {
            s.push_str("No tools are available; reason briefly and then finish.\n");
        } else {
            s.push_str("Available tools:\n");
            for t in &self.tools {
                s.push_str(&format!(
                    "- {}: {}\n  arguments schema: {}\n",
                    t.name, t.description, t.input_schema
                ));
            }
        }
        s
    }

    /// Turn the task + trajectory into a chat message list (pure; testable).
    fn build_messages(&self, task: &Task, trajectory: &Trajectory) -> Vec<ChatMessage> {
        let mut messages = vec![ChatMessage::new("system", self.system_prompt())];
        let mut user = format!("Goal: {}", task.goal);
        if let Some(ws) = &task.workspace {
            user.push_str(&format!("\nWorkspace: {ws}"));
        }
        messages.push(ChatMessage::new("user", user));

        for step in &trajectory.steps {
            let assistant = serde_json::json!({ "thought": step.thought.0, "action": step.action });
            messages.push(ChatMessage::new("assistant", assistant.to_string()));
            if let Some(obs) = &step.observation {
                let label = if obs.is_error {
                    "Observation (error)"
                } else {
                    "Observation"
                };
                messages.push(ChatMessage::new(
                    "user",
                    format!("{label}: {}", truncate(&obs.content, 4000)),
                ));
            }
        }
        messages
    }
}

#[async_trait]
impl Reasoner for ChatReasoner {
    async fn next_action(
        &self,
        task: &Task,
        trajectory: &Trajectory,
    ) -> sturdy_core::Result<Decision> {
        let messages = self.build_messages(task, trajectory);
        let body = ChatRequest {
            model: &self.model,
            messages: &messages,
            temperature: self.temperature,
            response_format: Some(ResponseFormat {
                kind: "json_object",
            }),
            stream: false,
        };
        let url = format!("{}/chat/completions", self.base_url);

        let mut request = self.http.post(&url).json(&body);
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        let resp = request
            .send()
            .await
            .map_err(|e| HarnessError::Reasoner(format!("request to {url} failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let detail = resp.text().await.unwrap_or_default();
            return Err(HarnessError::Reasoner(format!(
                "{status}: {}",
                truncate(&detail, 300)
            )));
        }

        let parsed: ChatResponse = resp
            .json()
            .await
            .map_err(|e| HarnessError::Reasoner(format!("decoding chat response: {e}")))?;
        let content = parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .ok_or_else(|| HarnessError::Reasoner("model returned no content".into()))?;
        let tokens = parsed.usage.map(|u| u.total_tokens).unwrap_or(0);

        parse_decision(&content, tokens).map_err(|e| {
            HarnessError::Reasoner(format!("{e} — raw output: {}", truncate(&content, 300)))
        })
    }
}

/// Parse the model's JSON into a [`Decision`]. Tolerant of a model that wraps the
/// object in stray prose/markdown by extracting the first balanced JSON object.
pub fn parse_decision(content: &str, tokens: u64) -> Result<Decision, LlmError> {
    let json = extract_json_object(content)
        .ok_or_else(|| LlmError::Parse("no JSON object found in output".into()))?;
    let model: ModelResponse = serde_json::from_str(json)?;
    Ok(Decision {
        thought: Thought(model.thought),
        action: model.action,
        tokens,
    })
}

/// Return the first balanced `{...}` object in `s`, respecting string literals.
fn extract_json_object(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    let start = bytes.iter().position(|&b| b == b'{')?;
    let (mut depth, mut in_str, mut esc) = (0i32, false, false);
    for i in start..bytes.len() {
        let b = bytes[i];
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
        } else {
            match b {
                b'"' => in_str = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&s[start..=i]);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        format!("{}…", s.chars().take(max).collect::<String>())
    } else {
        s.to_string()
    }
}

// ── wire types ──

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    temperature: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
    stream: bool,
}

#[derive(Serialize)]
struct ResponseFormat {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Serialize, Clone)]
struct ChatMessage {
    role: String,
    content: String,
}

impl ChatMessage {
    fn new(role: &str, content: impl Into<String>) -> Self {
        ChatMessage {
            role: role.to_string(),
            content: content.into(),
        }
    }
}

#[derive(Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Choice {
    message: RespMessage,
}

#[derive(Deserialize)]
struct RespMessage {
    #[serde(default)]
    content: Option<String>,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    total_tokens: u64,
}

/// The ReAct object the model is asked to emit.
#[derive(Deserialize)]
struct ModelResponse {
    #[serde(default)]
    thought: String,
    action: Action,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn parses_a_clean_tool_decision() {
        let content = r#"{"thought":"look around","action":{"kind":"tool","name":"shell","arguments":{"cmd":"ls"}}}"#;
        let d = parse_decision(content, 42).unwrap();
        assert_eq!(d.thought.0, "look around");
        assert_eq!(d.tokens, 42);
        match d.action {
            Action::Tool(c) => {
                assert_eq!(c.name, "shell");
                assert_eq!(c.arguments["cmd"], "ls");
            }
            _ => panic!("expected a tool action"),
        }
    }

    #[test]
    fn parses_a_finish_decision() {
        let d = parse_decision(
            r#"{"thought":"done","action":{"kind":"finish","answer":"42"}}"#,
            5,
        )
        .unwrap();
        assert!(matches!(d.action, Action::Finish { .. }));
    }

    #[test]
    fn recovers_json_wrapped_in_prose() {
        // A model that ignores json-mode and adds markdown fences / commentary.
        let content = "Sure! Here you go:\n```json\n{\"thought\":\"x\",\"action\":{\"kind\":\"finish\",\"answer\":\"ok {nested}\"}}\n```\nHope that helps.";
        let d = parse_decision(content, 0).unwrap();
        match d.action {
            Action::Finish { answer } => assert_eq!(answer, "ok {nested}"),
            _ => panic!("expected finish"),
        }
    }

    #[test]
    fn rejects_output_with_no_json() {
        assert!(parse_decision("I refuse to answer.", 0).is_err());
    }

    #[tokio::test]
    async fn drives_a_mock_openai_endpoint_end_to_end() {
        // A minimal mock chat-completions server on an ephemeral port.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 16 * 1024];
            let _ = sock.read(&mut buf).await; // consume the request (small)
            let reply = serde_json::json!({
                "choices": [ { "message": { "role": "assistant",
                    "content": "{\"thought\":\"inspect\",\"action\":{\"kind\":\"tool\",\"name\":\"read_file\",\"arguments\":{\"path\":\"a.rs\"}}}" } } ],
                "usage": { "total_tokens": 137 }
            })
            .to_string();
            let http = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                reply.len(),
                reply
            );
            sock.write_all(http.as_bytes()).await.unwrap();
            let _ = sock.flush().await;
        });

        let reasoner = ChatReasoner::new(format!("http://{addr}"), "mock-model", None, vec![]);
        let task = Task::new("read the file");
        let traj = Trajectory::new(task.id);
        let decision = reasoner.next_action(&task, &traj).await.unwrap();

        assert_eq!(decision.tokens, 137);
        assert_eq!(decision.thought.0, "inspect");
        match decision.action {
            Action::Tool(c) => assert_eq!(c.name, "read_file"),
            _ => panic!("expected a tool call"),
        }
    }
}
