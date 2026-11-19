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
    if spans.is_empty() {
        return Cow::Borrowed(input);
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
    let mut out = String::with_capacity(input.len());
    let mut pos = 0;
    for (start, end, label) in merged {
        out.push_str(&input[pos..start]);
        out.push_str("[REDACTED:");
        out.push_str(label);
        out.push(']');
        pos = end;
    }
    out.push_str(&input[pos..]);
    Cow::Owned(out)
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
}
