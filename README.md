# cloudex

A richer Rust CLI for **Codex Cloud tasks** that talks directly to the **ChatGPT/Codex backend API** using the same saved ChatGPT login that `codex` uses (via `CODEX_HOME`).

This binary is meant to complement (not replace) `codex cloud`:

- Environments: list + repo-aware detection, plus best-effort create/delete helpers
- Tasks: create, list, show, diff, watch progress, apply (preflight/apply), pick sibling attempts (best-of-N)
- PR helpers: after apply, create a GitHub PR locally (branch/commit/push) using `gh` if installed
- Extra: show rate limits / credits and fetch backend-managed requirements file

> Note: environment **create/delete** are not part of the public docs; they are implemented as **best-effort** requests against common backend paths. If your account/workspace does not allow these calls, the CLI will return the backend error payload.

## Build

From this crate directory:

```bash
cargo build
```

Or from anywhere:

```bash
cargo build --manifest-path /path/to/cloudex/Cargo.toml
```

## Standalone extraction

This crate is intentionally standalone and does not rely on the root `codex-rs` workspace.
Internal Codex dependencies are pulled from `https://github.com/openai/codex.git`, so you can move
`cloudex/` into its own repository and keep building it without the monorepo workspace.

When kept inside this monorepo, root workspace commands (`cargo check --workspace`, `cargo test --workspace`)
do not include this crate. Run checks explicitly with `--manifest-path /path/to/cloudex/Cargo.toml`.

## Authentication

This tool reuses the same credentials as Codex:

- Run `codex login` once.
- Optionally set `CODEX_HOME` (defaults to `~/.codex`).

## Quick examples

```bash
# list environments
cloudex env list

# detect environment from current git remote
cloudex env detect

# create a task (best-of-3 = 3 concurrent agent attempts)
cloudex task create --agents 3 "Fix flaky test in CI"

# watch progress and stream new assistant messages
cloudex task watch <TASK_ID>

# watch progress as JSONL events (for CI/scripting)
cloudex task watch <TASK_ID> --events jsonl

# show full details + attempts + diff
cloudex task show <TASK_ID> --attempts --diff

# print the diff for attempt placement 2
cloudex task diff <TASK_ID> --attempt 2

# preflight apply (no changes made)
cloudex task apply <TASK_ID> --preflight

# apply and then create a PR locally via gh
cloudex task apply <TASK_ID> --create-pr

# apply into an isolated git worktree (recommended)
cloudex task apply <TASK_ID> --worktree

# apply into an isolated worktree and create a PR from that worktree
cloudex task apply <TASK_ID> --worktree --create-pr

# one-shot: create → watch → apply → PR
cloudex task run --agents 2 --apply --create-pr "Add a healthcheck endpoint"

# one-shot with JSONL events + isolated worktree apply
cloudex task run --agents 2 --apply --worktree --events jsonl "Add a healthcheck endpoint"

# launch interactive TUI
cloudex tui

# launch TUI filtered to an environment
cloudex tui --env "prod" 
```

## Interactive TUI

Launch an interactive terminal UI that supports:

- browsing tasks (auto-refresh)
- viewing prompt / messages / diff
- best-of-N attempt switching
- creating new tasks
- preflight + apply locally (default: isolated worktree)
- optional PR creation via `gh`

Start it with:

```bash
cloudex tui
```

Key bindings (high level):

- Global
  - `q` / `Esc`: quit
  - `r`: refresh list
  - `o`: choose environment filter
  - `n`: new task
  - `Enter`: open details for selected task
  - `?`: toggle help
- Task details
  - `←` / `→`: switch Prompt / Messages / Diff
  - `Tab` / `Shift+Tab` or `[` / `]`: switch attempts
  - `j/k` or `↑/↓`: scroll
  - `a`: open apply modal (preflight + apply)
- Apply modal
  - `w`: toggle worktree mode
  - `c`: toggle create PR
  - `p`: re-run preflight
  - `y`: apply
  - `Esc`: close

CLI flags for the TUI:

- `--env <ID|LABEL>`: initial environment filter
- `--refresh <SECONDS>`: task list refresh interval (default: 5)
- `--poll <SECONDS>`: selected task details poll interval (default: 3)
- `--limit <N>`: number of tasks shown (default: 20)
- `--worktree-dir <PATH>`: root directory where worktrees are created from the apply modal

## Output formats

Add `--output json` to emit machine-readable output.

## JSONL progress events

Both `task watch` and `task run` can emit **structured progress events** as JSON Lines:

- `--events jsonl`

When enabled:

- Each progress update is a single JSON object per line on **stdout**.
- Human-oriented status messages are suppressed.
- External command output (git/gh) is routed to **stderr** so stdout stays JSONL-friendly.

Event types include (best-effort):

- `created` (run only)
- `status`
- `message` (when `--stream-messages` is enabled)
- `attempts` (when `--attempts` is enabled)
- `done`
- `apply_result` / `pr_created` / `run_complete` (run only)

## Worktree-based apply

`task apply` and `task run` support applying changes into an isolated **git worktree**:

- `--worktree` (create/reuse a deterministic worktree under `$CODEX_HOME/worktrees/...`)
- `--worktree-path <PATH>` (use an explicit worktree dir)
- `--worktree-dir <PATH>` (override the worktrees root)
- `--worktree-ref <REF>` (base ref to check out)
- `--worktree-clean` (remove/recreate the worktree before applying)

This is helpful when you want:

- a clean, repeatable apply target
- to avoid touching your main working tree
- to create a PR from the isolated worktree
