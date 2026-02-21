use crate::auth::{Session, task_url};
use crate::env_api;
use crate::pr::{CreatePrPlan, create_pr_from_dir, create_pr_from_worktree};
use crate::worktree;
use crate::{EventsFormat, OutputFormat};

use anyhow::Context;
use chrono::{DateTime, Utc};
use codex_cloud_tasks_client::{
    ApplyOutcome, ApplyStatus, AttemptStatus, CloudBackend, TaskId, TaskStatus, TaskSummary,
};
use serde_json::Value;
use std::io::IsTerminal;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339()
}

fn emit_jsonl(events: EventsFormat, event: Value) {
    if events == EventsFormat::Jsonl {
        // Single-line JSON for streaming consumers.
        if let Ok(s) = serde_json::to_string(&event) {
            println!("{s}");
        }
    }
}

pub async fn cmd_auth(
    session: &Session,
    show_token: bool,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let mut obj = serde_json::Map::new();
    obj.insert(
        "base_url".to_string(),
        Value::String(session.base_url.clone()),
    );
    obj.insert(
        "codex_home".to_string(),
        Value::String(session.codex_home.to_string_lossy().to_string()),
    );
    if let Some(acc) = &session.account_id {
        obj.insert("account_id".to_string(), Value::String(acc.clone()));
    } else {
        obj.insert("account_id".to_string(), Value::Null);
    }
    obj.insert(
        "user_agent".to_string(),
        Value::String(session.user_agent.clone()),
    );
    if show_token {
        obj.insert(
            "bearer_token".to_string(),
            Value::String(session.bearer_token.clone()),
        );
    }
    let payload = Value::Object(obj);
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&payload)?),
        OutputFormat::Table => {
            println!(
                "base_url: {}",
                payload["base_url"].as_str().unwrap_or_default()
            );
            println!(
                "codex_home: {}",
                payload["codex_home"].as_str().unwrap_or_default()
            );
            println!(
                "account_id: {}",
                payload["account_id"].as_str().unwrap_or("")
            );
            if show_token {
                println!(
                    "bearer_token: {}",
                    payload["bearer_token"].as_str().unwrap_or("")
                );
            }
        }
    }
    Ok(())
}

pub async fn cmd_usage(session: &Session, format: OutputFormat) -> anyhow::Result<()> {
    let client = session.backend_client()?;
    let snapshots = client.get_rate_limits_many().await?;
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&snapshots)?),
        OutputFormat::Table => {
            for s in snapshots {
                let id = s
                    .limit_id
                    .clone()
                    .unwrap_or_else(|| "<unknown>".to_string());
                let name = s.limit_name.clone().unwrap_or_default();
                println!("{id}\t{name}");
                if let Some(primary) = s.primary {
                    println!(
                        "  primary: used_percent={:.1} window_minutes={:?} resets_at={:?}",
                        primary.used_percent, primary.window_minutes, primary.resets_at
                    );
                }
                if let Some(secondary) = s.secondary {
                    println!(
                        "  secondary: used_percent={:.1} window_minutes={:?} resets_at={:?}",
                        secondary.used_percent, secondary.window_minutes, secondary.resets_at
                    );
                }
                if let Some(credits) = s.credits {
                    println!(
                        "  credits: has_credits={} unlimited={} balance={}",
                        credits.has_credits,
                        credits.unlimited,
                        credits.balance.as_deref().unwrap_or("<none>")
                    );
                }
            }
        }
    }
    Ok(())
}

pub async fn cmd_requirements(session: &Session, format: OutputFormat) -> anyhow::Result<()> {
    let client = session.backend_client()?;
    let resp = client.get_config_requirements_file().await?;
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&resp)?),
        OutputFormat::Table => {
            if let Some(contents) = resp.contents {
                print!("{contents}");
            } else {
                println!("(no requirements file returned)");
            }
        }
    }
    Ok(())
}

pub async fn cmd_request(
    session: &Session,
    method: String,
    path: String,
    body: Option<String>,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let method = method
        .parse::<reqwest::Method>()
        .context("invalid --method")?;
    let mut path = path;
    if !path.starts_with('/') {
        path = format!("/{path}");
    }
    let url = format!(
        "{}{}",
        crate::auth::normalize_base_url(&session.base_url),
        path
    );

    let client = reqwest::Client::new();
    let mut req = client
        .request(method.clone(), &url)
        .headers(session.headers());
    if let Some(b) = body {
        let raw = if b == "-" {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            buf
        } else {
            b
        };
        let json: Value = serde_json::from_str(&raw)?;
        req = req.json(&json);
    }

    let res = req.send().await?;
    let status = res.status();
    let text = res.text().await.unwrap_or_default();

    match format {
        OutputFormat::Json => {
            let out = serde_json::json!({"status": status.as_u16(), "body": text});
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
        OutputFormat::Table => {
            println!("{method} {url} -> {status}");
            print!("{text}");
        }
    }
    Ok(())
}

pub async fn cmd_task(
    session: &Session,
    cmd: super::TaskCmd,
    format: OutputFormat,
) -> anyhow::Result<()> {
    match cmd {
        super::TaskCmd::Create(args) => task_create(session, args, format).await,
        super::TaskCmd::Run(args) => task_run(session, args, format).await,
        super::TaskCmd::List(args) => task_list(session, args, format).await,
        super::TaskCmd::Show(args) => task_show(session, args, format).await,
        super::TaskCmd::Diff(args) => task_diff(session, args, format).await,
        super::TaskCmd::Watch(args) => task_watch(session, args, format).await,
        super::TaskCmd::Apply(args) => task_apply(session, args, format).await,
        super::TaskCmd::Prs(args) => task_prs(session, args, format).await,
    }
}

fn parse_task_id(raw: &str) -> anyhow::Result<TaskId> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        anyhow::bail!("task id must not be empty");
    }
    let without_fragment = trimmed.split('#').next().unwrap_or(trimmed);
    let without_prefix = without_fragment
        .rsplit('/')
        .next()
        .unwrap_or(without_fragment);
    Ok(TaskId(without_prefix.trim().to_string()))
}

fn read_prompt(prompt_arg: Option<String>) -> anyhow::Result<String> {
    match prompt_arg {
        Some(p) if p != "-" => Ok(p),
        maybe_dash => {
            let force_stdin = matches!(maybe_dash.as_deref(), Some("-"));
            if std::io::stdin().is_terminal() && !force_stdin {
                anyhow::bail!(
                    "no prompt provided. Pass one as an argument or pipe it via stdin (use '-' to force stdin)."
                )
            }
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            if buf.trim().is_empty() {
                anyhow::bail!("no prompt provided via stdin (received empty input)");
            }
            Ok(buf)
        }
    }
}

async fn resolve_git_ref(branch_override: Option<&String>) -> String {
    if let Some(branch) = branch_override {
        let b = branch.trim();
        if !b.is_empty() {
            return b.to_string();
        }
    }

    if let Ok(cwd) = std::env::current_dir() {
        if let Some(branch) = codex_core::git_info::current_branch_name(&cwd).await {
            branch
        } else if let Some(branch) = codex_core::git_info::default_branch_name(&cwd).await {
            branch
        } else {
            "main".to_string()
        }
    } else {
        "main".to_string()
    }
}

async fn task_create(
    session: &Session,
    args: super::TaskCreateArgs,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let prompt = read_prompt(args.prompt)?;
    let env_id = env_api::resolve_environment_id(session, args.env.as_deref(), None).await?;
    let git_ref = resolve_git_ref(args.r#ref.as_ref()).await;

    let client = session.cloud_client()?;
    let created = client
        .create_task(
            &env_id,
            &prompt,
            &git_ref,
            args.qa_mode,
            args.agents as usize,
        )
        .await?;

    let url = task_url(&session.base_url, &created.id.0);

    match format {
        OutputFormat::Json => {
            let out = serde_json::json!({"task_id": created.id.0, "url": url, "environment_id": env_id, "ref": git_ref, "agents": args.agents});
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
        OutputFormat::Table => {
            println!("task_id: {}", created.id.0);
            println!("environment: {env_id}");
            println!("ref: {git_ref}");
            println!("agents: {}", args.agents);
            if args.print_url {
                println!("url: {url}");
            }
        }
    }

    Ok(())
}

async fn task_run(
    session: &Session,
    args: super::TaskRunArgs,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let prompt = read_prompt(args.prompt)?;
    let env_id = env_api::resolve_environment_id(session, args.env.as_deref(), None).await?;
    let git_ref = resolve_git_ref(args.r#ref.as_ref()).await;

    let client = session.cloud_client()?;
    let created = client
        .create_task(
            &env_id,
            &prompt,
            &git_ref,
            args.qa_mode,
            args.agents as usize,
        )
        .await?;

    let id = created.id;
    let url = task_url(&session.base_url, &id.0);
    let mut json_result = serde_json::json!({
        "task_id": id.0.clone(),
        "url": url.clone(),
        "status": Value::Null,
        "applied": false,
        "pr_created": false,
        "worktree_path": Value::Null
    });

    if args.events == EventsFormat::Jsonl {
        emit_jsonl(
            args.events,
            serde_json::json!({
                "type": "created",
                "ts": now_rfc3339(),
                "task_id": id.0.clone(),
                "url": url.clone(),
                "environment_id": env_id,
                "ref": git_ref.clone(),
                "agents": args.agents,
                "qa_mode": args.qa_mode,
            }),
        );
    } else if matches!(format, OutputFormat::Table) {
        println!("Created {url}");
    }

    // Watch until terminal.
    let outcome = watch_until_done(
        session,
        &id,
        args.poll,
        true,
        true,
        args.agents > 1,
        args.events,
    )
    .await?;
    json_result["status"] = Value::String(format!("{:?}", outcome.status));

    if args.apply {
        let summary = session.cloud_client()?.get_task_summary(id.clone()).await?;

        let use_worktree =
            args.worktree || args.worktree_path.is_some() || args.worktree_dir.is_some();
        let mut worktree_path: Option<PathBuf> = None;

        let apply_outcome = if use_worktree {
            let cwd = std::env::current_dir().context("failed to read current directory")?;
            let repo_root = worktree::resolve_repo_root(&cwd)?;
            let worktrees_root = args
                .worktree_dir
                .clone()
                .unwrap_or_else(|| session.codex_home.join("worktrees"));
            let base_ref = args
                .worktree_ref
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(git_ref.as_str());

            let path = args.worktree_path.clone().unwrap_or_else(|| {
                worktree::worktree_path_in(&worktrees_root, &repo_root, &id.0, None)
            });
            let path = worktree::ensure_worktree(&repo_root, &path, base_ref, args.worktree_clean)?;
            worktree_path = Some(path.clone());

            apply_task_in_dir(session, &id, None, &path, false).await?
        } else {
            apply_task_with_optional_attempt(session, &id, None, false).await?
        };

        let applied_ok = matches!(
            apply_outcome.status,
            ApplyStatus::Success | ApplyStatus::Partial
        );
        json_result["applied"] = Value::Bool(applied_ok);
        if let Some(p) = &worktree_path {
            json_result["worktree_path"] = Value::String(p.display().to_string());
        }

        if args.events == EventsFormat::Jsonl {
            emit_jsonl(
                args.events,
                serde_json::json!({
                    "type": "apply_result",
                    "ts": now_rfc3339(),
                    "task_id": id.0.clone(),
                    "url": url.clone(),
                    "worktree_path": worktree_path.as_ref().map(|p| p.display().to_string()),
                    "apply": apply_outcome,
                }),
            );
        } else if matches!(format, OutputFormat::Table) {
            if let Some(p) = &worktree_path {
                println!("worktree: {}", p.display());
            }
            print_apply(&apply_outcome);
        }

        if args.create_pr && applied_ok {
            let branch = args
                .pr_branch
                .unwrap_or_else(|| format!("codex/task_{}", id.0));
            let plan = CreatePrPlan {
                branch: branch.clone(),
                title: format!("Codex: {} ({})", summary.title, id.0),
                body: Some(format!("Created from Codex cloud task: {url}")),
                remote: "origin".to_string(),
            };

            if let Some(wt) = &worktree_path {
                create_pr_from_dir(wt, plan)?;
            } else {
                create_pr_from_worktree(plan)?;
            }

            json_result["pr_created"] = Value::Bool(true);
            if args.events == EventsFormat::Jsonl {
                emit_jsonl(
                    args.events,
                    serde_json::json!({
                        "type": "pr_created",
                        "ts": now_rfc3339(),
                        "task_id": id.0.clone(),
                        "branch": branch,
                        "remote": "origin",
                    }),
                );
            }
        }
    }

    if args.events == EventsFormat::Jsonl {
        emit_jsonl(
            args.events,
            serde_json::json!({
                "type": "run_complete",
                "ts": now_rfc3339(),
                "result": json_result,
            }),
        );
    } else if matches!(format, OutputFormat::Json) {
        println!("{}", serde_json::to_string_pretty(&json_result)?);
    }

    Ok(())
}

async fn task_list(
    session: &Session,
    args: super::TaskListArgs,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let env_id = match args.env.as_deref() {
        Some(sel) => Some(env_api::resolve_environment_id(session, Some(sel), None).await?),
        None => None,
    };

    let client = session.cloud_client()?;
    let page = client
        .list_tasks(env_id.as_deref(), Some(args.limit), args.cursor.as_deref())
        .await?;

    let mut tasks = page.tasks;
    if !args.include_reviews {
        tasks.retain(|t| !t.is_review);
    }

    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"tasks": tasks, "cursor": page.cursor})
                )?
            );
        }
        OutputFormat::Table => {
            for t in tasks {
                print_task_line(&t);
            }
            if let Some(cursor) = page.cursor {
                println!("cursor: {cursor}");
            }
        }
    }

    Ok(())
}

fn print_task_line(t: &TaskSummary) {
    let env = t
        .environment_label
        .clone()
        .or(t.environment_id.clone())
        .unwrap_or_else(|| "".to_string());
    let rel = format_relative_time_now(t.updated_at.clone());
    let sum = &t.summary;
    println!(
        "{}\t{:?}\t{}\t{}\tfiles={} +{} -{}\t{}",
        t.id.0.as_str(),
        &t.status,
        rel,
        env,
        sum.files_changed,
        sum.lines_added,
        sum.lines_removed,
        t.title.as_str()
    );
}

fn format_relative_time_now(ts: DateTime<Utc>) -> String {
    let now = Utc::now();
    let mut secs = (now - ts).num_seconds();
    if secs < 0 {
        secs = 0;
    }
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h");
    }
    ts.to_rfc3339()
}

async fn task_show(
    session: &Session,
    args: super::TaskShowArgs,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let id = parse_task_id(&args.task_id)?;
    let client = session.cloud_client()?;

    let summary = client.get_task_summary(id.clone()).await?;
    let mut out = serde_json::json!({});
    out["summary"] = serde_json::to_value(&summary)?;

    if args.text {
        let text = client.get_task_text(id.clone()).await?;
        out["text"] = serde_json::json!({
            "prompt": text.prompt,
            "messages": text.messages,
            "turn_id": text.turn_id,
            "sibling_turn_ids": text.sibling_turn_ids,
            "attempt_placement": text.attempt_placement,
            "attempt_status": format!("{:?}", text.attempt_status),
        });
    }

    if args.diff {
        let diff = client.get_task_diff(id.clone()).await?;
        out["diff"] = serde_json::to_value(diff)?;
    }

    if args.attempts {
        let text = client.get_task_text(id.clone()).await?;
        if let Some(turn_id) = text.turn_id {
            let attempts = client.list_sibling_attempts(id.clone(), turn_id).await?;
            let attempts_json: Vec<Value> = attempts
                .into_iter()
                .map(|a| {
                    serde_json::json!({
                        "turn_id": a.turn_id,
                        "attempt_placement": a.attempt_placement,
                        "created_at": a.created_at.map(|t| t.to_rfc3339()),
                        "status": format!("{:?}", a.status),
                        "diff": a.diff,
                        "messages": a.messages,
                    })
                })
                .collect();
            out["attempts"] = Value::Array(attempts_json);
        }
    }

    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&out)?),
        OutputFormat::Table => {
            let url = task_url(&session.base_url, &id.0);
            println!("id: {}", id.0);
            println!("url: {url}");
            println!(
                "title: {}",
                out["summary"]["title"].as_str().unwrap_or_default()
            );
            println!(
                "status: {}",
                out["summary"]["status"].as_str().unwrap_or("")
            );
            println!("updated_at: {}", summary.updated_at);
            if let Some(eid) = &summary.environment_id {
                println!("environment_id: {eid}");
            }
            if let Some(lbl) = &summary.environment_label {
                println!("environment_label: {lbl}");
            }
            println!(
                "diff_summary: files={} +{} -{}",
                summary.summary.files_changed,
                summary.summary.lines_added,
                summary.summary.lines_removed
            );

            if args.text {
                let text = client.get_task_text(id.clone()).await?;
                if let Some(prompt) = text.prompt.as_deref() {
                    println!("\n--- prompt ---\n{prompt}");
                }
                if !text.messages.is_empty() {
                    println!("\n--- messages ---");
                    for (i, m) in text.messages.iter().enumerate() {
                        println!("[{i}] {m}");
                    }
                }
                if let Some(turn_id) = text.turn_id.as_deref() {
                    println!("turn_id: {turn_id}");
                }
                if !text.sibling_turn_ids.is_empty() {
                    println!("sibling_turn_ids: {}", text.sibling_turn_ids.join(","));
                }
                println!("attempt_status: {:?}", text.attempt_status);
            }

            if args.attempts {
                let text = client.get_task_text(id.clone()).await?;
                if let Some(turn_id) = text.turn_id {
                    let attempts = client.list_sibling_attempts(id.clone(), turn_id).await?;
                    println!("\n--- attempts ---");
                    for a in attempts {
                        let placement = a
                            .attempt_placement
                            .map(|n| n.to_string())
                            .unwrap_or_else(|| "?".to_string());
                        println!("{placement}\t{:?}\t{}", a.status, a.turn_id);
                    }
                }
            }

            if args.diff {
                let diff = client.get_task_diff(id.clone()).await?;
                if let Some(diff) = diff {
                    println!("\n--- diff ---\n{diff}");
                } else {
                    println!("\n(no diff)");
                }
            }
        }
    }

    Ok(())
}

async fn pick_attempt_diff(
    client: &impl CloudBackend,
    task: &TaskId,
    attempt_selector: &str,
) -> anyhow::Result<String> {
    let text = client.get_task_text(task.clone()).await?;
    let Some(turn_id) = text.turn_id else {
        anyhow::bail!("task has no assistant turn yet; cannot pick attempt");
    };
    let attempts = client.list_sibling_attempts(task.clone(), turn_id).await?;

    // attempt selector can be integer placement or turn_id
    if let Ok(n) = attempt_selector.parse::<i64>() {
        let found = attempts
            .iter()
            .find(|a| a.attempt_placement == Some(n))
            .and_then(|a| a.diff.clone());
        return found.ok_or_else(|| anyhow::anyhow!("no diff found for attempt placement {n}"));
    }

    let found = attempts
        .iter()
        .find(|a| a.turn_id == attempt_selector)
        .and_then(|a| a.diff.clone());
    found.ok_or_else(|| anyhow::anyhow!("no diff found for attempt '{attempt_selector}'"))
}

async fn task_diff(
    session: &Session,
    args: super::TaskDiffArgs,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let id = parse_task_id(&args.task_id)?;
    let client = session.cloud_client()?;

    let diff = if let Some(attempt) = args.attempt {
        Some(pick_attempt_diff(&client, &id, &attempt).await?)
    } else {
        client.get_task_diff(id.clone()).await?
    };

    match format {
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({"task_id": id.0, "diff": diff}))?
        ),
        OutputFormat::Table => {
            if let Some(d) = diff {
                print!("{d}");
            } else {
                println!("(no diff)");
            }
        }
    }
    Ok(())
}

async fn task_watch(
    session: &Session,
    args: super::TaskWatchArgs,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let id = parse_task_id(&args.task_id)?;
    let outcome = watch_until_done(
        session,
        &id,
        args.poll,
        args.exit_on_done,
        args.stream_messages,
        args.attempts,
        args.events,
    )
    .await?;

    // When streaming jsonl events, the watcher already emitted a terminal event.
    if args.events != EventsFormat::Jsonl {
        match format {
            OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&outcome.json)?),
            OutputFormat::Table => {
                println!("status: {:?}", outcome.status);
                println!("url: {}", task_url(&session.base_url, &id.0));
            }
        }
    }
    Ok(())
}

struct WatchOutcome {
    status: TaskStatus,
    json: Value,
}

async fn watch_until_done(
    session: &Session,
    id: &TaskId,
    poll_secs: u64,
    exit_on_done: bool,
    stream_messages: bool,
    show_attempts: bool,
    events: EventsFormat,
) -> anyhow::Result<WatchOutcome> {
    let client = session.cloud_client()?;
    let poll = Duration::from_secs(poll_secs.max(1));

    let mut last_status: Option<TaskStatus> = None;
    let mut printed_msgs = 0usize;
    let mut last_attempts: Option<Vec<(Option<i64>, AttemptStatus)>> = None;

    loop {
        let summary = client.get_task_summary(id.clone()).await?;
        if last_status.as_ref() != Some(&summary.status) {
            last_status = Some(summary.status.clone());
            if events == EventsFormat::Jsonl {
                emit_jsonl(
                    events,
                    serde_json::json!({
                        "type": "status",
                        "ts": now_rfc3339(),
                        "task_id": id.0.clone(),
                        "url": task_url(&session.base_url, &id.0),
                        "status": format!("{:?}", summary.status),
                        "title": &summary.title,
                        "updated_at": &summary.updated_at,
                    }),
                );
            } else {
                eprintln!("{:?}: {}", summary.status, summary.title);
            }
        }

        if stream_messages {
            let text = client.get_task_text(id.clone()).await.unwrap_or_default();
            if printed_msgs < text.messages.len() {
                for (i, m) in text.messages.iter().enumerate().skip(printed_msgs) {
                    if events == EventsFormat::Jsonl {
                        emit_jsonl(
                            events,
                            serde_json::json!({
                                "type": "message",
                                "ts": now_rfc3339(),
                                "task_id": id.0.clone(),
                                "index": i,
                                "message": m,
                            }),
                        );
                    } else {
                        println!("{m}");
                    }
                }
                printed_msgs = text.messages.len();
            }
        }

        if show_attempts {
            let text = client.get_task_text(id.clone()).await.unwrap_or_default();
            if let Some(turn_id) = text.turn_id {
                if let Ok(attempts) = client.list_sibling_attempts(id.clone(), turn_id).await {
                    let compact: Vec<(Option<i64>, AttemptStatus)> = attempts
                        .iter()
                        .map(|a| (a.attempt_placement, a.status.clone()))
                        .collect();
                    if last_attempts.as_ref() != Some(&compact) {
                        last_attempts = Some(compact);
                        if events == EventsFormat::Jsonl {
                            let attempts_json: Vec<Value> = attempts
                                .iter()
                                .map(|a| {
                                    serde_json::json!({
                                        "turn_id": &a.turn_id,
                                        "attempt_placement": a.attempt_placement,
                                        "created_at": a.created_at.as_ref().map(|t| t.to_rfc3339()),
                                        "status": format!("{:?}", a.status),
                                        "has_diff": a.diff.is_some(),
                                        "message_count": a.messages.len(),
                                    })
                                })
                                .collect();
                            emit_jsonl(
                                events,
                                serde_json::json!({
                                    "type": "attempts",
                                    "ts": now_rfc3339(),
                                    "task_id": id.0.clone(),
                                    "attempts": attempts_json,
                                }),
                            );
                        } else {
                            eprintln!("attempts: {}", attempts_summary(&attempts));
                        }
                    }
                }
            }
        }

        let terminal = matches!(
            summary.status,
            TaskStatus::Ready | TaskStatus::Applied | TaskStatus::Error
        );
        if terminal && exit_on_done {
            let json = serde_json::json!({
                "task_id": id.0.clone(),
                "status": format!("{:?}", summary.status),
                "title": &summary.title,
                "updated_at": &summary.updated_at,
                "diff_summary": &summary.summary,
            });
            if events == EventsFormat::Jsonl {
                emit_jsonl(
                    events,
                    serde_json::json!({
                        "type": "done",
                        "ts": now_rfc3339(),
                        "task_id": id.0.clone(),
                        "url": task_url(&session.base_url, &id.0),
                        "status": format!("{:?}", summary.status),
                        "title": &summary.title,
                        "updated_at": &summary.updated_at,
                        "diff_summary": &summary.summary,
                    }),
                );
            }
            return Ok(WatchOutcome {
                status: summary.status,
                json,
            });
        }

        tokio::time::sleep(poll).await;
    }
}

fn attempts_summary(attempts: &[codex_cloud_tasks_client::TurnAttempt]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for a in attempts {
        let placement = a
            .attempt_placement
            .map(|n| n.to_string())
            .unwrap_or_else(|| "?".to_string());
        parts.push(format!("{placement}:{:?}", &a.status));
    }
    parts.join(" ")
}

fn is_unified_diff(diff: &str) -> bool {
    let t = diff.trim_start();
    if t.starts_with("diff --git ") {
        return true;
    }
    let has_dash_headers = diff.contains("\n--- ") && diff.contains("\n+++ ");
    let has_hunk = diff.contains("\n@@ ") || diff.starts_with("@@ ");
    has_dash_headers && has_hunk
}

async fn fetch_diff_for_apply(
    client: &impl CloudBackend,
    id: &TaskId,
    attempt: Option<&str>,
) -> anyhow::Result<String> {
    if let Some(sel) = attempt {
        return pick_attempt_diff(client, id, sel).await;
    }
    let diff = client
        .get_task_diff(id.clone())
        .await?
        .ok_or_else(|| anyhow::anyhow!("No diff available for task {}", id.0))?;
    Ok(diff)
}

fn apply_diff_in_dir(
    task_id: &str,
    diff: &str,
    dir: &Path,
    preflight: bool,
) -> anyhow::Result<ApplyOutcome> {
    if !is_unified_diff(diff) {
        return Ok(ApplyOutcome {
            applied: false,
            status: ApplyStatus::Error,
            message: "Expected unified git diff; backend returned an incompatible format."
                .to_string(),
            skipped_paths: Vec::new(),
            conflict_paths: Vec::new(),
        });
    }

    let req = codex_git::ApplyGitRequest {
        cwd: dir.to_path_buf(),
        diff: diff.to_string(),
        revert: false,
        preflight,
    };
    let r = codex_git::apply_git_patch(&req)
        .map_err(|e| anyhow::anyhow!("git apply failed to run: {e}"))?;

    let status = if r.exit_code == 0 {
        ApplyStatus::Success
    } else if !r.applied_paths.is_empty() || !r.conflicted_paths.is_empty() {
        ApplyStatus::Partial
    } else {
        ApplyStatus::Error
    };
    let applied = matches!(status, ApplyStatus::Success) && !preflight;

    let message = if preflight {
        match status {
            ApplyStatus::Success => {
                format!("Preflight passed for task {task_id} (applies cleanly)")
            }
            ApplyStatus::Partial => format!(
                "Preflight: patch does not fully apply for task {task_id} (applied={}, skipped={}, conflicts={})",
                r.applied_paths.len(),
                r.skipped_paths.len(),
                r.conflicted_paths.len()
            ),
            ApplyStatus::Error => format!(
                "Preflight failed for task {task_id} (applied={}, skipped={}, conflicts={})",
                r.applied_paths.len(),
                r.skipped_paths.len(),
                r.conflicted_paths.len()
            ),
        }
    } else {
        match status {
            ApplyStatus::Success => {
                format!(
                    "Applied task {task_id} locally ({} files)",
                    r.applied_paths.len()
                )
            }
            ApplyStatus::Partial => format!(
                "Apply partially succeeded for task {task_id} (applied={}, skipped={}, conflicts={})",
                r.applied_paths.len(),
                r.skipped_paths.len(),
                r.conflicted_paths.len()
            ),
            ApplyStatus::Error => format!(
                "Apply failed for task {task_id} (applied={}, skipped={}, conflicts={})",
                r.applied_paths.len(),
                r.skipped_paths.len(),
                r.conflicted_paths.len()
            ),
        }
    };

    Ok(ApplyOutcome {
        applied,
        status,
        message,
        skipped_paths: r.skipped_paths,
        conflict_paths: r.conflicted_paths,
    })
}

async fn apply_task_in_dir(
    session: &Session,
    id: &TaskId,
    attempt: Option<&str>,
    dir: &Path,
    preflight: bool,
) -> anyhow::Result<ApplyOutcome> {
    let client = session.cloud_client()?;
    let diff = fetch_diff_for_apply(&client, id, attempt).await?;
    apply_diff_in_dir(&id.0, &diff, dir, preflight)
}

async fn apply_task_with_optional_attempt(
    session: &Session,
    id: &TaskId,
    attempt: Option<&str>,
    preflight: bool,
) -> anyhow::Result<ApplyOutcome> {
    let client = session.cloud_client()?;
    let diff_override = match attempt {
        Some(sel) => Some(pick_attempt_diff(&client, id, sel).await?),
        None => None,
    };

    if preflight {
        client
            .apply_task_preflight(id.clone(), diff_override)
            .await
            .map_err(|e| e.into())
    } else {
        client
            .apply_task(id.clone(), diff_override)
            .await
            .map_err(|e| e.into())
    }
}

fn print_apply(outcome: &ApplyOutcome) {
    println!("{}", outcome.message);
    if !outcome.skipped_paths.is_empty() {
        println!("skipped_paths: {}", outcome.skipped_paths.join(", "));
    }
    if !outcome.conflict_paths.is_empty() {
        println!("conflict_paths: {}", outcome.conflict_paths.join(", "));
    }
}

async fn task_apply(
    session: &Session,
    args: super::TaskApplyArgs,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let id = parse_task_id(&args.task_id)?;
    let use_worktree = args.worktree || args.worktree_path.is_some() || args.worktree_dir.is_some();
    let mut worktree_path: Option<PathBuf> = None;

    let outcome = if use_worktree {
        let cwd = std::env::current_dir().context("failed to read current directory")?;
        let repo_root = worktree::resolve_repo_root(&cwd)?;
        let worktrees_root = args
            .worktree_dir
            .clone()
            .unwrap_or_else(|| session.codex_home.join("worktrees"));
        let base_ref = resolve_git_ref(args.worktree_ref.as_ref()).await;
        let path = args.worktree_path.clone().unwrap_or_else(|| {
            worktree::worktree_path_in(&worktrees_root, &repo_root, &id.0, args.attempt.as_deref())
        });
        let path = worktree::ensure_worktree(&repo_root, &path, &base_ref, args.worktree_clean)?;
        worktree_path = Some(path.clone());
        apply_task_in_dir(session, &id, args.attempt.as_deref(), &path, args.preflight).await?
    } else {
        apply_task_with_optional_attempt(session, &id, args.attempt.as_deref(), args.preflight)
            .await?
    };

    let mut pr_created = false;
    if args.create_pr && !args.preflight {
        if matches!(outcome.status, ApplyStatus::Success | ApplyStatus::Partial) {
            let client = session.cloud_client()?;
            let summary = client.get_task_summary(id.clone()).await?;
            let url = task_url(&session.base_url, &id.0);
            let branch = args
                .pr_branch
                .unwrap_or_else(|| format!("codex/task_{}", id.0));
            let plan = CreatePrPlan {
                branch,
                title: format!("Codex: {} ({})", summary.title, id.0),
                body: Some(format!("Created from Codex cloud task: {url}")),
                remote: "origin".to_string(),
            };
            if let Some(wt) = &worktree_path {
                create_pr_from_dir(wt, plan)?;
            } else {
                create_pr_from_worktree(plan)?;
            }
            pr_created = true;
        }
    }

    match format {
        OutputFormat::Json => {
            if use_worktree || args.create_pr {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "task_id": id.0,
                        "worktree_path": worktree_path.map(|p| p.display().to_string()),
                        "apply": outcome,
                        "pr_created": pr_created,
                    }))?
                );
            } else {
                println!("{}", serde_json::to_string_pretty(&outcome)?);
            }
        }
        OutputFormat::Table => {
            if let Some(p) = &worktree_path {
                println!("worktree: {}", p.display());
            }
            print_apply(&outcome)
        }
    }

    Ok(())
}

async fn task_prs(
    session: &Session,
    args: super::TaskPrsArgs,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let id = parse_task_id(&args.task_id)?;
    let backend = session.backend_client()?;
    let (_parsed, body, _ct) = backend.get_task_details_with_body(&id.0).await?;
    let v: Value = serde_json::from_str(&body)?;

    let prs = v
        .get("task")
        .and_then(|t| t.get("external_pull_requests"))
        .and_then(|prs| prs.as_array())
        .cloned()
        .unwrap_or_default();

    match format {
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({"task_id": id.0, "external_pull_requests": prs})
            )?
        ),
        OutputFormat::Table => {
            if prs.is_empty() {
                println!("(no external pull requests)");
            } else {
                for pr in prs {
                    let url = pr
                        .get("pull_request")
                        .and_then(|p| p.get("url"))
                        .and_then(|u| u.as_str())
                        .unwrap_or("<unknown>");
                    let number = pr
                        .get("pull_request")
                        .and_then(|p| p.get("number"))
                        .and_then(|n| n.as_i64())
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| "?".to_string());
                    let state = pr
                        .get("pull_request")
                        .and_then(|p| p.get("state"))
                        .and_then(|s| s.as_str())
                        .unwrap_or("");
                    println!("#{number}\t{state}\t{url}");
                }
            }
        }
    }

    Ok(())
}
