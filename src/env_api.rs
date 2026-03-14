use crate::OutputFormat;
use crate::auth::Session;
use crate::auth::normalize_base_url;
use anyhow::Context;
use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::Read;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Environment {
    pub id: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub is_pinned: Option<bool>,
    #[serde(default)]
    pub task_count: Option<i64>,
}

fn env_base_path(base_url: &str) -> String {
    let normalized = normalize_base_url(base_url);
    if normalized.contains("/backend-api") {
        format!("{normalized}/wham/environments")
    } else {
        format!("{normalized}/api/codex/environments")
    }
}

fn env_by_repo_path(base_url: &str, owner: &str, repo: &str) -> String {
    let normalized = normalize_base_url(base_url);
    if normalized.contains("/backend-api") {
        format!("{normalized}/wham/environments/by-repo/github/{owner}/{repo}")
    } else {
        format!("{normalized}/api/codex/environments/by-repo/github/{owner}/{repo}")
    }
}

async fn get_json<T: for<'de> Deserialize<'de>>(
    url: &str,
    headers: HeaderMap,
) -> anyhow::Result<T> {
    let client = reqwest::Client::new();
    let res = client.get(url).headers(headers).send().await?;
    let status = res.status();
    let body = res.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("GET {url} failed: {status}; body={body}");
    }
    Ok(serde_json::from_str(&body)?)
}

async fn send_json<T: for<'de> Deserialize<'de>>(
    method: reqwest::Method,
    url: &str,
    headers: HeaderMap,
    body: Value,
) -> anyhow::Result<T> {
    let client = reqwest::Client::new();
    let res = client
        .request(method.clone(), url)
        .headers(headers)
        .json(&body)
        .send()
        .await?;
    let status = res.status();
    let text = res.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("{method} {url} failed: {status}; body={text}");
    }
    Ok(serde_json::from_str(&text)?)
}

pub async fn list_environments(session: &Session) -> anyhow::Result<Vec<Environment>> {
    let url = env_base_path(&session.base_url);
    get_json(&url, session.headers()).await
}

pub async fn list_environments_by_repo(
    session: &Session,
    owner: &str,
    repo: &str,
) -> anyhow::Result<Vec<Environment>> {
    let url = env_by_repo_path(&session.base_url, owner, repo);
    get_json(&url, session.headers()).await
}

pub async fn create_environment(session: &Session, body: Value) -> anyhow::Result<Value> {
    let url = env_base_path(&session.base_url);
    let headers = session.headers();
    send_json(reqwest::Method::POST, &url, headers, body).await
}

pub async fn delete_environment(session: &Session, env_id: &str) -> anyhow::Result<Value> {
    let normalized = normalize_base_url(&session.base_url);
    let url = if normalized.contains("/backend-api") {
        format!("{normalized}/wham/environments/{env_id}")
    } else {
        format!("{normalized}/api/codex/environments/{env_id}")
    };
    let client = reqwest::Client::new();
    let res = client
        .delete(&url)
        .headers(session.headers())
        .send()
        .await?;
    let status = res.status();
    let text = res.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("DELETE {url} failed: {status}; body={text}");
    }
    if text.trim().is_empty() {
        Ok(serde_json::json!({"deleted": true, "id": env_id}))
    } else {
        Ok(serde_json::from_str(&text)?)
    }
}

const DEFAULT_MACHINE_ID: &str = "wham-public/wham-universal";

fn machine_id_from_codex_state(codex_home: &std::path::Path) -> Option<String> {
    let path = codex_home.join(".codex-global-state.json");
    let raw = std::fs::read_to_string(path).ok()?;
    let parsed: Value = serde_json::from_str(&raw).ok()?;
    parsed
        .get("electron-persisted-atom-state")
        .and_then(|v| v.get("environment"))
        .and_then(|v| v.get("machine_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

fn resolve_machine_id(session: &Session) -> String {
    std::env::var("CODEX_CLOUD_MACHINE_ID")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .or_else(|| machine_id_from_codex_state(&session.codex_home))
        .unwrap_or_else(|| DEFAULT_MACHINE_ID.to_string())
}

async fn resolve_github_repo_selector(owner: &str, repo: &str) -> anyhow::Result<String> {
    let url = format!("https://api.github.com/repos/{owner}/{repo}");
    let mut req = reqwest::Client::new()
        .get(&url)
        .header(reqwest::header::USER_AGENT, "cloudex");
    if let Some(token) = std::env::var("GITHUB_TOKEN")
        .ok()
        .or_else(|| std::env::var("GH_TOKEN").ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        req = req.bearer_auth(token);
    }

    let res = req.send().await?;
    let status = res.status();
    let body = res.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("GET {url} failed: {status}; body={body}");
    }

    let parsed: Value = serde_json::from_str(&body)?;
    let repo_id = parsed
        .get("id")
        .and_then(Value::as_u64)
        .or_else(|| {
            parsed
                .get("id")
                .and_then(Value::as_i64)
                .and_then(|n| (n >= 0).then_some(n as u64))
        })
        .ok_or_else(|| anyhow::anyhow!("GitHub API response did not include a numeric repo id"))?;
    Ok(format!("github-{repo_id}"))
}

fn parse_owner_repo(input: &str) -> Option<(String, String)> {
    let mut s = input.trim();
    if s.is_empty() {
        return None;
    }
    s = s.trim_matches('/');
    s = s.trim_end_matches(".git");

    for prefix in [
        "git@github.com:",
        "https://github.com/",
        "http://github.com/",
    ] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest;
            break;
        }
    }

    let trimmed = s.trim_matches('/').trim_end_matches(".git");
    if trimmed.is_empty() {
        return None;
    }

    let mut parts = trimmed.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    if owner.is_empty() || repo.is_empty() || parts.next().is_some() {
        return None;
    }
    Some((owner.to_string(), repo.to_string()))
}

fn git_remotes() -> Vec<String> {
    let output = std::process::Command::new("git")
        .args(["remote", "-v"])
        .output();

    let Ok(output) = output else {
        return vec![];
    };
    if !output.status.success() {
        return vec![];
    }
    let stdout = String::from_utf8_lossy(&output.stdout);

    let mut urls: HashSet<String> = HashSet::new();
    for line in stdout.lines() {
        // origin\tgit@github.com:openai/codex.git (fetch)
        let mut parts = line.split_whitespace();
        let _name = parts.next();
        let url = parts.next();
        let kind = parts.next();
        if url.is_none() {
            continue;
        }
        if let Some(kind) = kind
            && kind.contains("(fetch)")
        {
            urls.insert(url.unwrap_or_default().to_string());
        }
    }
    urls.into_iter().collect()
}

fn parse_github_owner_repo(remote_url: &str) -> Option<(String, String)> {
    let s = remote_url.trim();
    if s.is_empty() {
        return None;
    }

    // SSH
    if let Some(rest) = s.strip_prefix("git@github.com:") {
        let rest = rest.trim_start_matches('/').trim_end_matches(".git");
        let mut parts = rest.splitn(2, '/');
        let owner = parts.next()?.to_string();
        let repo = parts.next()?.to_string();
        return Some((owner, repo));
    }

    // HTTPS
    for prefix in ["https://github.com/", "http://github.com/"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            let rest = rest.trim_start_matches('/').trim_end_matches(".git");
            let mut parts = rest.splitn(2, '/');
            let owner = parts.next()?.to_string();
            let repo = parts.next()?.to_string();
            return Some((owner, repo));
        }
    }

    None
}

/// Best-effort environment selection:
/// - If `selector` is Some(...), treat it as id or label (case-insensitive).
/// - If None, prefer a pinned environment; else prefer a by-repo environment; else first global.
pub async fn resolve_environment_id(
    session: &Session,
    selector: Option<&str>,
    desired_label: Option<&str>,
) -> anyhow::Result<String> {
    let mut envs = list_environments(session).await?;

    // Add by-repo envs when we can.
    let remotes = git_remotes();
    let mut by_repo: Vec<Environment> = Vec::new();
    for remote in remotes {
        if let Some((owner, repo)) = parse_github_owner_repo(&remote) {
            if let Ok(list) = list_environments_by_repo(session, &owner, &repo).await {
                by_repo.extend(list);
            }
        }
    }

    // Merge by id (by-repo wins for label/pinned hints).
    let mut map: HashMap<String, Environment> = HashMap::new();
    for e in envs.drain(..) {
        map.insert(e.id.clone(), e);
    }
    for e in by_repo {
        map.entry(e.id.clone())
            .and_modify(|existing| {
                if existing.label.is_none() {
                    existing.label = e.label.clone();
                }
                existing.is_pinned =
                    Some(existing.is_pinned.unwrap_or(false) || e.is_pinned.unwrap_or(false));
                if existing.task_count.is_none() {
                    existing.task_count = e.task_count;
                }
            })
            .or_insert(e);
    }

    let all: Vec<Environment> = map.into_values().collect();

    if let Some(sel) = selector {
        let sel = sel.trim();
        if sel.is_empty() {
            anyhow::bail!("--env must not be empty");
        }
        if let Some(e) = all.iter().find(|e| e.id == sel) {
            return Ok(e.id.clone());
        }
        let matches: Vec<&Environment> = all
            .iter()
            .filter(|e| {
                e.label
                    .as_deref()
                    .is_some_and(|l| l.eq_ignore_ascii_case(sel))
            })
            .collect();
        return match matches.as_slice() {
            [] => anyhow::bail!(
                "environment '{sel}' not found; run `cloudex env list` to see available environments"
            ),
            [one] => Ok(one.id.clone()),
            _ => anyhow::bail!(
                "environment label '{sel}' is ambiguous; use the environment id instead"
            ),
        };
    }

    if let Some(label) = desired_label {
        let matches: Vec<&Environment> = all
            .iter()
            .filter(|e| {
                e.label
                    .as_deref()
                    .is_some_and(|l| l.eq_ignore_ascii_case(label))
            })
            .collect();
        if let [one] = matches.as_slice() {
            return Ok(one.id.clone());
        }
    }

    if let Some(pinned) = all.iter().find(|e| e.is_pinned.unwrap_or(false)) {
        return Ok(pinned.id.clone());
    }

    if let Some(first) = all.first() {
        return Ok(first.id.clone());
    }

    anyhow::bail!("no environments available")
}

pub async fn cmd_env(
    session: &Session,
    cmd: super::EnvCmd,
    format: OutputFormat,
) -> anyhow::Result<()> {
    match cmd {
        super::EnvCmd::List(args) => {
            let mut envs = list_environments(session).await?;
            if let Some(repo) = args.repo {
                if let Some((owner, repo)) = parse_owner_repo(&repo) {
                    if let Ok(more) = list_environments_by_repo(session, &owner, &repo).await {
                        envs.extend(more);
                    }
                }
            }
            if let Some(filter) = args.filter {
                let f = filter.to_lowercase();
                envs.retain(|e| {
                    e.id.to_lowercase().contains(&f)
                        || e.label.as_deref().unwrap_or("").to_lowercase().contains(&f)
                });
            }
            envs.sort_by(|a, b| {
                b.is_pinned
                    .unwrap_or(false)
                    .cmp(&a.is_pinned.unwrap_or(false))
                    .then_with(|| {
                        a.label
                            .clone()
                            .unwrap_or_default()
                            .cmp(&b.label.clone().unwrap_or_default())
                    })
                    .then_with(|| a.id.cmp(&b.id))
            });

            match format {
                OutputFormat::Json => {
                    println!("{}", serde_json::to_string_pretty(&envs)?);
                }
                OutputFormat::Table => {
                    for e in envs {
                        let pinned = if e.is_pinned.unwrap_or(false) {
                            "*"
                        } else {
                            " "
                        };
                        let label = e.label.unwrap_or_else(|| "".to_string());
                        let tc = e
                            .task_count
                            .map(|n| n.to_string())
                            .unwrap_or_else(|| "".to_string());
                        println!("{pinned} {}\t{}\t{}", e.id, label, tc);
                    }
                }
            }
        }
        super::EnvCmd::Detect(args) => {
            let id = resolve_environment_id(session, None, args.label.as_deref()).await?;
            match format {
                OutputFormat::Json => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({"environment_id": id}))?
                    );
                }
                OutputFormat::Table => {
                    println!("{id}");
                }
            }
        }
        super::EnvCmd::Create(args) => {
            let super::EnvCreateArgs {
                label,
                repo,
                raw_json,
            } = args;
            let body = if let Some(path_or_dash) = raw_json {
                let raw = if path_or_dash == "-" {
                    let mut buf = String::new();
                    std::io::stdin().read_to_string(&mut buf)?;
                    buf
                } else {
                    std::fs::read_to_string(path_or_dash)?
                };
                serde_json::from_str::<Value>(&raw)?
            } else {
                // Best-effort body based on (label, repo). This is NOT guaranteed to match the backend.
                let label = label.or_else(|| repo.clone()).ok_or_else(|| {
                    anyhow::anyhow!(
                        "--label is required when --raw-json is not set (or pass --repo to use it as the label)"
                    )
                })?;
                let parsed_repo = match repo.as_deref() {
                    Some(raw_repo) => Some(
                        parse_owner_repo(raw_repo)
                            .ok_or_else(|| anyhow::anyhow!("--repo must be in owner/repo form"))?,
                    ),
                    None => None,
                };

                let mut obj = serde_json::Map::new();
                obj.insert("label".to_string(), Value::String(label));

                // ChatGPT /wham/environments currently requires machine_id + repos.
                if normalize_base_url(&session.base_url).contains("/backend-api") {
                    obj.insert(
                        "machine_id".to_string(),
                        Value::String(resolve_machine_id(session)),
                    );
                    let mut repos: Vec<Value> = Vec::new();
                    if let Some((owner, repo)) = parsed_repo.as_ref() {
                        let selector = resolve_github_repo_selector(owner, repo)
                            .await
                            .with_context(|| {
                                format!(
                                    "failed to resolve GitHub repo id for {owner}/{repo}; \
set GITHUB_TOKEN or GH_TOKEN for private repos, or pass --raw-json with repos=[\"github-<id>\"]"
                                )
                            })?;
                        repos.push(Value::String(selector));
                    }
                    obj.insert("repos".to_string(), Value::Array(repos));
                }

                // Keep the older singular repo shape for best-effort compatibility.
                if let Some((owner, repo)) = parsed_repo.as_ref() {
                    obj.insert(
                        "repo".to_string(),
                        serde_json::json!({"provider":"github", "owner": owner, "repo": repo}),
                    );
                }
                Value::Object(obj)
            };

            let resp = create_environment(session, body).await?;
            match format {
                OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&resp)?),
                OutputFormat::Table => println!("{resp}"),
            }
        }
        super::EnvCmd::Delete(args) => {
            let resp = delete_environment(session, &args.env_id).await?;
            match format {
                OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&resp)?),
                OutputFormat::Table => println!("{resp}"),
            }
        }
    }
    Ok(())
}
