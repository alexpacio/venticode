//! Showcase: curated GitHub sources of pi skills and plugins, browsed and
//! installed through the project's harness (`catalog_*` operations).
//! A member may also name any public `owner/repo`.
use super::*;
use std::time::Duration;

/// Downloading and scanning a repository takes longer than a directory listing.
const CATALOG_DEADLINE: Duration = Duration::from_secs(150);

/// `(id, repo, ref, title, italiano, english)`. Checked live by the harness test
/// `catalog::tests::showcase_sources_list_and_install`.
const SOURCES: &[(&str, &str, &str, &str, &str, &str)] = &[
    (
        "mattpocock",
        "mattpocock/skills",
        "main",
        "Matt Pocock",
        "Skill di ingegneria quotidiane: TDD, code review, modellazione del dominio, debug",
        "Everyday engineering skills: TDD, code review, domain modeling, debugging",
    ),
    (
        "superpowers",
        "obra/superpowers",
        "main",
        "Superpowers (obra)",
        "Metodo di sviluppo: brainstorming, piani, TDD, debug sistematico",
        "Development methodology: brainstorming, plans, TDD, systematic debugging",
    ),
    (
        "anthropic",
        "anthropics/skills",
        "main",
        "Anthropic",
        "Skill ufficiali: documenti, design, costruzione di server MCP",
        "Official skills: documents, design, building MCP servers",
    ),
    (
        "addyosmani",
        "addyosmani/agent-skills",
        "main",
        "Addy Osmani",
        "Ingegneria di produzione: accessibilità, API, CI/CD, prestazioni",
        "Production engineering: accessibility, APIs, CI/CD, performance",
    ),
    (
        "vercel",
        "vercel-labs/agent-skills",
        "main",
        "Vercel",
        "Buone pratiche React e Next.js, deploy su Vercel",
        "React and Next.js best practices, Vercel deploys",
    ),
    (
        "wshobson",
        "wshobson/agents",
        "main",
        "wshobson/agents",
        "Oltre 180 skill per molti domini",
        "180+ skills across many domains",
    ),
    (
        "pi",
        "earendil-works/pi",
        "main",
        "pi (esempi ufficiali)",
        "Plugin ed esempi ufficiali di pi",
        "Official pi plugin examples and skills",
    ),
    (
        "mitsuhiko",
        "mitsuhiko/agent-stuff",
        "main",
        "Armin Ronacher",
        "Plugin e skill pi: todo, review, sottoagenti",
        "pi plugins and skills: todos, review, subagents",
    ),
    (
        "tmustier",
        "tmustier/pi-extensions",
        "main",
        "tmustier/pi-extensions",
        "Plugin pi della comunità",
        "Community pi plugins",
    ),
    (
        "kcosr",
        "kcosr/pi-extensions",
        "main",
        "kcosr/pi-extensions",
        "Plugin pi: apply_patch, codemap, assistente",
        "pi plugins: apply_patch, codemap, assistant",
    ),
    (
        "dot314",
        "w-winter/dot314",
        "main",
        "w-winter/dot314",
        "Plugin, skill e prompt pi",
        "pi plugins, skills and prompts",
    ),
];

fn valid_segment(s: &str, extra: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s != "."
        && s != ".."
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || extra.contains(c))
}

/// A curated source id, or `owner/repo` (optionally `@ref`) for any public repository.
fn resolve(source: &str, git_ref: Option<&str>) -> Result<(String, String)> {
    let (repo, default_ref) = match SOURCES.iter().find(|s| s.0 == source) {
        Some(s) => (s.1.to_owned(), s.2),
        None => {
            let (owner, name) = source.split_once('/').ok_or_else(bad)?;
            if !valid_segment(owner, "") || !valid_segment(name, "._") {
                return Err(bad());
            }
            (source.to_owned(), "main")
        }
    };
    let git_ref = git_ref.unwrap_or(default_ref);
    if git_ref.len() > 100
        || git_ref.contains("..")
        || git_ref.starts_with('/')
        || !git_ref
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-/".contains(c))
    {
        return Err(bad());
    }
    Ok((repo, git_ref.to_owned()))
}

fn clip(v: &Value, max: usize) -> String {
    v.as_str().unwrap_or_default().chars().take(max).collect()
}

/// `GET /showcase` — curated sources.
pub(super) async fn sources(State(app): State<App>, h: HeaderMap) -> Result<Json<Value>> {
    Member::load(&app, &h, false).await?;
    let items: Vec<Value> = SOURCES
        .iter()
        .map(|(id, repo, git_ref, title, it, en)| {
            json!({"id": id, "repo": repo, "ref": git_ref, "title": title, "description": {"it": it, "en": en}})
        })
        .collect();
    Ok(Json(json!({ "items": items })))
}

#[derive(Deserialize)]
pub(super) struct ItemsQuery {
    source: String,
    #[serde(rename = "ref")]
    git_ref: Option<String>,
}

/// `GET /projects/{id}/showcase?source=` — skills and plugins in a source.
pub(super) async fn items(
    State(app): State<App>,
    h: HeaderMap,
    Path(id): Path<Uuid>,
    Query(q): Query<ItemsQuery>,
) -> Result<Json<Value>> {
    let m = Member::load(&app, &h, false).await?;
    let (repo, git_ref) = resolve(&q.source, q.git_ref.as_deref())?;
    let args = json!({"repo": repo, "ref": git_ref});
    let value = crate::projects::harness_call(
        &app,
        &h,
        &m,
        id,
        "catalog_list",
        args,
        Some(CATALOG_DEADLINE),
    )
    .await?;
    // The harness is outside the trust boundary: keep bounded, known fields.
    let items: Vec<Value> = value["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| matches!(i["kind"].as_str(), Some("skill" | "plugin")))
        .take(1000)
        .map(|i| {
            json!({"kind": i["kind"], "name": clip(&i["name"], 128), "description": clip(&i["description"], 300),
                "path": clip(&i["path"], 512), "tools": i["tools"] == true, "outside_imports": i["outside_imports"] == true})
        })
        .collect();
    Ok(Json(json!({"repo": repo, "ref": git_ref, "items": items})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Install {
    source: String,
    #[serde(rename = "ref")]
    git_ref: Option<String>,
    kind: String,
    path: String,
    scope: String,
}

/// `POST /projects/{id}/showcase/install` — copy one listed item into the project or user folder.
pub(super) async fn install(
    State(app): State<App>,
    h: HeaderMap,
    Path(id): Path<Uuid>,
    Json(b): Json<Install>,
) -> Result<Json<Value>> {
    let m = Member::load(&app, &h, true).await?;
    if !matches!(
        (b.kind.as_str(), b.scope.as_str()),
        ("skill", "project" | "user") | ("plugin", "project")
    ) || b.path.len() > 512
    {
        return Err(bad());
    }
    let (repo, git_ref) = resolve(&b.source, b.git_ref.as_deref())?;
    let args =
        json!({"repo": repo, "ref": git_ref, "kind": b.kind, "path": b.path, "scope": b.scope});
    let v = crate::projects::harness_call(
        &app,
        &h,
        &m,
        id,
        "catalog_install",
        args,
        Some(CATALOG_DEADLINE),
    )
    .await?;
    Ok(Json(
        json!({"kind": b.kind, "scope": b.scope, "name": clip(&v["name"], 128),
        "installed": clip(&v["installed"], 1024), "files": v["files"].as_u64().unwrap_or(0), "replaced": v["replaced"] == true}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Remove {
    kind: String,
    name: String,
    scope: String,
}

/// `POST /projects/{id}/showcase/remove` — delete an installed skill or plugin.
pub(super) async fn remove(
    State(app): State<App>,
    h: HeaderMap,
    Path(id): Path<Uuid>,
    Json(b): Json<Remove>,
) -> Result<Json<Value>> {
    let m = Member::load(&app, &h, true).await?;
    if !matches!(
        (b.kind.as_str(), b.scope.as_str()),
        ("skill", "project" | "user") | ("plugin", "project")
    ) || b.name.is_empty()
        || b.name.len() > 128
    {
        return Err(bad());
    }
    let args = json!({"kind": b.kind, "name": b.name, "scope": b.scope});
    let v = crate::projects::harness_call(&app, &h, &m, id, "catalog_remove", args, None).await?;
    Ok(Json(json!({"removed": v["removed"] == true})))
}

/// `GET /projects/{id}/plugins` — plugins installed in the project.
pub(super) async fn installed(
    State(app): State<App>,
    h: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>> {
    let m = Member::load(&app, &h, false).await?;
    let v = crate::projects::harness_call(&app, &h, &m, id, "catalog_installed", json!({}), None)
        .await?;
    let items: Vec<Value> = v["plugins"]
        .as_array()
        .into_iter()
        .flatten()
        .take(256)
        .map(|p| json!({"name": clip(&p["name"], 128), "path": clip(&p["path"], 512)}))
        .collect();
    Ok(Json(json!({ "items": items })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_resolve_and_custom_repos_are_validated() {
        assert_eq!(
            resolve("mattpocock", None).unwrap(),
            ("mattpocock/skills".into(), "main".into())
        );
        assert_eq!(
            resolve("acme/tools", Some("v1.0")).unwrap(),
            ("acme/tools".into(), "v1.0".into())
        );
        for bad in ["nope", "a/b/c", "../x", "a/..", "a b/c", "a/b?x"] {
            assert!(resolve(bad, None).is_err(), "{bad}");
        }
        assert!(resolve("mattpocock", Some("../main")).is_err());
        let ids: std::collections::HashSet<_> = SOURCES.iter().map(|s| s.0).collect();
        assert_eq!(ids.len(), SOURCES.len());
    }
}
