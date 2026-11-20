//! pi agent skills (Agent Skills `SKILL.md` format), ported from
//! pi_agent_rust/src/resources.rs discovery.
//!
//! Roots, in precedence order (first skill with a name wins):
//! project `.pi/skills/`, project `.agents/skills/`, user
//! `$PI_CODING_AGENT_DIR/skills` (default `~/.pi/agent/skills/`), user
//! `~/.agents/skills/`. Discovery is recursive within a root, skips hidden
//! entries and `node_modules`; a directory holding `SKILL.md` is one skill.
//! pi-specific roots also accept root-level `*.md` files.
//!
//! shortcut: symlinks are skipped (pi follows them) so a project skill can
//! never read outside its root; follow them if users ask for linked skills.
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const MAX_SKILL_BYTES: u64 = 256 * 1024;
const MAX_ENTRIES_PER_ROOT: usize = 16_384;
const MAX_DEPTH: usize = 8;

struct Skill {
    name: String,
    description: String,
    path: PathBuf,
    model_invocable: bool,
    user_invocable: bool,
}

fn roots(project: &Path) -> Vec<(PathBuf, bool)> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    let agent_dir = std::env::var_os("PI_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".pi").join("agent")));
    let mut roots = vec![
        (project.join(".pi").join("skills"), true),
        (project.join(".agents").join("skills"), false),
    ];
    if let Some(agent_dir) = agent_dir {
        roots.push((agent_dir.join("skills"), true));
    }
    if let Some(home) = home {
        roots.push((home.join(".agents").join("skills"), false));
    }
    roots
}

fn discover(project: &Path) -> Vec<Skill> {
    let mut skills: Vec<Skill> = Vec::new();
    let project_real = project.canonicalize().ok();
    for (index, (root, pi_specific)) in roots(project).into_iter().enumerate() {
        // Project roots (the first two) must resolve inside the project, e.g. no `.pi -> /` link.
        if index < 2
            && root.exists()
            && !root
                .canonicalize()
                .ok()
                .zip(project_real.as_ref())
                .is_some_and(|(r, p)| r.starts_with(p))
        {
            continue;
        }
        let mut found = Vec::new();
        let mut budget = MAX_ENTRIES_PER_ROOT;
        if walk(&root, pi_specific, 0, &mut budget, &mut found).is_err() || budget == 0 {
            // pi rejects an over-budget root whole instead of a partial, order-dependent catalog.
            continue;
        }
        for skill in found {
            if !skills.iter().any(|s| s.name == skill.name) {
                skills.push(skill);
            }
        }
    }
    skills
}

fn walk(
    dir: &Path,
    root_level: bool,
    depth: usize,
    budget: &mut usize,
    out: &mut Vec<Skill>,
) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    let skill_md = dir.join("SKILL.md");
    if depth > 0 && is_file(&skill_md) {
        let fallback = dir.file_name().unwrap_or_default().to_string_lossy();
        out.extend(parse(&skill_md, &fallback));
        return Ok(());
    }
    for entry in entries {
        *budget = budget.checked_sub(1).context("skill root too large")?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || name == "node_modules" {
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() && depth < MAX_DEPTH {
            walk(&path, false, depth + 1, budget, out)?;
        } else if kind.is_file() && root_level && depth == 0 && name.ends_with(".md") {
            out.extend(parse(&path, name.trim_end_matches(".md")));
        }
    }
    Ok(())
}

fn is_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
}

fn read_bounded(path: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_SKILL_BYTES {
        tracing::debug!(path = %path.display(), "skipping unreadable or oversized skill");
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// `(frontmatter fields, body)`; no or unclosed frontmatter yields no fields.
fn split_frontmatter(content: &str) -> (Vec<(String, String)>, &str) {
    let Some(rest) = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))
    else {
        return (Vec::new(), content);
    };
    let Some(end) = rest.find("\n---") else {
        return (Vec::new(), content);
    };
    let body = rest[end + 4..]
        .trim_start_matches(['-', '\r'])
        .trim_start_matches('\n');
    let mut fields: Vec<(String, String)> = Vec::new();
    // Set while a key's value continues on indented lines (`>`, `|-`, or an empty value).
    let mut folding = false;
    for line in rest[..end].lines() {
        if folding && (line.starts_with([' ', '\t']) || line.trim().is_empty()) {
            if let Some((_, value)) = fields.last_mut()
                && !line.trim().is_empty()
            {
                if !value.is_empty() {
                    value.push(' ');
                }
                value.push_str(line.trim());
            }
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            folding = false;
            continue;
        };
        let v = v.trim();
        folding = v.is_empty() || matches!(v, ">" | ">-" | ">+" | "|" | "|-" | "|+");
        let v = if folding { "" } else { v };
        let v = v
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| v.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(v);
        fields.push((k.trim().to_owned(), v.to_owned()));
    }
    (fields, body)
}

/// `(name, description)` of a SKILL.md text; `None` without a description (pi skips those).
pub fn describe(content: &str, fallback_name: &str) -> Option<(String, String)> {
    let (fields, _) = split_frontmatter(content);
    let get = |k: &str| {
        fields
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.as_str())
    };
    let description = get("description").filter(|d| !d.is_empty())?.to_owned();
    let name = get("name")
        .filter(|n| !n.is_empty())
        .unwrap_or(fallback_name)
        .to_owned();
    Some((name, description))
}

fn parse(path: &Path, fallback_name: &str) -> Option<Skill> {
    let content = read_bounded(path)?;
    let (fields, _) = split_frontmatter(&content);
    let get = |k: &str| {
        fields
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.as_str())
    };
    let description = get("description").filter(|d| !d.is_empty())?.to_owned();
    Some(Skill {
        name: get("name")
            .filter(|n| !n.is_empty())
            .unwrap_or(fallback_name)
            .to_owned(),
        description,
        path: path.to_owned(),
        model_invocable: get("disable-model-invocation") != Some("true"),
        user_invocable: get("user-invocable") != Some("false"),
    })
}

/// `skill_list`: the catalog, without bodies.
pub fn list(project: &Path) -> Value {
    let skills: Vec<Value> = discover(project)
        .into_iter()
        .map(|s| {
            json!({"name": s.name, "description": s.description, "location": s.path.display().to_string(),
                "model_invocable": s.model_invocable, "user_invocable": s.user_invocable})
        })
        .collect();
    json!({ "skills": skills })
}

/// `skill_read`: one skill's body (frontmatter stripped) and the directory its references are relative to.
pub fn read(project: &Path, name: &str) -> Result<Value> {
    let Some(skill) = discover(project).into_iter().find(|s| s.name == name) else {
        bail!("Unknown skill '{name}'");
    };
    let content = read_bounded(&skill.path).context("Skill file is no longer readable")?;
    let (_, body) = split_frontmatter(&content);
    Ok(json!({
        "name": skill.name,
        "location": skill.path.display().to_string(),
        "base_dir": skill.path.parent().unwrap_or(Path::new("")).display().to_string(),
        "user_invocable": skill.user_invocable,
        "content": body.trim(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn discovers_project_skills_with_precedence_and_flags() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path();
        write(
            p,
            ".pi/skills/sql/SKILL.md",
            "---\nname: \"sql-expert\"\ndescription: SQL help\n---\nUse CTEs.\n",
        );
        write(
            p,
            ".pi/skills/quick.md",
            "---\ndescription: Quick one\nuser-invocable: false\n---\nQuick.",
        );
        write(
            p,
            ".pi/skills/nodesc/SKILL.md",
            "---\nname: nodesc\n---\nSkipped.",
        );
        write(
            p,
            ".agents/skills/group/review/SKILL.md",
            "---\ndescription: Review\ndisable-model-invocation: true\n---\nReview body.",
        );
        write(
            p,
            ".agents/skills/sql/SKILL.md",
            "---\nname: sql-expert\ndescription: shadowed\n---\nNo.",
        );
        write(
            p,
            ".agents/skills/root.md",
            "---\ndescription: universal roots ignore root files\n---\n",
        );
        write(
            p,
            ".agents/skills/node_modules/x/SKILL.md",
            "---\ndescription: dep\n---\n",
        );
        let listed = list(p);
        let names: Vec<&str> = listed["skills"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            // User-level roots may exist on the test machine; keep project ones.
            .filter(|n| ["sql-expert", "quick", "review", "nodesc", "root", "x"].contains(n))
            .collect();
        assert_eq!(names, ["quick", "sql-expert", "review"]);
        let sql = &listed["skills"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "sql-expert")
            .unwrap();
        assert_eq!(sql["description"], "SQL help");
        let review = listed["skills"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "review")
            .unwrap();
        assert_eq!(review["model_invocable"], false);
        let read_back = read(p, "sql-expert").unwrap();
        assert_eq!(read_back["content"], "Use CTEs.");
        assert!(read_back["base_dir"].as_str().unwrap().ends_with("sql"));
        assert_eq!(read(p, "quick").unwrap()["user_invocable"], false);
        assert!(read(p, "missing").is_err());
    }

    #[test]
    fn folded_yaml_descriptions_parse() {
        for text in [
            "---\nname: a\ndescription: >\n  Folded\n  text.\nlicense: MIT\n---\nBody",
            "---\nname: a\ndescription: |-\n  Folded\n  text.\n---\nBody",
            "---\nname: a\ndescription:\n  Folded\n  text.\n---\nBody",
        ] {
            assert_eq!(
                describe(text, "x"),
                Some(("a".into(), "Folded text.".into()))
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_skills_are_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write(
            outside.path(),
            "leak/SKILL.md",
            "---\ndescription: outside\n---\nSecret.",
        );
        std::fs::create_dir_all(tmp.path().join(".pi/skills")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("leak"),
            tmp.path().join(".pi/skills/leak"),
        )
        .unwrap();
        assert!(read(tmp.path(), "leak").is_err());
    }
}
