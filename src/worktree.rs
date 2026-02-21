use anyhow::Context;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn run_checked(mut cmd: Command) -> anyhow::Result<std::process::Output> {
    let out = cmd
        .output()
        .with_context(|| format!("failed to run command: {cmd:?}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "command failed: {:?}\nstatus={}\nstdout={}\nstderr={} ",
            cmd,
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(out)
}

pub fn git_stdout_in(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(dir);
    let out = run_checked(cmd)?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn run_git_in(dir: &Path, args: &[&str]) -> anyhow::Result<()> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to run git {args:?}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "git {args:?} failed with status {}\nstdout={}\nstderr={}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

pub fn resolve_repo_root(cwd: &Path) -> anyhow::Result<PathBuf> {
    let root = git_stdout_in(cwd, &["rev-parse", "--show-toplevel"])
        .context("not inside a git repository")?;
    Ok(PathBuf::from(root))
}

fn repo_name_from_root(root: &Path) -> String {
    root.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("repo")
        .to_string()
}

fn normalize_repo_slug(owner_repo: &str) -> String {
    owner_repo
        .trim()
        .trim_end_matches(".git")
        .replace('/', "__")
        .replace('\\', "__")
}

fn parse_github_owner_repo(remote: &str) -> Option<String> {
    let r = remote.trim();

    // git@github.com:owner/repo.git
    if let Some(rest) = r.strip_prefix("git@github.com:") {
        let rest = rest.trim_end_matches(".git");
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() >= 2 {
            return Some(format!("{}/{}", parts[0], parts[1]));
        }
    }

    // https://github.com/owner/repo.git, ssh://git@github.com/owner/repo.git
    if let Some(idx) = r.find("github.com/") {
        let rest = &r[idx + "github.com/".len()..];
        let rest = rest.trim_start_matches('/').trim_end_matches(".git");
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() >= 2 {
            return Some(format!("{}/{}", parts[0], parts[1]));
        }
    }

    None
}

pub fn repo_slug(repo_root: &Path) -> String {
    if let Ok(remote) = git_stdout_in(repo_root, &["config", "--get", "remote.origin.url"]) {
        if let Some(owner_repo) = parse_github_owner_repo(&remote) {
            return normalize_repo_slug(&owner_repo);
        }
    }
    normalize_repo_slug(&repo_name_from_root(repo_root))
}

fn sanitize_component(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    let out = out.trim_matches('_').to_string();
    if out.is_empty() {
        "x".to_string()
    } else if out.len() > 80 {
        out[..80].to_string()
    } else {
        out
    }
}

/// Compute a deterministic worktree path under an explicit `worktrees_root`.
///
/// The final shape is:
///
///   `<worktrees_root>/<repo_slug>/task_<task_id>[_attempt_<selector>]`
pub fn worktree_path_in(
    worktrees_root: &Path,
    repo_root: &Path,
    task_id: &str,
    attempt: Option<&str>,
) -> PathBuf {
    let slug = repo_slug(repo_root);
    let mut name = format!("task_{}", sanitize_component(task_id));
    if let Some(a) = attempt {
        name.push_str("_attempt_");
        name.push_str(&sanitize_component(a));
    }
    worktrees_root.join(slug).join(name)
}

fn is_git_worktree_dir(path: &Path) -> bool {
    path.join(".git").exists()
}

pub fn ensure_worktree(
    repo_root: &Path,
    worktree_path: &Path,
    base_ref: &str,
    clean: bool,
) -> anyhow::Result<PathBuf> {
    if clean {
        if worktree_path.exists() {
            if is_git_worktree_dir(worktree_path) {
                let p = worktree_path.display().to_string();
                run_git_in(repo_root, &["worktree", "remove", "-f", &p])
                    .context("git worktree remove failed")?;
                let _ = run_git_in(repo_root, &["worktree", "prune"]);
            } else {
                anyhow::bail!(
                    "refusing to delete existing directory that doesn't look like a git worktree: {}",
                    worktree_path.display()
                );
            }
        }
    }

    if worktree_path.exists() {
        if !is_git_worktree_dir(worktree_path) {
            anyhow::bail!(
                "worktree path exists but doesn't look like a git worktree: {}",
                worktree_path.display()
            );
        }

        let dirty = !git_stdout_in(worktree_path, &["status", "--porcelain"])
            .unwrap_or_default()
            .trim()
            .is_empty();
        if dirty {
            anyhow::bail!(
                "worktree is not clean: {} (use --worktree-clean to recreate it)",
                worktree_path.display()
            );
        }

        return Ok(worktree_path.to_path_buf());
    }

    std::fs::create_dir_all(worktree_path.parent().unwrap_or_else(|| Path::new(".")))
        .with_context(|| {
            format!(
                "failed to create worktree parent dir: {}",
                worktree_path.display()
            )
        })?;

    let base_ref = base_ref.trim();
    if base_ref.is_empty() {
        anyhow::bail!("worktree base ref must not be empty");
    }

    // Detached worktree avoids conflicts when the base branch is checked out elsewhere.
    let p = worktree_path.display().to_string();
    run_git_in(repo_root, &["worktree", "add", "--detach", &p, base_ref])
        .context("git worktree add failed")?;

    Ok(worktree_path.to_path_buf())
}
