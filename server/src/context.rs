//! Project context for the system prompt, ported from pi_agent_rust (`app.rs`
//! project-context loader + `context_files.rs` foreign-rule import).
//!
//! Native `AGENTS.md` (or `CLAUDE.md`) at the project folder is injected
//! verbatim. Other agents' rule files are read in their native shape:
//!
//! | Format | Location | Scoping |
//! |--------|----------|---------|
//! | Cursor MDC | `.cursor/rules/*.mdc` | frontmatter `globs` / `alwaysApply` |
//! | Cursor legacy | `.cursorrules` | always |
//! | Cline | `.clinerules` file or `.clinerules/*.md` dir | always |
//! | Copilot | `.github/copilot-instructions.md` | always |
//! | Copilot scoped | `.github/instructions/*.instructions.md` | frontmatter `applyTo` |
//! | Windsurf legacy | `.windsurfrules` | always |
//! | Windsurf rules | `.windsurf/rules/*` | frontmatter `trigger` / `globs` / `description` |
//! | Gemini | `GEMINI.md` | always |
//!
//! Files are read through the project's harness with the read-only
//! `list_files`/`read_file` operations, so sandboxing and old harnesses work
//! unchanged. Loading is best effort: any failure only omits the context.
use super::*;
use std::collections::HashSet;
use std::fmt::Write as _;

/// Total byte budget for injected foreign-rule content (pi's value).
const FOREIGN_RULES_BUDGET_BYTES: usize = 24 * 1024;
/// Native files are user-authored instructions; keep them whole up to this cap.
const NATIVE_BUDGET_BYTES: usize = 64 * 1024;
const MAX_DIR_RULES: usize = 32;

struct Rule {
    content: String,
    globs: Vec<String>,
    always_apply: bool,
    description: Option<String>,
    source: String,
    format: &'static str,
}

/// The conversation's project folder on its harness.
pub(super) struct Project {
    harness: Uuid,
    root: String,
    path: String,
    project: Option<Uuid>,
}

impl Project {
    /// From a conversation row (`harness_id`, `root`, `path`, `project_id`) the member may use.
    pub(super) fn new(member: &Member, c: &sqlx::postgres::PgRow) -> Option<Self> {
        let harness: Uuid = c.get::<Option<Uuid>, _>("harness_id")?;
        let root: String = c.get("root");
        member.harness(harness, &root).ok()?;
        Some(Project {
            harness,
            root,
            path: c.get("path"),
            project: c.get("project_id"),
        })
    }

    /// One read-only harness operation; `None` on any failure.
    async fn call(
        &self,
        app: &App,
        h: &HeaderMap,
        operation: &str,
        arguments: Value,
    ) -> Option<Value> {
        let request_id = Uuid::new_v4();
        let execute = json!({
            "action": "execute", "request_id": request_id, "project_id": self.project.unwrap_or(request_id),
            "harness_id": self.harness, "root": self.root, "project_path": self.path,
            "operation": operation, "arguments": arguments,
        });
        crate::browse::browse(app, h, &execute, request_id)
            .await
            .ok()
    }

    /// System-prompt additions: project context files and the skills catalog.
    pub(super) async fn prompt(&self, app: &App, h: &HeaderMap) -> Option<String> {
        let (files, skills) = futures_util::future::join(
            self.context_files(app, h),
            self.call(app, h, "skill_list", json!({})),
        )
        .await;
        let out = files.unwrap_or_default() + &skills.map(|s| skills_block(&s)).unwrap_or_default();
        (!out.is_empty()).then_some(out)
    }

    /// pi's `/skill:name args`: replace such user messages with the skill body and the arguments.
    pub(super) async fn expand_skill_commands(
        &self,
        app: &App,
        h: &HeaderMap,
        messages: &mut [Value],
    ) {
        for message in messages.iter_mut().filter(|m| m["role"] == "user") {
            let Some((name, args)) = message["content"].as_str().and_then(skill_command) else {
                continue;
            };
            let Some(skill) = self.call(app, h, "skill_read", json!({"name": name})).await else {
                continue;
            };
            if skill["user_invocable"] == false {
                continue;
            }
            message["content"] = json!(expand_skill(&skill, &args));
        }
    }

    /// Context files (`AGENTS.md` and foreign rules) read through the harness.
    async fn context_files(&self, app: &App, h: &HeaderMap) -> Option<String> {
        let call = |operation: &'static str, path: String| {
            self.call(app, h, operation, json!({"path": path}))
        };
        let names = |listing: Option<Value>, dirs: bool| -> Vec<String> {
            let mut names: Vec<String> = listing
                .and_then(|v| v["items"].as_array().cloned())
                .unwrap_or_default()
                .iter()
                .filter(|i| i["directory"] == dirs && i["symlink"] != true)
                .filter_map(|i| i["name"].as_str().map(str::to_owned))
                .collect();
            names.sort();
            names
        };
        let top = call("list_files", ".".into()).await;
        if top.is_none() {
            tracing::debug!("project context skipped: harness listing failed");
            return None;
        }
        let (files, dirs) = (names(top.clone(), false), names(top, true));
        let has_file = |n: &str| files.iter().any(|f| f == n);
        let has_dir = |n: &str| dirs.iter().any(|d| d == n);

        // Directory candidates: (dir, extension filter, format).
        let mut dir_specs: Vec<(&str, Option<&str>, &'static str)> = Vec::new();
        if has_dir(".cursor") {
            dir_specs.push((".cursor/rules", Some("mdc"), "cursor-mdc"));
        }
        if has_dir(".clinerules") {
            dir_specs.push((".clinerules", Some("md"), "cline"));
        }
        if has_dir(".github") {
            dir_specs.push((".github/instructions", Some("md"), "copilot-scoped"));
        }
        if has_dir(".windsurf") {
            dir_specs.push((".windsurf/rules", None, "windsurf-dir"));
        }
        let listings = futures_util::future::join_all(
            dir_specs
                .iter()
                .map(|(d, _, _)| call("list_files", (*d).into())),
        )
        .await;

        // Everything to read, in pi's discovery order.
        let mut wanted: Vec<(String, &'static str)> = Vec::new();
        for native in ["AGENTS.md", "CLAUDE.md"] {
            if has_file(native) {
                wanted.push((native.into(), "native"));
            }
        }
        let dir_files = |dir: &str, ext: Option<&str>, listing: &Option<Value>| -> Vec<String> {
            names(listing.clone(), false)
                .into_iter()
                .filter(|n| {
                    ext.is_none_or(|e| {
                        n.rsplit_once('.')
                            .is_some_and(|(_, x)| x.eq_ignore_ascii_case(e))
                    })
                })
                .take(MAX_DIR_RULES)
                .map(|n| format!("{dir}/{n}"))
                .collect()
        };
        let spec_files = |format: &str| {
            dir_specs
                .iter()
                .zip(&listings)
                .filter(|((_, _, f), _)| *f == format)
                .flat_map(|((d, e, _), l)| dir_files(d, *e, l))
                .collect::<Vec<_>>()
        };
        wanted.extend(
            spec_files("cursor-mdc")
                .into_iter()
                .map(|p| (p, "cursor-mdc")),
        );
        if has_file(".cursorrules") {
            wanted.push((".cursorrules".into(), "cursor-legacy"));
        }
        wanted.extend(spec_files("cline").into_iter().map(|p| (p, "cline")));
        if has_file(".clinerules") {
            wanted.push((".clinerules".into(), "cline"));
        }
        if has_dir(".github") {
            wanted.push((".github/copilot-instructions.md".into(), "copilot"));
        }
        wanted.extend(
            spec_files("copilot-scoped")
                .into_iter()
                .filter(|p| p.ends_with(".instructions.md"))
                .map(|p| (p, "copilot-scoped")),
        );
        if has_file(".windsurfrules") {
            wanted.push((".windsurfrules".into(), "windsurf"));
        }
        wanted.extend(
            spec_files("windsurf-dir")
                .into_iter()
                .map(|p| (p, "windsurf-dir")),
        );
        if has_file("GEMINI.md") {
            wanted.push(("GEMINI.md".into(), "gemini"));
        }
        let contents = futures_util::future::join_all(
            wanted.iter().map(|(p, _)| call("read_file", p.clone())),
        )
        .await;
        let read: Vec<(String, &'static str, String)> = wanted
            .into_iter()
            .zip(contents)
            .filter_map(|((path, format), v)| {
                let v = v?;
                let content = v["content"].as_str().filter(|_| v["encoding"] == "utf8")?;
                Some((path, format, content.to_owned()))
            })
            .collect();
        render(read)
    }
}

/// `/skill:name rest` → `(name, rest)`.
fn skill_command(text: &str) -> Option<(String, String)> {
    let rest = text.strip_prefix("/skill:")?;
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let name = &rest[..end];
    (!name.is_empty()).then(|| (name.to_owned(), rest[end..].trim().to_owned()))
}

fn expand_skill(skill: &Value, args: &str) -> String {
    let block = format!(
        "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
        skill["name"].as_str().unwrap_or_default(),
        skill["location"].as_str().unwrap_or_default(),
        skill["base_dir"].as_str().unwrap_or_default(),
        skill["content"].as_str().unwrap_or_default()
    );
    if args.is_empty() {
        block
    } else {
        format!("{block}\n\n{args}")
    }
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// pi's `<available_skills>` block for skills the model may invoke.
fn skills_block(listing: &Value) -> String {
    let skills: Vec<String> = listing["skills"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| s["model_invocable"] != false)
        .take(128)
        .map(|s| {
            format!(
                "  <skill>\n    <name>{}</name>\n    <description>{}</description>\n    <location>{}</location>\n  </skill>",
                escape_xml(s["name"].as_str().unwrap_or_default()),
                escape_xml(s["description"].as_str().unwrap_or_default()),
                escape_xml(s["location"].as_str().unwrap_or_default())
            )
        })
        .collect();
    if skills.is_empty() {
        return String::new();
    }
    format!(
        "\n\nThe following skills provide specialized instructions for specific tasks.\nUse the skill tool with the skill's name to load it when the task matches its description.\nWhen a skill references a relative path, resolve it against the skill's base_dir.\n\n<available_skills>\n{}\n</available_skills>\n",
        skills.join("\n")
    )
}

/// Build the prompt block from `(relative path, format, content)` in discovery order.
fn render(read: Vec<(String, &'static str, String)>) -> Option<String> {
    let mut out = String::new();
    // pi: the first native file in a directory wins (AGENTS.md before CLAUDE.md).
    let native = read.iter().find(|(_, f, _)| *f == "native");
    if let Some((path, _, content)) = native {
        let content = &content[..content.floor_char_boundary(NATIVE_BUDGET_BYTES)];
        let _ = write!(
            out,
            "\n\n# Project Context\n\nProject-specific instructions and guidelines:\n\n## {path}\n\n{}\n",
            content.trim()
        );
    }
    let native_body = native.map(|(_, _, c)| c.trim().to_owned());
    let rules =
        read.into_iter()
            .filter(|(_, f, _)| *f != "native")
            .map(|(source, format, content)| match format {
                "cursor-mdc" => parse_cursor_mdc(source, &content),
                "copilot-scoped" => parse_copilot_scoped(source, &content),
                "windsurf-dir" => parse_windsurf_rule(source, &content),
                _ => plain_rule(source, content, format),
            });
    if let Some(block) = foreign_block(rules, native_body.as_deref()) {
        out.push_str("\n\n");
        out.push_str(&block);
    }
    (!out.is_empty()).then_some(out)
}

fn foreign_block(rules: impl Iterator<Item = Rule>, native: Option<&str>) -> Option<String> {
    // Dedupe against the native file and identical rules with the same scope.
    let mut seen = HashSet::new();
    let mut spent = 0usize;
    let mut truncated = 0usize;
    let mut kept = Vec::new();
    for rule in rules {
        let body = rule.content.trim().to_owned();
        if body.is_empty() || Some(body.as_str()) == native {
            continue;
        }
        let mut scope = if rule.always_apply {
            Vec::new()
        } else {
            rule.globs.clone()
        };
        scope.sort();
        scope.dedup();
        if !seen.insert((body, rule.always_apply, scope, rule.description.clone())) {
            continue;
        }
        let cost = rule.content.len() + rule.description.as_ref().map_or(0, String::len);
        if truncated > 0 || spent + cost > FOREIGN_RULES_BUDGET_BYTES {
            truncated += 1;
            continue;
        }
        spent += cost;
        kept.push(rule);
    }
    let mut block = String::new();
    for rule in kept.iter().filter(|r| r.always_apply) {
        let _ = write!(
            block,
            "## {} ({})\n\n{}\n\n",
            rule.source,
            rule.format,
            rule.content.trim()
        );
    }
    // shortcut: pi injects scoped rules when a tool touches a matching path; here the
    // model is told to read them itself. Upgrade if models ignore the hint.
    for rule in kept.iter().filter(|r| !r.always_apply) {
        if !rule.globs.is_empty() {
            let _ = writeln!(
                block,
                "Path-scoped rule at `{}` applies to files matching `{}`. Read it before changing matching files.",
                rule.source,
                rule.globs.join("`, `")
            );
        } else if let Some(description) = &rule.description {
            let _ = writeln!(
                block,
                "On-demand rule at `{}`: {description}\nRead this rule file only when that description is relevant to the current task.",
                rule.source
            );
        } else {
            let _ = writeln!(
                block,
                "On-demand rule at `{}`: read only when explicitly requested; not automatically applied.",
                rule.source
            );
        }
    }
    if truncated > 0 {
        let _ = writeln!(
            block,
            "[{truncated} imported rule(s) omitted: {FOREIGN_RULES_BUDGET_BYTES} byte budget reached]"
        );
    }
    (!block.is_empty()).then(|| {
        format!("# Imported Rules\n\nRules imported read-only from other tools' native config files in this project:\n\n{block}")
    })
}

fn plain_rule(source: String, content: String, format: &'static str) -> Rule {
    Rule {
        content,
        globs: Vec::new(),
        always_apply: true,
        description: None,
        source,
        format,
    }
}

/// Split a `---` frontmatter block into simple `key: value` fields and the body.
/// A malformed or unclosed block yields no fields and the whole content as body.
fn split_frontmatter(content: &str) -> (Vec<(String, String)>, String) {
    let mut lines = content.lines();
    if !matches!(lines.next(), Some(first) if first.trim() == "---") {
        return (Vec::new(), content.to_owned());
    }
    let mut fields: Vec<(String, String)> = Vec::new();
    let mut closed = false;
    let mut block_scalar = false;
    for line in lines.by_ref() {
        if line.trim() == "---" {
            closed = true;
            break;
        }
        if block_scalar && (line.starts_with([' ', '\t']) || line.trim().is_empty()) {
            if let Some((_, value)) = fields.last_mut() {
                if !value.is_empty() && !line.trim().is_empty() {
                    value.push(' ');
                }
                value.push_str(line.trim());
            }
            continue;
        }
        block_scalar = false;
        // Sequence entries first: glob paths may contain a colon.
        if let Some(item) = line.trim().strip_prefix("- ") {
            if let Some((_, value)) = fields.last_mut() {
                if !value.is_empty() {
                    value.push(',');
                }
                value.push_str(item.trim());
            }
        } else if let Some((key, value)) = line.split_once(':') {
            let key = key.trim();
            if !key.is_empty() {
                let value = value.trim();
                block_scalar = matches!(value, ">" | ">-" | ">+" | "|" | "|-" | "|+");
                fields.push((
                    key.to_owned(),
                    if block_scalar { "" } else { value }.to_owned(),
                ));
            }
        }
    }
    if !closed {
        return (Vec::new(), content.to_owned());
    }
    (fields, lines.collect::<Vec<_>>().join("\n"))
}

/// Strip quotes only when they enclose the entire scalar.
fn quoted_scalar(value: &str) -> Option<&str> {
    let quote = value.chars().next()?;
    if !matches!(quote, '\'' | '"') {
        return None;
    }
    let mut escaped = false;
    for (index, ch) in value.char_indices().skip(1) {
        if escaped {
            escaped = false;
        } else if ch == '\\' && quote == '"' {
            escaped = true;
        } else if ch == quote {
            return (index + 1 == value.len()).then_some(&value[1..index]);
        }
    }
    None
}

/// Comma-separated scalars, flow lists or flattened block lists. Commas inside
/// braces, classes, quotes and escapes belong to the glob.
fn parse_glob_list(value: &str) -> Vec<String> {
    let value = value.trim();
    let value = quoted_scalar(value).unwrap_or_else(|| {
        value
            .strip_prefix('[')
            .and_then(|v| v.strip_suffix(']'))
            .unwrap_or(value)
    });
    let mut globs = Vec::new();
    let (mut start, mut braces, mut in_class, mut quote, mut escaped) =
        (0, 0usize, false, None, false);
    for (index, ch) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if let Some(delimiter) = quote {
            if ch == delimiter {
                quote = None;
            }
            continue;
        }
        if in_class {
            in_class = ch != ']';
            continue;
        }
        match ch {
            '\'' | '"' if value[start..index].trim().is_empty() => quote = Some(ch),
            '[' => in_class = true,
            '{' => braces += 1,
            '}' => braces = braces.saturating_sub(1),
            ',' if braces == 0 => {
                push_glob(&mut globs, &value[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    push_glob(&mut globs, &value[start..]);
    globs
}

fn push_glob(globs: &mut Vec<String>, value: &str) {
    let value = value.trim();
    let value = quoted_scalar(value).unwrap_or(value).trim();
    if !value.is_empty() {
        globs.push(value.to_owned());
    }
}

fn field<'a>(fields: &'a [(String, String)], key: &str) -> Option<&'a str> {
    fields
        .iter()
        .rev()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn description(fields: &[(String, String)]) -> Option<String> {
    field(fields, "description")
        .map(|v| quoted_scalar(v).unwrap_or(v).trim())
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

/// Cursor `.mdc`: `description` / `globs` / `alwaysApply`. An explicit
/// `alwaysApply: false` without globs is on-demand.
fn parse_cursor_mdc(source: String, content: &str) -> Rule {
    let (fields, body) = split_frontmatter(content);
    let globs = field(&fields, "globs")
        .map(parse_glob_list)
        .unwrap_or_default();
    let always_apply =
        field(&fields, "alwaysApply").map_or(globs.is_empty(), |v| v.trim() == "true");
    Rule {
        description: (!always_apply && globs.is_empty())
            .then(|| description(&fields))
            .flatten(),
        content: body,
        globs,
        always_apply,
        source,
        format: "cursor-mdc",
    }
}

/// Copilot `*.instructions.md`: `applyTo` globs; `**` means always.
fn parse_copilot_scoped(source: String, content: &str) -> Rule {
    let (fields, body) = split_frontmatter(content);
    let globs = field(&fields, "applyTo")
        .map(parse_glob_list)
        .unwrap_or_default();
    let always_apply = globs.is_empty() || globs.iter().any(|g| g == "**");
    Rule {
        content: body,
        globs: if always_apply { Vec::new() } else { globs },
        always_apply,
        description: None,
        source,
        format: "copilot-scoped",
    }
}

/// Windsurf rules: `always_on` | `glob` | `model_decision` | `manual`; unknown stays on-demand.
fn parse_windsurf_rule(source: String, content: &str) -> Rule {
    let (fields, body) = split_frontmatter(content);
    let trigger = field(&fields, "trigger").map(|v| quoted_scalar(v).unwrap_or(v).trim());
    let mut rule = plain_rule(source, body, "windsurf");
    match trigger {
        None | Some("always_on") => {}
        Some("glob") => {
            rule.always_apply = false;
            rule.globs = field(&fields, "globs")
                .map(parse_glob_list)
                .unwrap_or_default();
        }
        Some("model_decision") => {
            rule.always_apply = false;
            rule.description = description(&fields);
        }
        Some(_) => rule.always_apply = false,
    }
    rule
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(path: &str, format: &'static str, content: &str) -> (String, &'static str, String) {
        (path.into(), format, content.into())
    }

    #[test]
    fn renders_native_and_every_foreign_format_with_scoping() {
        let out = render(vec![
            r("AGENTS.md", "native", "Use tabs.\n"),
            r("CLAUDE.md", "native", "Ignored second native file."),
            r(".cursor/rules/style.mdc", "cursor-mdc", "---\ndescription: style\nglobs: \"*.ts,src/**/*.{ts,tsx}\"\nalwaysApply: false\n---\nTS style."),
            r(".cursor/rules/ondemand.mdc", "cursor-mdc", "---\ndescription: db migrations\nalwaysApply: false\n---\nMigrations."),
            r(".cursorrules", "cursor-legacy", "Use tabs."),
            r(".clinerules/01.md", "cline", "Cline rule."),
            r(".github/instructions/b.instructions.md", "copilot-scoped", "---\napplyTo: \"**\"\n---\nEverywhere."),
            r(".windsurf/rules/m.md", "windsurf-dir", "---\ntrigger: manual\n---\nManual."),
            r("GEMINI.md", "gemini", "Gemini rule."),
        ])
        .unwrap();
        assert!(out.contains("# Project Context") && out.contains("## AGENTS.md\n\nUse tabs."));
        assert!(!out.contains("Ignored second native file"));
        // Identical to the native file: dropped.
        assert!(!out.contains("(cursor-legacy)"));
        assert!(out.contains("applies to files matching `*.ts`, `src/**/*.{ts,tsx}`"));
        assert!(out.contains("On-demand rule at `.cursor/rules/ondemand.mdc`: db migrations"));
        assert!(
            out.contains(
                "## .github/instructions/b.instructions.md (copilot-scoped)\n\nEverywhere."
            )
        );
        assert!(
            out.contains("## .clinerules/01.md (cline)") && out.contains("## GEMINI.md (gemini)")
        );
        assert!(out.contains(
            "On-demand rule at `.windsurf/rules/m.md`: read only when explicitly requested"
        ));
        assert!(!out.contains("TS style.") && !out.contains("Manual."));
    }

    #[test]
    fn foreign_budget_drops_overflow_deterministically() {
        let big = "x".repeat(FOREIGN_RULES_BUDGET_BYTES - 10);
        let out = render(vec![
            r(".cursorrules", "cursor-legacy", &big),
            r("GEMINI.md", "gemini", "second rule body that overflows"),
        ])
        .unwrap();
        assert!(!out.contains("# Project Context"));
        assert!(out.contains("[1 imported rule(s) omitted"));
        assert!(render(vec![]).is_none());
    }

    #[test]
    fn skills_catalog_and_commands_follow_pi() {
        let listing = json!({"skills":[
            {"name":"sql","description":"SQL <help>","location":"/p/.pi/skills/sql/SKILL.md","model_invocable":true},
            {"name":"hidden","description":"x","location":"/h","model_invocable":false}
        ]});
        let block = skills_block(&listing);
        assert!(
            block.contains("<name>sql</name>\n    <description>SQL &lt;help&gt;</description>")
        );
        assert!(!block.contains("hidden"));
        assert!(skills_block(&json!({"skills":[]})).is_empty());
        assert_eq!(
            skill_command("/skill:sql  optimize this\nquery"),
            Some(("sql".into(), "optimize this\nquery".into()))
        );
        assert_eq!(skill_command("/skill:"), None);
        assert_eq!(skill_command("hello /skill:sql"), None);
        let skill =
            json!({"name":"sql","location":"/s/SKILL.md","base_dir":"/s","content":"Use CTEs."});
        assert_eq!(
            expand_skill(&skill, "go"),
            "<skill name=\"sql\" location=\"/s/SKILL.md\">\nReferences are relative to /s.\n\nUse CTEs.\n</skill>\n\ngo"
        );
    }

    #[test]
    fn frontmatter_and_globs_parse_like_pi() {
        assert_eq!(parse_glob_list("[\"a,b\", 'c']"), vec!["a,b", "c"]);
        assert_eq!(
            parse_glob_list("src/{a,b}/*.rs, [ab].md"),
            vec!["src/{a,b}/*.rs", "[ab].md"]
        );
        let (fields, body) = split_frontmatter("---\nglobs:\n  - a/**\n  - b:c\n---\nBody");
        assert_eq!(field(&fields, "globs"), Some("a/**,b:c"));
        assert_eq!(body, "Body");
        let (fields, body) = split_frontmatter("---\nunclosed: yes\nBody");
        assert!(fields.is_empty() && body.starts_with("---"));
    }
}
