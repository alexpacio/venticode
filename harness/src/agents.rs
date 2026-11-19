//! Native agent messages; the host never substitutes its own model loop.
use crate::protocol::Agent;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::path::Path;

pub struct Session {
    agent: Agent,
    cwd: String,
    prompt: Option<String>,
    stage: u8,
    thread_id: Option<String>,
    session_id: Option<String>,
    turn_seq: u64,
}
impl Session {
    pub fn new(agent: Agent, cwd: &Path, prompt: Option<&str>) -> Result<Self> {
        ensure!(
            prompt.is_none_or(|v| !v.is_empty() && v.len() <= 32768),
            "Invalid agent prompt"
        );
        Ok(Self {
            agent,
            cwd: cwd.to_str().context("Invalid working directory")?.into(),
            prompt: prompt.map(str::to_owned),
            stage: 0,
            thread_id: None,
            session_id: None,
            turn_seq: 0,
        })
    }
    fn format_prompt(&mut self, text: &str) -> Result<Value> {
        self.turn_seq += 1;
        let id = if self.turn_seq == 1 {
            "ventiharness:prompt".to_owned()
        } else {
            format!("ventiharness:prompt:{}", self.turn_seq)
        };
        Ok(match self.agent {
            Agent::ClaudeCode => {
                json!({"type":"user","message":{"role":"user","content":text},"parent_tool_use_id":null,"session_id":""})
            }
            Agent::Pi => json!({"type":"prompt","message":text}),
            Agent::Codex => {
                let thread = self.thread_id.as_deref().context("Missing Codex thread")?;
                json!({"id":id,"method":"turn/start","params":{"threadId":thread,"input":[{"type":"text","text":text,"text_elements":[]}]}})
            }
            _ => {
                let session = self.session_id.as_deref().context("Missing ACP session")?;
                json!({"jsonrpc":"2.0","id":id,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":text}]}})
            }
        })
    }
    pub fn start(&mut self) -> Vec<Value> {
        match self.agent {
            Agent::ClaudeCode | Agent::Pi => {
                self.stage = 3;
                if let Some(prompt) = self.prompt.take() {
                    self.format_prompt(&prompt).into_iter().collect()
                } else {
                    vec![]
                }
            }
            Agent::Codex => {
                self.stage = 1;
                vec![
                    json!({"id":"ventiharness:init","method":"initialize","params":{"clientInfo":{"name":"ventiharness","version":env!("CARGO_PKG_VERSION")},"capabilities":{}}}),
                ]
            }
            _ => {
                self.stage = 1;
                vec![
                    json!({"jsonrpc":"2.0","id":"ventiharness:init","method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false},"clientInfo":{"name":"ventiharness","version":env!("CARGO_PKG_VERSION")}}}),
                ]
            }
        }
    }
    pub fn prompt(&mut self, prompt: &str) -> Result<Vec<Value>> {
        ensure!(
            !prompt.is_empty() && prompt.len() <= 32768,
            "Invalid agent prompt"
        );
        if self.stage < 3 {
            self.prompt = Some(prompt.to_owned());
            return Ok(vec![]);
        }
        Ok(vec![self.format_prompt(prompt)?])
    }
    pub fn receive(&mut self, event: &Value) -> Result<Vec<Value>> {
        let id = event["id"].as_str().unwrap_or("");
        if id == "ventiharness:init" && self.stage == 1 {
            ensure!(event["error"].is_null(), "Agent initialization failed");
            self.stage = 2;
            return Ok(if self.agent == Agent::Codex {
                vec![
                    json!({"method":"initialized"}),
                    json!({"id":"ventiharness:session","method":"thread/start","params":{"cwd":self.cwd}}),
                ]
            } else {
                ensure!(
                    event["result"]["protocolVersion"] == 1,
                    "Agent does not support ACP v1"
                );
                vec![
                    json!({"jsonrpc":"2.0","id":"ventiharness:session","method":"session/new","params":{"cwd":self.cwd,"mcpServers":[]}}),
                ]
            });
        }
        if id == "ventiharness:session" && self.stage == 2 {
            ensure!(event["error"].is_null(), "Agent session creation failed");
            self.stage = 3;
            if self.agent == Agent::Codex {
                let thread = event["result"]["thread"]["id"]
                    .as_str()
                    .context("Missing Codex thread")?;
                self.thread_id = Some(thread.to_owned());
                return if let Some(prompt) = self.prompt.take() {
                    Ok(vec![self.format_prompt(&prompt)?])
                } else {
                    Ok(vec![])
                };
            }
            let session = event["result"]["sessionId"]
                .as_str()
                .context("Missing ACP session")?;
            self.session_id = Some(session.to_owned());
            return if let Some(prompt) = self.prompt.take() {
                Ok(vec![self.format_prompt(&prompt)?])
            } else {
                Ok(vec![])
            };
        }
        Ok(vec![])
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn codex_initializes_once_before_starting_a_turn_and_supports_followup() {
        let mut session =
            Session::new(Agent::Codex, Path::new("/project"), Some("fix test")).unwrap();
        assert_eq!(session.start()[0]["method"], "initialize");
        let ready = json!({"id":"ventiharness:init","result":{}});
        assert_eq!(
            session.receive(&ready).unwrap()[1]["method"],
            "thread/start"
        );
        assert!(session.receive(&ready).unwrap().is_empty());
        let event = json!({"id":"ventiharness:session","result":{"thread":{"id":"native-thread"}}});
        assert_eq!(
            session.receive(&event).unwrap()[0]["params"]["threadId"],
            "native-thread"
        );
        assert!(session.receive(&event).unwrap().is_empty());
        let follow = session.prompt("now add tests").unwrap();
        assert_eq!(follow[0]["method"], "turn/start");
        assert_eq!(follow[0]["params"]["threadId"], "native-thread");
        assert_eq!(follow[0]["params"]["input"][0]["text"], "now add tests");
    }
    #[test]
    fn incompatible_acp_does_not_start_a_session() {
        let mut session =
            Session::new(Agent::Gemini, Path::new("/project"), Some("fix test")).unwrap();
        session.start();
        assert!(
            session
                .receive(&json!({"id":"ventiharness:init","result":{"protocolVersion":999}}))
                .is_err()
        );
    }
    #[test]
    fn promptless_codex_and_acp_sessions_still_complete_handshake_and_followup() {
        let mut codex = Session::new(Agent::Codex, Path::new("/project"), None).unwrap();
        assert_eq!(codex.start()[0]["method"], "initialize");
        // Queued before handshake completes
        assert!(codex.prompt("queued prompt").unwrap().is_empty());
        let ready = json!({"id":"ventiharness:init","result":{}});
        assert_eq!(codex.receive(&ready).unwrap()[1]["method"], "thread/start");
        let started = json!({"id":"ventiharness:session","result":{"thread":{"id":"t1"}}});
        let emitted = codex.receive(&started).unwrap();
        assert_eq!(emitted[0]["params"]["threadId"], "t1");
        assert_eq!(emitted[0]["params"]["input"][0]["text"], "queued prompt");

        let mut gemini = Session::new(Agent::Gemini, Path::new("/project"), None).unwrap();
        assert_eq!(gemini.start()[0]["method"], "initialize");
        let init = json!({"id":"ventiharness:init","result":{"protocolVersion":1}});
        assert_eq!(gemini.receive(&init).unwrap()[0]["method"], "session/new");
        let acp_started = json!({"id":"ventiharness:session","result":{"sessionId":"s1"}});
        assert!(gemini.receive(&acp_started).unwrap().is_empty());
        let acp_follow = gemini.prompt("follow up acp").unwrap();
        assert_eq!(acp_follow[0]["method"], "session/prompt");
        assert_eq!(acp_follow[0]["params"]["sessionId"], "s1");

        let mut claude = Session::new(Agent::ClaudeCode, Path::new("/project"), None).unwrap();
        assert!(claude.start().is_empty());
        let claude_follow = claude.prompt("hello claude").unwrap();
        assert_eq!(claude_follow[0]["message"]["content"], "hello claude");

        let mut pi = Session::new(Agent::Pi, Path::new("/project"), None).unwrap();
        assert!(pi.start().is_empty());
        let pi_follow = pi.prompt("hello pi").unwrap();
        assert_eq!(pi_follow[0]["message"], "hello pi");
    }
}
