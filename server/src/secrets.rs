use regex::Regex;
use std::{borrow::Cow, sync::OnceLock};

struct SecretRule {
    label: &'static str,
    regex: Regex,
    /// When set, only redact the capture group at this index instead of the full match.
    group: Option<usize>,
}

fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("VENTICODE_REDACT_SECRETS")
            .ok()
            .map(|v| {
                !matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "0" | "false" | "off"
                )
            })
            .unwrap_or(true)
    })
}

fn rules() -> &'static [SecretRule] {
    static RULES: OnceLock<Vec<SecretRule>> = OnceLock::new();
    RULES.get_or_init(|| {
        let specs: &[(&str, &str, Option<usize>)] = &[
            (
                "PEM_PRIVATE_KEY",
                r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?-----END [A-Z0-9 ]*PRIVATE KEY-----",
                None,
            ),
            (
                "ANTHROPIC_KEY",
                r"(?:^|[^A-Za-z0-9_])(sk-ant-[A-Za-z0-9_-]{20,})",
                Some(1),
            ),
            (
                "OPENAI_KEY",
                r"(?:^|[^A-Za-z0-9_])(sk-(?:proj-|svcacct-)?[A-Za-z0-9_-]{20,})",
                Some(1),
            ),
            (
                "GITHUB_TOKEN",
                r"(?:^|[^A-Za-z0-9_])((?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{22,})",
                Some(1),
            ),
            (
                "GITLAB_TOKEN",
                r"(?:^|[^A-Za-z0-9_])(glpat-[A-Za-z0-9_-]{20,})",
                Some(1),
            ),
            (
                "SLACK_TOKEN",
                r"(?:^|[^A-Za-z0-9_])(xox[baprs]-[0-9A-Za-z-]{16,})",
                Some(1),
            ),
            (
                "AWS_ACCESS_KEY",
                r"(?:^|[^A-Za-z0-9_])((?:AKIA|ASIA)[0-9A-Z]{16})(?:[^A-Za-z0-9_]|$)",
                Some(1),
            ),
            (
                "BEARER_TOKEN",
                r"(?i)\bAuthorization\s*:\s*Bearer\s+([A-Za-z0-9._~+/=-]{20,})",
                Some(1),
            ),
            (
                "DSN_CREDENTIAL",
                r"\b(?:postgres(?:ql)?|mysql|redis|rediss|mongodb(?:\+srv)?|amqp|amqps)://[^:\s/@]+:([^@\s/]{6,})@",
                Some(1),
            ),
        ];
        specs
            .iter()
            .filter_map(|(label, pattern, group)| {
                Regex::new(pattern).ok().map(|regex| SecretRule {
                    label,
                    regex,
                    group: *group,
                })
            })
            .collect()
    })
}

/// Redact high-confidence credential patterns before persisting tool output or sending to LLMs.
/// Returns `Cow::Borrowed` with zero allocation when no secret pattern is found.
pub(super) fn redact(input: &str) -> Cow<'_, str> {
    if !enabled() || input.is_empty() {
        return Cow::Borrowed(input);
    }
    redact_always(input)
}

fn redact_always(input: &str) -> Cow<'_, str> {
    replace_spans(input, |_, label| format!("[REDACTED:{label}]"))
}

/// Merged, sorted secret spans `(start, end, label)` from every rule.
fn spans(input: &str) -> Vec<(usize, usize, &'static str)> {
    let mut spans: Vec<(usize, usize, &'static str)> = Vec::new();
    for rule in rules() {
        match rule.group {
            None => {
                for m in rule.regex.find_iter(input) {
                    spans.push((m.start(), m.end(), rule.label));
                }
            }
            Some(group_idx) => {
                for caps in rule.regex.captures_iter(input) {
                    if let Some(m) = caps.get(group_idx) {
                        spans.push((m.start(), m.end(), rule.label));
                    }
                }
            }
        }
    }
    spans.sort_by_key(|(start, end, _)| (*start, std::cmp::Reverse(*end)));
    let mut merged: Vec<(usize, usize, &'static str)> = Vec::with_capacity(spans.len());
    for span in spans {
        if let Some(last) = merged.last_mut()
            && span.0 <= last.1
        {
            if span.1 > last.1 {
                last.1 = span.1;
            }
            continue;
        }
        merged.push(span);
    }
    merged
}

fn replace_spans<'a>(
    input: &'a str,
    mut with: impl FnMut(&str, &'static str) -> String,
) -> Cow<'a, str> {
    let merged = spans(input);
    if merged.is_empty() {
        return Cow::Borrowed(input);
    }
    let mut out = String::with_capacity(input.len());
    let mut pos = 0;
    for (start, end, label) in merged {
        out.push_str(&input[pos..start]);
        out.push_str(&with(&input[start..end], label));
        pos = end;
    }
    out.push_str(&input[pos..]);
    Cow::Owned(out)
}

/// Secrets vault (ported from pi_agent_rust/src/secrets.rs, `obfuscate` mode):
/// credential-shaped values become stable placeholders before a provider call
/// and are restored in tool arguments before execution. Placeholders derive
/// from the server's HMAC fingerprint key, so a secret maps to the same
/// placeholder in every run and leaks nothing about its value. Text the model
/// streams to the user keeps the placeholder.
#[derive(Default)]
pub(super) struct Vault(std::collections::BTreeMap<String, String>);

impl Vault {
    /// Replace secrets in every string of `value` (message content and tool-call arguments).
    pub(super) fn obfuscate(&mut self, app: &crate::App, value: &mut serde_json::Value) {
        if enabled() {
            self.obfuscate_with(value, &|secret| app.storage.fingerprint(secret));
        }
    }

    fn obfuscate_with(&mut self, value: &mut serde_json::Value, key: &dyn Fn(&str) -> String) {
        use serde_json::Value;
        match value {
            Value::String(text) => {
                if let Cow::Owned(out) = replace_spans(text, |secret, label| {
                    let placeholder = format!("[SECRET:{label}:{}]", &key(secret)[..12]);
                    self.0.insert(placeholder.clone(), secret.to_owned());
                    placeholder
                }) {
                    *text = out;
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|v| self.obfuscate_with(v, key)),
            Value::Object(map) => {
                for (k, v) in map.iter_mut() {
                    // Tool-call arguments are JSON text: obfuscate the decoded strings so a
                    // restored secret (e.g. a PEM with newlines) is not left JSON-escaped.
                    if k == "arguments"
                        && let Some(mut parsed) = v
                            .as_str()
                            .and_then(|a| serde_json::from_str::<Value>(a).ok())
                    {
                        self.obfuscate_with(&mut parsed, key);
                        *v = Value::String(parsed.to_string());
                    } else {
                        self.obfuscate_with(v, key);
                    }
                }
            }
            _ => {}
        }
    }

    /// Put the original values back into every string of `value`.
    pub(super) fn restore(&self, value: &mut serde_json::Value) {
        use serde_json::Value;
        match value {
            Value::String(text) if text.contains("[SECRET:") => {
                for (placeholder, secret) in &self.0 {
                    if text.contains(placeholder.as_str()) {
                        *text = text.replace(placeholder.as_str(), secret);
                    }
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|v| self.restore(v)),
            Value::Object(map) => map.values_mut().for_each(|v| self.restore(v)),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_common_provider_and_dsn_secrets() {
        let sample = "ANTHROPIC=sk-ant-api03-abcdefghijklmnopqrstuvwx\n\
            OPENAI=sk-proj-1234567890abcdefghijklmno\n\
            GH=ghp_1234567890abcdefghijklmnopqrstuvwxyz\n\
            GL=glpat-abcdefghijklmnopqrstuv\n\
            SLACK=xoxb-123456789012-123456789012-abcdefghijkl\n\
            AWS=AKIAIOSFODNN7EXAMPLE\n\
            DATABASE_URL=postgres://admin:supersecretpass@db.internal:5432/app\n\
            Authorization: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.abc\n\
            -----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA\n-----END RSA PRIVATE KEY-----";
        let out = redact_always(sample);
        assert!(!out.contains("sk-ant-api03"));
        assert!(!out.contains("sk-proj-12345"));
        assert!(!out.contains("ghp_12345"));
        assert!(!out.contains("glpat-abc"));
        assert!(!out.contains("xoxb-12345"));
        assert!(!out.contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(!out.contains("supersecretpass"));
        assert!(!out.contains("eyJhbGciOiJI"));
        assert!(!out.contains("MIIEowIBAAKCAQEA"));
        assert!(out.contains("[REDACTED:ANTHROPIC_KEY]"));
        assert!(out.contains("[REDACTED:PEM_PRIVATE_KEY]"));
        assert!(out.contains("postgres://admin:[REDACTED:DSN_CREDENTIAL]@db.internal:5432/app"));
    }

    #[test]
    fn avoids_false_positives_on_normal_code() {
        let clean = "const key = process.env.OPENAI_API_KEY;\n\
            // task-management-service-endpoint\n\
            https://example.com/docs/api";
        let out = redact_always(clean);
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(out, clean);
    }

    #[test]
    fn vault_obfuscates_stably_and_restores_tool_arguments() {
        let key = |s: &str| format!("{:0>64x}", s.len() * 7919);
        let mut vault = Vault::default();
        let mut messages = serde_json::json!([
            {"role":"user","content":"deploy with sk-ant-api03-abcdefghijklmnopqrstuvwx please"},
            {"role":"assistant","tool_calls":[{"function":{"arguments":"{\"k\":\"sk-ant-api03-abcdefghijklmnopqrstuvwx\",\"pem\":\"-----BEGIN RSA PRIVATE KEY-----\\nMIIE\\n-----END RSA PRIVATE KEY-----\"}"}}]}
        ]);
        vault.obfuscate_with(&mut messages, &key);
        let text = messages.to_string();
        assert!(!text.contains("abcdefghijklmnop"));
        let placeholder = messages[0]["content"]
            .as_str()
            .unwrap()
            .split(' ')
            .nth(2)
            .unwrap()
            .to_owned();
        assert!(placeholder.starts_with("[SECRET:ANTHROPIC_KEY:"));
        assert!(
            messages[1]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .unwrap()
                .contains(&placeholder)
        );
        let mut args = serde_json::json!({"command": format!("echo {placeholder} > .env"), "n": 1});
        vault.restore(&mut args);
        assert_eq!(
            args["command"],
            "echo sk-ant-api03-abcdefghijklmnopqrstuvwx > .env"
        );
        let decoded: serde_json::Value = serde_json::from_str(
            messages[1]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        let mut pem = decoded["pem"].clone();
        vault.restore(&mut pem);
        assert_eq!(
            pem,
            "-----BEGIN RSA PRIVATE KEY-----\nMIIE\n-----END RSA PRIVATE KEY-----"
        );
    }
}
