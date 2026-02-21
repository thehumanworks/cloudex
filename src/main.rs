mod auth;
mod env_api;
mod pr;
mod tasks;
mod tui;
mod worktree;

use crate::auth::Session;
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum OutputFormat {
    Table,
    Json,
}

impl Default for OutputFormat {
    fn default() -> Self {
        Self::Table
    }
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum EventsFormat {
    /// No structured events (default).
    None,
    /// Emit JSON Lines events (one JSON object per line).
    Jsonl,
}

impl Default for EventsFormat {
    fn default() -> Self {
        Self::None
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "cloudex",
    version,
    about = "Cloudex: a richer Codex Cloud CLI (tasks, environments, apply, and PR helpers)."
)]
struct Cli {
    /// Codex base URL. For ChatGPT, use https://chatgpt.com/backend-api
    #[arg(long, env = "CODEX_CLOUD_TASKS_BASE_URL")]
    base_url: Option<String>,

    /// Override CODEX_HOME (defaults to ~/.codex). Equivalent to setting CODEX_HOME env var.
    #[arg(long)]
    codex_home: Option<PathBuf>,

    /// Output format.
    #[arg(long, value_enum, default_value_t)]
    output: OutputFormat,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Show resolved auth/session info.
    Auth(AuthCmd),

    /// Environment operations.
    #[command(subcommand)]
    Env(EnvCmd),

    /// Task operations.
    #[command(subcommand)]
    Task(TaskCmd),

    /// Show Codex rate limits / credits.
    Usage,

    /// Fetch managed requirements file from the backend.
    Requirements,

    /// Low-level HTTP request helper (power-user / debugging).
    Request(RequestCmd),

    /// Launch an interactive terminal UI.
    Tui(TuiArgs),
}

#[derive(Debug, Args)]
struct TuiArgs {
    /// Initial environment id/label filter for the task list.
    #[arg(long)]
    env: Option<String>,

    /// Task list refresh interval (seconds).
    #[arg(long, default_value_t = 5)]
    refresh: u64,

    /// Poll interval (seconds) for the selected task details view.
    #[arg(long, default_value_t = 3)]
    poll: u64,

    /// Number of tasks to show in the list.
    #[arg(long, default_value_t = 20)]
    limit: i64,

    /// Root directory where worktrees are created from the TUI when worktree apply is enabled.
    /// Defaults to $CODEX_HOME/worktrees.
    #[arg(long)]
    worktree_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct AuthCmd {
    /// Print the bearer token (DANGEROUS). Use only for debugging.
    #[arg(long)]
    show_token: bool,
}

#[derive(Debug, Subcommand)]
enum EnvCmd {
    /// List environments visible to the current account.
    List(EnvListArgs),

    /// Attempt to auto-pick an environment based on the current git remote.
    Detect(EnvDetectArgs),

    /// Create an environment (UNOFFICIAL / best-effort; may not work for all accounts).
    Create(EnvCreateArgs),

    /// Delete an environment (UNOFFICIAL / best-effort).
    Delete(EnvDeleteArgs),
}

#[derive(Debug, Args)]
struct EnvListArgs {
    /// Only show environments that match this substring (label or id).
    #[arg(long)]
    filter: Option<String>,

    /// When present, also query by-repo environments for the given repo (github only: owner/repo)
    #[arg(long)]
    repo: Option<String>,
}

#[derive(Debug, Args)]
struct EnvDetectArgs {
    /// Prefer an environment with this label (case-insensitive) if it exists.
    #[arg(long)]
    label: Option<String>,
}

#[derive(Debug, Args)]
struct EnvCreateArgs {
    /// Friendly label.
    #[arg(long)]
    label: Option<String>,

    /// GitHub repo in owner/repo form.
    #[arg(long)]
    repo: Option<String>,

    /// Raw JSON body to send. Use '-' to read from stdin.
    #[arg(long)]
    raw_json: Option<String>,
}

#[derive(Debug, Args)]
struct EnvDeleteArgs {
    /// Environment id.
    env_id: String,
}

#[derive(Debug, Subcommand)]
enum TaskCmd {
    /// Create a task.
    Create(TaskCreateArgs),

    /// Convenience: create a task, watch it, then optionally apply and/or create a PR.
    Run(TaskRunArgs),

    /// List tasks.
    List(TaskListArgs),

    /// Show task details.
    Show(TaskShowArgs),

    /// Print the diff for a task.
    Diff(TaskDiffArgs),

    /// Watch a task until completion.
    Watch(TaskWatchArgs),

    /// Apply a task diff locally.
    Apply(TaskApplyArgs),

    /// Show PRs associated with a task (if any).
    Prs(TaskPrsArgs),
}

#[derive(Debug, Args)]
struct TaskCreateArgs {
    /// Environment id or label. If omitted, attempts to autodetect.
    #[arg(long)]
    env: Option<String>,

    /// Branch or commit SHA. Defaults to current branch, then default branch, then 'main'.
    #[arg(long)]
    r#ref: Option<String>,

    /// The task prompt. Use '-' to read from stdin.
    #[arg(value_name = "PROMPT")]
    prompt: Option<String>,

    /// Run the environment in QA mode.
    #[arg(long, default_value_t = false)]
    qa_mode: bool,

    /// Number of concurrent agent attempts (best-of-N). 1-4.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=4))]
    agents: u8,

    /// Print the web URL for the created task.
    #[arg(long, default_value_t = true)]
    print_url: bool,
}

#[derive(Debug, Args)]
struct TaskRunArgs {
    /// Environment id or label. If omitted, attempts to autodetect.
    #[arg(long)]
    env: Option<String>,

    /// Branch or commit SHA.
    #[arg(long)]
    r#ref: Option<String>,

    /// The task prompt. Use '-' to read from stdin.
    #[arg(value_name = "PROMPT")]
    prompt: Option<String>,

    /// Run the environment in QA mode.
    #[arg(long, default_value_t = false)]
    qa_mode: bool,

    /// Number of concurrent agent attempts (best-of-N). 1-4.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=4))]
    agents: u8,

    /// Poll interval (seconds).
    #[arg(long, default_value_t = 3)]
    poll: u64,

    /// Emit structured progress events as JSON Lines (one JSON object per line).
    #[arg(long, value_enum, default_value_t)]
    events: EventsFormat,

    /// Apply the resulting diff after the task completes.
    #[arg(long, default_value_t = false)]
    apply: bool,

    /// Create a PR locally after applying (uses `gh` if installed).
    #[arg(long, default_value_t = false)]
    create_pr: bool,

    /// Branch name to use when creating a PR.
    #[arg(long)]
    pr_branch: Option<String>,

    /// Apply inside a dedicated git worktree (recommended for isolation).
    #[arg(long, default_value_t = false)]
    worktree: bool,

    /// Explicit worktree path to use/create. Implies --worktree.
    #[arg(long)]
    worktree_path: Option<PathBuf>,

    /// Root directory where worktrees are created when --worktree is set.
    /// Defaults to $CODEX_HOME/worktrees.
    #[arg(long)]
    worktree_dir: Option<PathBuf>,

    /// Base ref to checkout when creating the worktree (defaults to the task --ref).
    #[arg(long)]
    worktree_ref: Option<String>,

    /// If the worktree already exists, remove/recreate it before applying.
    #[arg(long, default_value_t = false)]
    worktree_clean: bool,
}

#[derive(Debug, Args)]
struct TaskListArgs {
    /// Filter by environment id or label.
    #[arg(long)]
    env: Option<String>,

    /// Number of tasks.
    #[arg(long, default_value_t = 20)]
    limit: i64,

    /// Pagination cursor.
    #[arg(long)]
    cursor: Option<String>,

    /// Include review tasks.
    #[arg(long, default_value_t = false)]
    include_reviews: bool,
}

#[derive(Debug, Args)]
struct TaskShowArgs {
    task_id: String,

    /// Also print prompt and assistant messages.
    #[arg(long, default_value_t = true)]
    text: bool,

    /// Also print diff summary and/or full diff.
    #[arg(long)]
    diff: bool,

    /// Show sibling attempts when available.
    #[arg(long, default_value_t = false)]
    attempts: bool,
}

#[derive(Debug, Args)]
struct TaskDiffArgs {
    task_id: String,

    /// Choose an attempt: an integer attempt placement (1..N) or a turn id.
    #[arg(long)]
    attempt: Option<String>,
}

#[derive(Debug, Args)]
struct TaskWatchArgs {
    task_id: String,

    /// Poll interval (seconds).
    #[arg(long, default_value_t = 3)]
    poll: u64,

    /// Exit as soon as the task reaches a terminal state.
    #[arg(long, default_value_t = true)]
    exit_on_done: bool,

    /// Also print new assistant messages as they appear.
    #[arg(long, default_value_t = true)]
    stream_messages: bool,

    /// Show attempt statuses (best-of-N).
    #[arg(long, default_value_t = false)]
    attempts: bool,

    /// Emit structured progress events as JSON Lines (one JSON object per line).
    #[arg(long, value_enum, default_value_t)]
    events: EventsFormat,
}

#[derive(Debug, Args)]
struct TaskApplyArgs {
    task_id: String,

    /// Choose an attempt: an integer attempt placement (1..N) or a turn id.
    #[arg(long)]
    attempt: Option<String>,

    /// Only validate applicability; do not modify the working tree.
    #[arg(long, default_value_t = false)]
    preflight: bool,

    /// Create a PR locally after applying (uses `gh` if installed).
    #[arg(long, default_value_t = false)]
    create_pr: bool,

    /// Branch name to use when creating a PR.
    #[arg(long)]
    pr_branch: Option<String>,

    /// Apply inside a dedicated git worktree (recommended for isolation).
    #[arg(long, default_value_t = false)]
    worktree: bool,

    /// Explicit worktree path to use/create. Implies --worktree.
    #[arg(long)]
    worktree_path: Option<PathBuf>,

    /// Root directory where worktrees are created when --worktree is set.
    /// Defaults to $CODEX_HOME/worktrees.
    #[arg(long)]
    worktree_dir: Option<PathBuf>,

    /// Base ref to checkout when creating the worktree.
    #[arg(long)]
    worktree_ref: Option<String>,

    /// If the worktree already exists, remove/recreate it before applying.
    #[arg(long, default_value_t = false)]
    worktree_clean: bool,
}

#[derive(Debug, Args)]
struct TaskPrsArgs {
    task_id: String,
}

#[derive(Debug, Args)]
struct RequestCmd {
    /// HTTP method.
    #[arg(long, default_value = "GET")]
    method: String,

    /// Path starting with /, e.g. /wham/environments
    path: String,

    /// JSON body (string) or '-' for stdin.
    #[arg(long)]
    body: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let session = Session::load(cli.base_url.clone(), cli.codex_home.clone()).await?;

    match cli.command {
        Command::Auth(cmd) => {
            tasks::cmd_auth(&session, cmd.show_token, cli.output).await?;
        }
        Command::Env(cmd) => {
            env_api::cmd_env(&session, cmd, cli.output).await?;
        }
        Command::Task(cmd) => {
            tasks::cmd_task(&session, cmd, cli.output).await?;
        }
        Command::Usage => {
            tasks::cmd_usage(&session, cli.output).await?;
        }
        Command::Requirements => {
            tasks::cmd_requirements(&session, cli.output).await?;
        }
        Command::Request(cmd) => {
            tasks::cmd_request(&session, cmd.method, cmd.path, cmd.body, cli.output).await?;
        }
        Command::Tui(args) => {
            tui::run_tui(&session, args).await?;
        }
    }

    Ok(())
}
