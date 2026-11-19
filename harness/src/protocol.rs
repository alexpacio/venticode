use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Sandboxed,
    Yolo,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub consent_id: Uuid,
    pub root: String,
    pub operation: String,
    pub arguments: Value,
    pub mcp_grants: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub mode: Mode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Agent {
    ClaudeCode,
    Codex,
    Cursor,
    Grok,
    Opencode,
    Cline,
    Gemini,
    Pi,
}
impl Agent {
    pub const ALL: [Self; 8] = [
        Self::ClaudeCode,
        Self::Codex,
        Self::Cursor,
        Self::Grok,
        Self::Opencode,
        Self::Cline,
        Self::Gemini,
        Self::Pi,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::Cursor => "cursor",
            Self::Grok => "grok",
            Self::Opencode => "opencode",
            Self::Cline => "cline",
            Self::Gemini => "gemini",
            Self::Pi => "pi",
        }
    }
    pub fn binary(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude",
            Self::Cursor => "agent",
            _ => self.name(),
        }
    }
    pub fn protocol(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude",
            Self::Codex => "codex",
            Self::Pi => "pi",
            _ => "acp",
        }
    }
    pub fn arguments(self) -> Vec<&'static str> {
        match self {
            Self::ClaudeCode => vec![
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--include-partial-messages",
                "--permission-prompt-tool",
                "stdio",
            ],
            Self::Codex => vec!["app-server", "--listen", "stdio://"],
            Self::Pi => vec!["--mode", "rpc"],
            Self::Cursor | Self::Opencode => vec!["acp"],
            Self::Grok => vec!["--no-auto-update", "agent", "stdio"],
            Self::Cline | Self::Gemini => vec!["--acp"],
        }
    }
}
