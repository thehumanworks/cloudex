use anyhow::Context;
use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Debug, Clone)]
pub struct PrCreateResult {
    pub branch: String,
    pub remote: String,
    pub pr_url: Option<String>,
    pub used_gh: bool,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone)]
pub struct CreatePrPlan {
    pub branch: String,
    pub title: String,
    pub body: Option<String>,
    pub remote: String,
}

fn run_git_in(dir: &Path, args: &[&str]) -> anyhow::Result<()> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::inherit())
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

    if !out.stdout.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&out.stdout));
    }
    if !out.stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(())
}

fn git_stdout_in(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| format!("failed to run git {args:?}"))?;
    if !out.status.success() {
        anyhow::bail!("git {args:?} failed with status {}", out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn command_exists(cmd: &str) -> bool {
    Command::new(cmd)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn run_git_capture(dir: &Path, args: &[&str]) -> anyhow::Result<(String, String)> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to run git {args:?}"))?;

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();

    if !out.status.success() {
        anyhow::bail!(
            "git {args:?} failed with status {}\nstdout={}\nstderr={}",
            out.status,
            stdout,
            stderr
        );
    }
    Ok((stdout, stderr))
}

fn parse_first_pr_url(s: &str) -> Option<String> {
    for token in s.split_whitespace() {
        if token.starts_with("https://")
            && token.contains("github.com")
            && (token.contains("/pull/") || token.contains("/pulls/"))
        {
            return Some(token.trim().trim_end_matches('.').to_string());
        }
    }
    None
}

pub fn create_pr_from_worktree(plan: CreatePrPlan) -> anyhow::Result<()> {
    let cwd = std::env::current_dir().context("failed to read current directory")?;
    create_pr_from_dir(&cwd, plan)
}

pub fn create_pr_from_dir(dir: &Path, plan: CreatePrPlan) -> anyhow::Result<()> {
    // Ensure we're in a repo.
    let inside = git_stdout_in(dir, &["rev-parse", "--is-inside-work-tree"]).unwrap_or_default();
    if inside.trim() != "true" {
        anyhow::bail!("Not inside a git repository: {}", dir.display());
    }

    // Switch/create branch.
    let branch_exists = Command::new("git")
        .args(["rev-parse", "--verify", &plan.branch])
        .current_dir(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if branch_exists {
        run_git_in(dir, &["switch", &plan.branch])
            .or_else(|_| run_git_in(dir, &["checkout", &plan.branch]))?;
    } else {
        run_git_in(dir, &["switch", "-c", &plan.branch])
            .or_else(|_| run_git_in(dir, &["checkout", "-b", &plan.branch]))?;
    }

    // Commit changes.
    run_git_in(dir, &["add", "-A"])?;

    // If there is nothing to commit, bail early.
    let diff = git_stdout_in(dir, &["status", "--porcelain"]).unwrap_or_default();
    if diff.trim().is_empty() {
        anyhow::bail!("No local changes to commit (working tree clean).");
    }

    // git commit -m ...
    let commit_args = vec!["-c", "commit.gpgsign=false", "commit", "-m", &plan.title];
    run_git_in(dir, &commit_args)?;

    // Push.
    run_git_in(dir, &["push", "-u", &plan.remote, &plan.branch])?;

    if command_exists("gh") {
        // gh pr create --title ... --body ...
        let mut args: Vec<OsString> = vec![
            "pr".into(),
            "create".into(),
            "--title".into(),
            plan.title.clone().into(),
        ];
        if let Some(body) = &plan.body {
            args.push("--body".into());
            args.push(body.clone().into());
        } else {
            args.push("--fill".into());
        }

        let status = Command::new("gh")
            .args(args)
            .current_dir(dir)
            .stdin(Stdio::inherit())
            .output()
            .context("failed to run gh pr create")?;

        if !status.status.success() {
            anyhow::bail!(
                "gh pr create failed with status {}\nstdout={}\nstderr={}",
                status.status,
                String::from_utf8_lossy(&status.stdout),
                String::from_utf8_lossy(&status.stderr)
            );
        }
        if !status.stdout.is_empty() {
            eprint!("{}", String::from_utf8_lossy(&status.stdout));
        }
        if !status.stderr.is_empty() {
            eprint!("{}", String::from_utf8_lossy(&status.stderr));
        }
    } else {
        eprintln!(
            "`gh` not found. Branch pushed; create a PR manually (or install GitHub CLI).\n  branch: {}\n  remote: {}",
            plan.branch, plan.remote
        );
    }

    Ok(())
}

/// Create a PR using only non-interactive child process IO.
///
/// This is intended for TUI/automation callers that don't want subprocess output to
/// corrupt the terminal UI.
pub fn create_pr_from_dir_capture(
    dir: &Path,
    plan: CreatePrPlan,
) -> anyhow::Result<PrCreateResult> {
    // Ensure we're in a repo.
    let inside = git_stdout_in(dir, &["rev-parse", "--is-inside-work-tree"]).unwrap_or_default();
    if inside.trim() != "true" {
        anyhow::bail!("Not inside a git repository: {}", dir.display());
    }

    let mut agg_out = String::new();
    let mut agg_err = String::new();

    // Switch/create branch.
    let branch_exists = Command::new("git")
        .args(["rev-parse", "--verify", plan.branch.as_str()])
        .current_dir(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if branch_exists {
        let (o, e) = run_git_capture(dir, &["switch", plan.branch.as_str()])
            .or_else(|_| run_git_capture(dir, &["checkout", plan.branch.as_str()]))?;
        agg_out.push_str(&o);
        agg_err.push_str(&e);
    } else {
        let (o, e) = run_git_capture(dir, &["switch", "-c", plan.branch.as_str()])
            .or_else(|_| run_git_capture(dir, &["checkout", "-b", plan.branch.as_str()]))?;
        agg_out.push_str(&o);
        agg_err.push_str(&e);
    }

    // Commit changes.
    let (o, e) = run_git_capture(dir, &["add", "-A"])?;
    agg_out.push_str(&o);
    agg_err.push_str(&e);

    // If there is nothing to commit, bail early.
    let diff = git_stdout_in(dir, &["status", "--porcelain"]).unwrap_or_default();
    if diff.trim().is_empty() {
        anyhow::bail!("No local changes to commit (working tree clean).");
    }

    // git commit -m ...
    let (o, e) = run_git_capture(
        dir,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            plan.title.as_str(),
        ],
    )?;
    agg_out.push_str(&o);
    agg_err.push_str(&e);

    // Push.
    let (o, e) = run_git_capture(
        dir,
        &["push", "-u", plan.remote.as_str(), plan.branch.as_str()],
    )?;
    agg_out.push_str(&o);
    agg_err.push_str(&e);

    let mut pr_url: Option<String> = None;
    let mut used_gh = false;

    if command_exists("gh") {
        used_gh = true;
        // gh pr create --title ... --body ...
        let mut args: Vec<OsString> = vec![
            "pr".into(),
            "create".into(),
            "--title".into(),
            plan.title.clone().into(),
        ];
        if let Some(body) = &plan.body {
            args.push("--body".into());
            args.push(body.clone().into());
        } else {
            args.push("--fill".into());
        }

        let out = Command::new("gh")
            .args(args)
            .current_dir(dir)
            .stdin(Stdio::null())
            .output()
            .context("failed to run gh pr create")?;

        let gh_out = String::from_utf8_lossy(&out.stdout).to_string();
        let gh_err = String::from_utf8_lossy(&out.stderr).to_string();
        if !out.status.success() {
            anyhow::bail!(
                "gh pr create failed with status {}\nstdout={}\nstderr={}",
                out.status,
                gh_out,
                gh_err
            );
        }

        pr_url = parse_first_pr_url(&gh_out).or_else(|| parse_first_pr_url(&gh_err));
        agg_out.push_str(&gh_out);
        agg_err.push_str(&gh_err);
    } else {
        agg_err.push_str(
            "`gh` not found. Branch pushed; create a PR manually (or install GitHub CLI).\n",
        );
    }

    Ok(PrCreateResult {
        branch: plan.branch,
        remote: plan.remote,
        pr_url,
        used_gh,
        stdout: agg_out,
        stderr: agg_err,
    })
}
