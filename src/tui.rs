use crate::TuiArgs;
use crate::auth::{Session, task_url};
use crate::env_api;
use crate::pr;
use crate::worktree;

use anyhow::Context;
use chrono::{DateTime, Utc};
use codex_cloud_tasks_client::{AttemptStatus, TaskId, TaskStatus, TaskSummary};
use crossterm::ExecutableCommand;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::prelude::Frame;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Tabs, Wrap};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio_stream::StreamExt;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use codex_cloud_tasks_client::{ApplyOutcome, ApplyStatus, CloudBackend, DiffSummary};
use codex_tui::{ComposerAction, ComposerInput};

/// Launch the interactive TUI.
pub async fn run_tui(session: &Session, args: TuiArgs) -> anyhow::Result<()> {
    let list_refresh = Duration::from_secs(args.refresh.max(1));
    let detail_refresh = Duration::from_secs(args.poll.max(1));
    let worktree_dir = args
        .worktree_dir
        .clone()
        .unwrap_or_else(|| session.codex_home.join("worktrees"));

    // Resolve initial env filter, if provided.
    let initial_env_filter = if let Some(sel) = args.env.as_deref() {
        match env_api::resolve_environment_id(session, Some(sel), None).await {
            Ok(id) => Some(id),
            Err(e) => {
                eprintln!("Warning: failed to resolve --env '{sel}': {e}");
                None
            }
        }
    } else {
        None
    };

    // Backend client.
    let backend = session.cloud_client()?;
    let backend: Arc<dyn CloudBackend> = Arc::new(backend);

    // Terminal setup.
    let mut stdout = std::io::stdout();
    enable_raw_mode()?;
    stdout.execute(EnterAlternateScreen)?;
    let backend_ui = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend_ui)?;
    terminal.clear()?;
    let _term_guard = TerminalGuard::new();

    // App state.
    let mut app = App::new(
        session.base_url.clone(),
        backend.clone(),
        args.limit.clamp(1, 50),
        list_refresh,
        detail_refresh,
        worktree_dir,
    );
    app.env_filter = initial_env_filter;
    app.status = "Loading tasks…".to_string();

    let (tx, mut rx) = unbounded_channel::<AppEvent>();

    // Kick initial loads.
    spawn_load_envs(session.clone(), tx.clone());
    app.refresh_tasks(tx.clone());

    // Event stream.
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(120));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut needs_redraw = true;

    loop {
        if needs_redraw {
            terminal.draw(|f| draw(f, &mut app))?;
            needs_redraw = false;
        }

        tokio::select! {
            _ = tick.tick() => {
                app.spinner_tick();
                if app.auto_refresh_due() {
                    app.refresh_tasks(tx.clone());
                    needs_redraw = true;
                }
                if app.details_refresh_due() {
                    app.refresh_details(tx.clone());
                    needs_redraw = true;
                }
                if app.flush_paste_burst_if_due() {
                    needs_redraw = true;
                }
            }
            maybe_evt = events.next() => {
                if let Some(Ok(evt)) = maybe_evt {
                    match evt {
                        Event::Key(key) if key.kind == KeyEventKind::Press => {
                            if handle_key(&mut app, key, tx.clone()).await? {
                                break;
                            }
                            needs_redraw = true;
                        }
                        Event::Paste(p) => {
                            if app.handle_paste(p) {
                                needs_redraw = true;
                            }
                        }
                        Event::Resize(_, _) => {
                            needs_redraw = true;
                        }
                        _ => {}
                    }
                }
            }
            maybe_msg = rx.recv() => {
                if let Some(msg) = maybe_msg {
                    app.on_app_event(msg, tx.clone());
                    needs_redraw = true;
                }
            }
        }
    }

    // terminal guard will restore terminal.
    Ok(())
}

/// Terminal state restoration guard.
struct TerminalGuard;

impl TerminalGuard {
    fn new() -> Self {
        Self
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = std::io::stdout();
        let _ = stdout.execute(LeaveAlternateScreen);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EnvModalTarget {
    Filter,
    NewTask,
}

#[derive(Clone, Debug)]
struct EnvModalState {
    query: String,
    selected: usize,
    target: EnvModalTarget,
}

#[derive(Clone, Debug, Default)]
struct AttemptsModalState {
    selected: usize, // 0..3 means 1..4
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DetailTab {
    Prompt,
    Messages,
    Diff,
}

#[derive(Clone, Debug, Default)]
struct AttemptInfo {
    placement: Option<i64>,
    turn_id: Option<String>,
    status: AttemptStatus,
    prompt: Option<String>,
    messages: Vec<String>,
    diff: Option<String>,
}

impl AttemptInfo {
    fn label(&self) -> String {
        if let Some(p) = self.placement {
            format!("{p}")
        } else if let Some(t) = &self.turn_id {
            t.clone()
        } else {
            "?".to_string()
        }
    }
}

#[derive(Clone, Debug)]
struct DetailsState {
    task_id: TaskId,
    title: String,
    status: TaskStatus,
    updated_at: DateTime<Utc>,
    env_label: Option<String>,
    env_id: Option<String>,
    summary: DiffSummary,
    attempt_total_hint: Option<usize>,

    tab: DetailTab,
    attempts: Vec<AttemptInfo>,
    selected_attempt: usize,

    scroll: ScrollableText,
    last_refresh: Instant,
    refresh_interval: Duration,
    loading: bool,
}

impl DetailsState {
    fn new(task: &TaskSummary, refresh_interval: Duration) -> Self {
        let mut scroll = ScrollableText::new();
        scroll.set_content(vec!["Loading…".to_string()]);
        Self {
            task_id: task.id.clone(),
            title: task.title.clone(),
            status: task.status.clone(),
            updated_at: task.updated_at,
            env_label: task.environment_label.clone(),
            env_id: task.environment_id.clone(),
            summary: task.summary.clone(),
            attempt_total_hint: task.attempt_total,
            tab: DetailTab::Prompt,
            attempts: vec![AttemptInfo::default()],
            selected_attempt: 0,
            scroll,
            last_refresh: Instant::now(),
            refresh_interval,
            loading: true,
        }
    }

    fn current_attempt(&self) -> Option<&AttemptInfo> {
        self.attempts.get(self.selected_attempt)
    }

    fn attempt_display_total(&self) -> usize {
        self.attempt_total_hint
            .unwrap_or_else(|| self.attempts.len().max(1))
            .max(1)
    }

    fn set_tab(&mut self, tab: DetailTab) {
        self.tab = tab;
        self.apply_selection_to_scroll();
    }

    fn step_attempt(&mut self, delta: isize) {
        let total = self.attempts.len().max(1);
        let total_isize = total as isize;
        let cur = self.selected_attempt as isize;
        let mut next = cur + delta;
        next = ((next % total_isize) + total_isize) % total_isize;
        self.selected_attempt = next as usize;
        self.apply_selection_to_scroll();
    }

    fn select_attempt_idx(&mut self, idx: usize) {
        if idx < self.attempts.len() {
            self.selected_attempt = idx;
            self.apply_selection_to_scroll();
        }
    }

    fn apply_selection_to_scroll(&mut self) {
        let Some(a) = self.current_attempt() else {
            self.scroll.set_content(vec!["(no data)".to_string()]);
            self.scroll.to_top();
            return;
        };
        let lines: Vec<String> = match self.tab {
            DetailTab::Prompt => {
                let p = a.prompt.as_deref().unwrap_or("(prompt unavailable)");
                p.lines().map(|s| s.to_string()).collect()
            }
            DetailTab::Messages => {
                if a.messages.is_empty() {
                    vec!["(no messages yet)".to_string()]
                } else {
                    let mut out = Vec::new();
                    for (i, m) in a.messages.iter().enumerate() {
                        out.push(format!("[{i}] {m}"));
                    }
                    out
                }
            }
            DetailTab::Diff => match a.diff.as_deref() {
                Some(d) if !d.trim().is_empty() => d.lines().map(|s| s.to_string()).collect(),
                _ => vec!["(no diff)".to_string()],
            },
        };
        self.scroll.set_content(lines);
        self.scroll.to_top();
    }
}

struct NewTaskState {
    composer: ComposerInput,
    env_id: Option<String>,
    env_label: Option<String>,
    attempts: usize,
    qa_mode: bool,
    git_ref: String,
    submitting: bool,
}

impl NewTaskState {
    fn new(
        default_env_id: Option<String>,
        default_env_label: Option<String>,
        git_ref: String,
    ) -> Self {
        let mut composer = ComposerInput::new();
        composer.set_hint_items(vec![
            ("⏎", "submit"),
            ("Shift+⏎", "newline"),
            ("⌃O", "env"),
            ("⌃N", "agents"),
            ("⌃Q", "qa"),
            ("Esc", "cancel"),
        ]);
        Self {
            composer,
            env_id: default_env_id,
            env_label: default_env_label,
            attempts: 1,
            qa_mode: false,
            git_ref,
            submitting: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ApplyStage {
    PreflightRunning,
    PreflightDone,
    Applying,
    Done,
}

#[derive(Clone, Debug)]
struct ApplyModalState {
    task_id: TaskId,
    title: String,
    attempt_label: String,
    diff: String,
    use_worktree: bool,
    worktree_path: Option<PathBuf>,
    create_pr: bool,
    pr_branch: String,
    stage: ApplyStage,
    preflight: Option<ApplyOutcome>,
    apply: Option<ApplyOutcome>,
    pr: Option<pr::PrCreateResult>,
    error: Option<String>,
}

impl ApplyModalState {
    fn new(
        task_id: TaskId,
        title: String,
        attempt_label: String,
        diff: String,
        default_worktree: bool,
    ) -> Self {
        let pr_branch = format!("codex/task_{}", task_id.0);
        Self {
            task_id,
            title,
            attempt_label,
            diff,
            use_worktree: default_worktree,
            worktree_path: None,
            create_pr: false,
            pr_branch,
            stage: ApplyStage::PreflightRunning,
            preflight: None,
            apply: None,
            pr: None,
            error: None,
        }
    }
}

#[derive(Clone, Debug)]
struct TaskDetailsPayload {
    summary: TaskSummary,
    attempts: Vec<AttemptInfo>,
}

#[derive(Debug)]
enum AppEvent {
    TasksLoaded {
        generation: u64,
        env: Option<String>,
        result: Result<Vec<TaskSummary>, String>,
    },
    EnvsLoaded(Result<Vec<env_api::Environment>, String>),
    NewTaskCreated(Result<codex_cloud_tasks_client::CreatedTask, String>),
    DetailsLoaded {
        task_id: TaskId,
        result: Result<TaskDetailsPayload, String>,
    },
    ApplyPreflightDone {
        task_id: TaskId,
        result: Result<(ApplyOutcome, Option<PathBuf>), String>,
    },
    ApplyDone {
        task_id: TaskId,
        result: Result<(ApplyOutcome, Option<PathBuf>), String>,
    },
    PrDone {
        task_id: TaskId,
        result: Result<pr::PrCreateResult, String>,
    },
}

struct App {
    backend: Arc<dyn CloudBackend>,
    base_url: String,

    // List view.
    tasks: Vec<TaskSummary>,
    selected: usize,
    list_generation: u64,
    list_refresh_inflight: bool,
    last_list_refresh: Instant,
    list_refresh_interval: Duration,
    detail_refresh_interval: Duration,
    limit: i64,

    // Environment filter.
    env_filter: Option<String>,
    env_filter_label: Option<String>,

    // Environments cache.
    envs: Vec<env_api::Environment>,
    env_loading: bool,
    env_error: Option<String>,

    // Overlays.
    env_modal: Option<EnvModalState>,
    attempts_modal: Option<AttemptsModalState>,
    new_task: Option<NewTaskState>,
    details: Option<DetailsState>,
    apply_modal: Option<ApplyModalState>,
    show_help: bool,

    // Status.
    status: String,
    spinner: usize,

    // Config.
    worktree_dir: PathBuf,
}

impl App {
    fn new(
        base_url: String,
        backend: Arc<dyn CloudBackend>,
        limit: i64,
        list_refresh_interval: Duration,
        detail_refresh_interval: Duration,
        worktree_dir: PathBuf,
    ) -> Self {
        Self {
            base_url,
            backend,
            tasks: Vec::new(),
            selected: 0,
            list_generation: 0,
            list_refresh_inflight: false,
            last_list_refresh: Instant::now(),
            list_refresh_interval,
            detail_refresh_interval,
            limit,
            env_filter: None,
            env_filter_label: None,
            envs: Vec::new(),
            env_loading: false,
            env_error: None,
            env_modal: None,
            attempts_modal: None,
            new_task: None,
            details: None,
            apply_modal: None,
            show_help: false,
            status: String::new(),
            spinner: 0,
            worktree_dir,
        }
    }

    fn spinner_tick(&mut self) {
        self.spinner = self.spinner.wrapping_add(1);
    }

    fn spinner_char(&self) -> char {
        match self.spinner % 4 {
            0 => '|',
            1 => '/',
            2 => '-',
            _ => '\\',
        }
    }

    fn selected_task(&self) -> Option<&TaskSummary> {
        self.tasks.get(self.selected)
    }

    fn auto_refresh_due(&self) -> bool {
        if self.list_refresh_inflight {
            return false;
        }
        Instant::now().duration_since(self.last_list_refresh) >= self.list_refresh_interval
    }

    fn details_refresh_due(&self) -> bool {
        let Some(d) = &self.details else {
            return false;
        };
        if d.loading {
            return false;
        }
        Instant::now().duration_since(d.last_refresh) >= d.refresh_interval
    }

    fn flush_paste_burst_if_due(&mut self) -> bool {
        if let Some(nt) = self.new_task.as_mut() {
            return nt.composer.flush_paste_burst_if_due();
        }
        false
    }

    fn handle_paste(&mut self, pasted: String) -> bool {
        if let Some(nt) = self.new_task.as_mut() {
            return nt.composer.handle_paste(pasted);
        }
        false
    }

    fn refresh_tasks(&mut self, tx: UnboundedSender<AppEvent>) {
        if self.list_refresh_inflight {
            return;
        }
        self.list_refresh_inflight = true;
        self.last_list_refresh = Instant::now();
        self.list_generation = self.list_generation.saturating_add(1);
        let generation = self.list_generation;
        let backend = self.backend.clone();
        let env = self.env_filter.clone();
        let limit = self.limit;
        tokio::spawn(async move {
            let res = backend
                .list_tasks(env.as_deref(), Some(limit), None)
                .await
                .map(|page| {
                    page.tasks
                        .into_iter()
                        .filter(|t| !t.is_review)
                        .collect::<Vec<_>>()
                })
                .map_err(|e| format!("{e}"));
            let _ = tx.send(AppEvent::TasksLoaded {
                generation,
                env,
                result: res,
            });
        });
    }

    fn refresh_details(&mut self, tx: UnboundedSender<AppEvent>) {
        let Some(task) = self
            .details
            .as_ref()
            .map(|d| d.task_id.clone())
            .or_else(|| self.selected_task().map(|t| t.id.clone()))
        else {
            return;
        };
        // Mark details loading if overlay is open.
        if let Some(d) = self.details.as_mut() {
            d.loading = true;
            d.last_refresh = Instant::now();
        }
        let backend = self.backend.clone();
        tokio::spawn(async move {
            let res = fetch_task_details(&*backend, task.clone())
                .await
                .map_err(|e| format!("{e}"));
            let _ = tx.send(AppEvent::DetailsLoaded {
                task_id: task,
                result: res,
            });
        });
    }

    fn on_app_event(&mut self, evt: AppEvent, tx: UnboundedSender<AppEvent>) {
        match evt {
            AppEvent::TasksLoaded {
                generation,
                env,
                result,
            } => {
                if generation != self.list_generation {
                    return;
                }
                self.list_refresh_inflight = false;
                match result {
                    Ok(mut tasks) => {
                        // Keep selection on the same task id if possible.
                        let prev_id = self.selected_task().map(|t| t.id.0.clone());
                        tasks.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
                        self.tasks = tasks;

                        if let Some(prev) = prev_id {
                            if let Some(idx) = self.tasks.iter().position(|t| t.id.0 == prev) {
                                self.selected = idx;
                            } else {
                                self.selected =
                                    self.selected.min(self.tasks.len().saturating_sub(1));
                            }
                        } else {
                            self.selected = self.selected.min(self.tasks.len().saturating_sub(1));
                        }

                        self.env_filter_label = env.as_deref().and_then(|eid| {
                            self.envs
                                .iter()
                                .find(|e| e.id == eid)
                                .and_then(|e| e.label.clone())
                        });
                        self.status = format!("{} task(s) loaded", self.tasks.len());
                    }
                    Err(e) => {
                        self.status = format!("Refresh failed: {e}");
                    }
                }
            }
            AppEvent::EnvsLoaded(res) => {
                self.env_loading = false;
                match res {
                    Ok(mut envs) => {
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
                        self.envs = envs;
                        self.env_error = None;
                    }
                    Err(e) => {
                        self.env_error = Some(e);
                    }
                }
            }
            AppEvent::NewTaskCreated(res) => {
                if let Some(nt) = self.new_task.as_mut() {
                    nt.submitting = false;
                }
                match res {
                    Ok(created) => {
                        self.status = format!("Created task {}", created.id.0);
                        // Refresh list and open details.
                        self.refresh_tasks(tx.clone());
                        // Open a placeholder details overlay.
                        let placeholder = TaskSummary {
                            id: created.id.clone(),
                            title: "(loading…)".to_string(),
                            status: TaskStatus::Pending,
                            updated_at: Utc::now(),
                            environment_id: None,
                            environment_label: None,
                            summary: DiffSummary::default(),
                            is_review: false,
                            attempt_total: None,
                        };
                        self.details = Some(DetailsState::new(
                            &placeholder,
                            self.detail_refresh_interval,
                        ));
                        self.new_task = None;
                        self.refresh_details(tx);
                    }
                    Err(e) => {
                        self.status = format!("Task create failed: {e}");
                    }
                }
            }
            AppEvent::DetailsLoaded { task_id, result } => {
                // Ignore stale detail responses for a task that is no longer open.
                let should_handle = self
                    .details
                    .as_ref()
                    .is_some_and(|d| d.task_id.0 == task_id.0);
                if !should_handle {
                    return;
                }
                if let Some(d) = self.details.as_mut() {
                    d.loading = false;
                }
                match result {
                    Ok(payload) => {
                        let sum = payload.summary;
                        if sum.id.0 != task_id.0 {
                            return;
                        }
                        if let Some(d) = self.details.as_mut() {
                            d.title = sum.title.clone();
                            d.status = sum.status.clone();
                            d.updated_at = sum.updated_at;
                            d.env_label = sum.environment_label.clone();
                            d.env_id = sum.environment_id.clone();
                            d.summary = sum.summary;
                            d.attempt_total_hint = sum.attempt_total;
                            d.attempts = payload.attempts;
                            d.selected_attempt =
                                d.selected_attempt.min(d.attempts.len().saturating_sub(1));
                            d.apply_selection_to_scroll();
                            d.last_refresh = Instant::now();
                        }
                    }
                    Err(e) => {
                        self.status = format!("Failed to load task details: {e}");
                    }
                }
            }
            AppEvent::ApplyPreflightDone { task_id, result } => {
                if let Some(m) = self.apply_modal.as_mut() {
                    if m.task_id.0 != task_id.0 {
                        return;
                    }
                    match result {
                        Ok((outcome, wt)) => {
                            m.preflight = Some(outcome);
                            m.worktree_path = wt;
                            m.stage = ApplyStage::PreflightDone;
                            m.error = None;
                        }
                        Err(e) => {
                            m.error = Some(e);
                            m.stage = ApplyStage::PreflightDone;
                        }
                    }
                }
            }
            AppEvent::ApplyDone { task_id, result } => {
                if let Some(m) = self.apply_modal.as_mut() {
                    if m.task_id.0 != task_id.0 {
                        return;
                    }
                    match result {
                        Ok((outcome, wt)) => {
                            m.apply = Some(outcome);
                            m.worktree_path = wt;
                            m.stage = ApplyStage::Done;
                            m.error = None;

                            let should_pr = m.create_pr
                                && m.apply.as_ref().is_some_and(|o| {
                                    matches!(&o.status, ApplyStatus::Success | ApplyStatus::Partial)
                                });
                            if should_pr {
                                self.spawn_create_pr(tx);
                            }
                        }
                        Err(e) => {
                            m.error = Some(e);
                            m.stage = ApplyStage::Done;
                        }
                    }
                }
            }
            AppEvent::PrDone { task_id, result } => {
                if let Some(m) = self.apply_modal.as_mut() {
                    if m.task_id.0 != task_id.0 {
                        return;
                    }
                    match result {
                        Ok(r) => {
                            m.pr = Some(r);
                        }
                        Err(e) => {
                            m.error = Some(format!("PR create failed: {e}"));
                        }
                    }
                }
            }
        }
    }

    fn open_env_modal(&mut self, target: EnvModalTarget) {
        self.env_modal = Some(EnvModalState {
            query: String::new(),
            selected: 0,
            target,
        });
        if self.envs.is_empty() && !self.env_loading {
            self.env_loading = true;
        }
    }

    fn open_new_task(&mut self) {
        let default_env_id = self.env_filter.clone().or_else(|| {
            self.envs
                .iter()
                .find(|e| e.is_pinned.unwrap_or(false))
                .map(|e| e.id.clone())
                .or_else(|| self.envs.first().map(|e| e.id.clone()))
        });
        let default_env_label = default_env_id.as_deref().and_then(|eid| {
            self.envs
                .iter()
                .find(|e| e.id == eid)
                .and_then(|e| e.label.clone())
        });
        let git_ref = current_git_ref().unwrap_or_else(|_| "main".to_string());
        self.new_task = Some(NewTaskState::new(
            default_env_id,
            default_env_label,
            git_ref,
        ));
    }

    fn open_details_for_selected(&mut self) {
        let Some(t) = self.selected_task().cloned() else {
            self.status = "No task selected".to_string();
            return;
        };
        // Close other overlays.
        self.new_task = None;
        self.apply_modal = None;
        self.env_modal = None;
        self.attempts_modal = None;

        self.details = Some(DetailsState::new(&t, self.detail_refresh_interval));
    }

    fn spawn_create_task(&mut self, tx: UnboundedSender<AppEvent>, prompt: String) {
        let Some(nt) = self.new_task.as_mut() else {
            return;
        };
        let Some(env_id) = nt.env_id.clone() else {
            self.status = "No environment selected".to_string();
            return;
        };
        let git_ref = nt.git_ref.clone();
        let qa = nt.qa_mode;
        let attempts = nt.attempts;
        let backend = self.backend.clone();
        nt.submitting = true;
        tokio::spawn(async move {
            let res = backend
                .create_task(&env_id, &prompt, &git_ref, qa, attempts)
                .await
                .map_err(|e| format!("{e}"));
            let _ = tx.send(AppEvent::NewTaskCreated(res));
        });
    }

    fn spawn_apply_preflight(&mut self, tx: UnboundedSender<AppEvent>) {
        let Some(m) = self.apply_modal.as_mut() else {
            return;
        };
        m.stage = ApplyStage::PreflightRunning;
        m.preflight = None;
        m.apply = None;
        m.pr = None;
        m.error = None;

        let task_id = m.task_id.clone();
        let task_id_for_worker = task_id.0.clone();
        let diff = m.diff.clone();
        let use_worktree = m.use_worktree;
        let worktree_dir = self.worktree_dir.clone();

        tokio::spawn(async move {
            let res = tokio::task::spawn_blocking(
                move || -> anyhow::Result<(ApplyOutcome, Option<PathBuf>)> {
                    if use_worktree {
                        let cwd =
                            std::env::current_dir().context("failed to read current directory")?;
                        let repo_root = worktree::resolve_repo_root(&cwd)?;
                        let base_ref = current_git_ref().unwrap_or_else(|_| "main".to_string());
                        let wt_path = worktree::worktree_path_in(
                            &worktree_dir,
                            &repo_root,
                            &task_id_for_worker,
                            None,
                        );
                        let path =
                            worktree::ensure_worktree(&repo_root, &wt_path, &base_ref, false)?;
                        let outcome = apply_diff_in_dir(&task_id_for_worker, &diff, &path, true)?;
                        Ok((outcome, Some(path)))
                    } else {
                        let cwd =
                            std::env::current_dir().context("failed to read current directory")?;
                        let outcome = apply_diff_in_dir(&task_id_for_worker, &diff, &cwd, true)?;
                        Ok((outcome, None))
                    }
                },
            )
            .await
            .map_err(|e| format!("apply preflight join error: {e}"))
            .and_then(|r| r.map_err(|e| format!("{e}")));

            let _ = tx.send(AppEvent::ApplyPreflightDone {
                task_id,
                result: res,
            });
        });
    }

    fn spawn_apply(&mut self, tx: UnboundedSender<AppEvent>) {
        let Some(m) = self.apply_modal.as_mut() else {
            return;
        };
        m.stage = ApplyStage::Applying;
        m.apply = None;
        m.pr = None;
        m.error = None;

        let task_id = m.task_id.clone();
        let task_id_for_worker = task_id.0.clone();
        let diff = m.diff.clone();
        let use_worktree = m.use_worktree;
        let worktree_dir = self.worktree_dir.clone();

        tokio::spawn(async move {
            let res = tokio::task::spawn_blocking(
                move || -> anyhow::Result<(ApplyOutcome, Option<PathBuf>)> {
                    if use_worktree {
                        let cwd =
                            std::env::current_dir().context("failed to read current directory")?;
                        let repo_root = worktree::resolve_repo_root(&cwd)?;
                        let base_ref = current_git_ref().unwrap_or_else(|_| "main".to_string());
                        let wt_path = worktree::worktree_path_in(
                            &worktree_dir,
                            &repo_root,
                            &task_id_for_worker,
                            None,
                        );
                        let path =
                            worktree::ensure_worktree(&repo_root, &wt_path, &base_ref, false)?;
                        let outcome = apply_diff_in_dir(&task_id_for_worker, &diff, &path, false)?;
                        Ok((outcome, Some(path)))
                    } else {
                        let cwd =
                            std::env::current_dir().context("failed to read current directory")?;
                        let outcome = apply_diff_in_dir(&task_id_for_worker, &diff, &cwd, false)?;
                        Ok((outcome, None))
                    }
                },
            )
            .await
            .map_err(|e| format!("apply join error: {e}"))
            .and_then(|r| r.map_err(|e| format!("{e}")));

            let _ = tx.send(AppEvent::ApplyDone {
                task_id,
                result: res,
            });
        });
    }

    fn spawn_create_pr(&mut self, tx: UnboundedSender<AppEvent>) {
        let Some(m) = self.apply_modal.as_ref() else {
            return;
        };
        let dir = m
            .worktree_path
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir()));
        let task_id = m.task_id.clone();
        let task_id_for_worker = task_id.0.clone();
        let title = m.title.clone();
        let branch = m.pr_branch.clone();
        let url = task_url(&self.base_url, &task_id.0);

        tokio::spawn(async move {
            let res = tokio::task::spawn_blocking(move || {
                let plan = pr::CreatePrPlan {
                    branch: branch.clone(),
                    title: format!("Codex: {} ({})", title, task_id_for_worker),
                    body: Some(format!("Created from Codex cloud task: {url}")),
                    remote: "origin".to_string(),
                };
                pr::create_pr_from_dir_capture(&dir, plan)
            })
            .await
            .map_err(|e| format!("pr join error: {e}"))
            .and_then(|r| r.map_err(|e| format!("{e}")));
            let _ = tx.send(AppEvent::PrDone {
                task_id,
                result: res,
            });
        });
    }
}

async fn handle_key(
    app: &mut App,
    key: KeyEvent,
    tx: UnboundedSender<AppEvent>,
) -> anyhow::Result<bool> {
    // Global quit.
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
    {
        return Ok(true);
    }

    // Apply modal has highest priority.
    if let Some(m) = app.apply_modal.as_mut() {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
                // Allow closing only when not running.
                if matches!(m.stage, ApplyStage::PreflightRunning | ApplyStage::Applying) {
                    app.status = "Apply is running…".to_string();
                } else {
                    app.apply_modal = None;
                }
            }
            KeyCode::Char('w') | KeyCode::Char('W') => {
                if matches!(m.stage, ApplyStage::PreflightRunning | ApplyStage::Applying) {
                    app.status = "Finish the current apply/preflight first.".to_string();
                } else {
                    m.use_worktree = !m.use_worktree;
                    app.spawn_apply_preflight(tx);
                }
            }
            KeyCode::Char('c') | KeyCode::Char('C') => {
                if matches!(m.stage, ApplyStage::PreflightRunning | ApplyStage::Applying) {
                    app.status = "Finish the current apply/preflight first.".to_string();
                } else {
                    m.create_pr = !m.create_pr;
                }
            }
            KeyCode::Char('p') | KeyCode::Char('P') => {
                if matches!(m.stage, ApplyStage::PreflightRunning | ApplyStage::Applying) {
                    app.status = "Finish the current apply/preflight first.".to_string();
                } else {
                    app.spawn_apply_preflight(tx);
                }
            }
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if matches!(m.stage, ApplyStage::PreflightDone) {
                    app.spawn_apply(tx);
                }
            }
            _ => {}
        }
        return Ok(false);
    }

    // Environment modal.
    if let Some(em) = app.env_modal.as_mut() {
        match key.code {
            KeyCode::Esc => {
                app.env_modal = None;
                return Ok(false);
            }
            KeyCode::Backspace => {
                em.query.pop();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                em.selected = em.selected.saturating_add(1);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                em.selected = em.selected.saturating_sub(1);
            }
            KeyCode::Home => {
                em.selected = 0;
            }
            KeyCode::End => {
                em.selected = 9999;
            }
            KeyCode::Enter => {
                let options = env_modal_options(&app.envs, &em.query);
                let idx = em.selected.min(options.len().saturating_sub(1));
                if let Some(choice) = options.get(idx) {
                    match (&em.target, choice) {
                        (EnvModalTarget::Filter, None) => {
                            app.env_filter = None;
                            app.env_filter_label = None;
                            app.env_modal = None;
                            app.refresh_tasks(tx);
                        }
                        (EnvModalTarget::Filter, Some(env)) => {
                            app.env_filter = Some(env.id.clone());
                            app.env_filter_label = env.label.clone();
                            app.env_modal = None;
                            app.refresh_tasks(tx);
                        }
                        (EnvModalTarget::NewTask, None) => {
                            if let Some(nt) = app.new_task.as_mut() {
                                nt.env_id = None;
                                nt.env_label = None;
                            }
                            app.env_modal = None;
                        }
                        (EnvModalTarget::NewTask, Some(env)) => {
                            if let Some(nt) = app.new_task.as_mut() {
                                nt.env_id = Some(env.id.clone());
                                nt.env_label = env.label.clone();
                            }
                            app.env_modal = None;
                        }
                    }
                }
            }
            KeyCode::Char(ch)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                em.query.push(ch);
            }
            _ => {}
        }
        return Ok(false);
    }

    // Attempts modal.
    if let Some(am) = app.attempts_modal.as_mut() {
        match key.code {
            KeyCode::Esc => {
                app.attempts_modal = None;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                am.selected = (am.selected + 1).min(3);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                am.selected = am.selected.saturating_sub(1);
            }
            KeyCode::Char('1') | KeyCode::Char('2') | KeyCode::Char('3') | KeyCode::Char('4') => {
                let n = match key.code {
                    KeyCode::Char('1') => 1,
                    KeyCode::Char('2') => 2,
                    KeyCode::Char('3') => 3,
                    _ => 4,
                };
                if let Some(nt) = app.new_task.as_mut() {
                    nt.attempts = n;
                }
                app.attempts_modal = None;
            }
            KeyCode::Enter => {
                let n = am.selected + 1;
                if let Some(nt) = app.new_task.as_mut() {
                    nt.attempts = n;
                }
                app.attempts_modal = None;
            }
            _ => {}
        }
        return Ok(false);
    }

    // New task page.
    if let Some(nt) = app.new_task.as_mut() {
        match key.code {
            KeyCode::Esc => {
                app.new_task = None;
                return Ok(false);
            }
            _ => {}
        }

        // Ctrl+O env
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('o') | KeyCode::Char('O'))
        {
            app.open_env_modal(EnvModalTarget::NewTask);
            return Ok(false);
        }

        // Ctrl+N attempts
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('n') | KeyCode::Char('N'))
        {
            app.attempts_modal = Some(AttemptsModalState {
                selected: nt.attempts.saturating_sub(1).min(3),
            });
            return Ok(false);
        }

        // Ctrl+Q toggle qa
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('q') | KeyCode::Char('Q'))
        {
            nt.qa_mode = !nt.qa_mode;
            return Ok(false);
        }

        if nt.submitting {
            return Ok(false);
        }

        match nt.composer.input(key) {
            ComposerAction::Submitted(text) => {
                let text = text.trim().to_string();
                if text.is_empty() {
                    app.status = "Prompt is empty".to_string();
                } else {
                    app.spawn_create_task(tx, text);
                }
            }
            ComposerAction::None => {}
        }
        return Ok(false);
    }

    // Details overlay.
    if let Some(d) = app.details.as_mut() {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                app.details = None;
            }
            KeyCode::Left => {
                d.set_tab(match d.tab {
                    DetailTab::Prompt => DetailTab::Prompt,
                    DetailTab::Messages => DetailTab::Prompt,
                    DetailTab::Diff => DetailTab::Messages,
                });
            }
            KeyCode::Right => {
                d.set_tab(match d.tab {
                    DetailTab::Prompt => DetailTab::Messages,
                    DetailTab::Messages => DetailTab::Diff,
                    DetailTab::Diff => DetailTab::Diff,
                });
            }
            KeyCode::Tab => {
                d.step_attempt(1);
                let total = d.attempt_display_total();
                let cur = d.selected_attempt + 1;
                app.status = format!("Viewing attempt {cur} of {total}");
            }
            KeyCode::BackTab => {
                d.step_attempt(-1);
                let total = d.attempt_display_total();
                let cur = d.selected_attempt + 1;
                app.status = format!("Viewing attempt {cur} of {total}");
            }
            KeyCode::Char(']') | KeyCode::Char('}') => {
                d.step_attempt(1);
            }
            KeyCode::Char('[') | KeyCode::Char('{') => {
                d.step_attempt(-1);
            }
            KeyCode::Char('1') | KeyCode::Char('2') | KeyCode::Char('3') | KeyCode::Char('4') => {
                let idx = match key.code {
                    KeyCode::Char('1') => 0,
                    KeyCode::Char('2') => 1,
                    KeyCode::Char('3') => 2,
                    _ => 3,
                };
                d.select_attempt_idx(idx);
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
                app.refresh_details(tx);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                d.scroll.scroll_by(1);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                d.scroll.scroll_by(-1);
            }
            KeyCode::PageDown | KeyCode::Char(' ') => {
                let step = d.scroll.state.viewport_h.saturating_sub(1) as i16;
                d.scroll.page_by(step);
            }
            KeyCode::PageUp => {
                let step = d.scroll.state.viewport_h.saturating_sub(1) as i16;
                d.scroll.page_by(-step);
            }
            KeyCode::Home => {
                d.scroll.to_top();
            }
            KeyCode::End => {
                d.scroll.to_bottom();
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                if let Some(attempt) = d.current_attempt() {
                    if let Some(diff) = attempt.diff.as_deref() {
                        if diff.trim().is_empty() {
                            app.status = "No diff available for this attempt".to_string();
                        } else {
                            let attempt_label = attempt.label();
                            app.apply_modal = Some(ApplyModalState::new(
                                d.task_id.clone(),
                                d.title.clone(),
                                attempt_label,
                                diff.to_string(),
                                true,
                            ));
                            app.spawn_apply_preflight(tx);
                        }
                    } else {
                        app.status = "No diff available for this attempt".to_string();
                    }
                }
            }
            _ => {}
        }
        return Ok(false);
    }

    // Base list view.
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
        KeyCode::Down | KeyCode::Char('j') => {
            if !app.tasks.is_empty() {
                app.selected = (app.selected + 1).min(app.tasks.len().saturating_sub(1));
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.selected = app.selected.saturating_sub(1);
        }
        KeyCode::Char('r') | KeyCode::Char('R') => {
            app.refresh_tasks(tx);
        }
        KeyCode::Char('o') | KeyCode::Char('O') => {
            app.open_env_modal(EnvModalTarget::Filter);
        }
        KeyCode::Char('n') | KeyCode::Char('N') => {
            app.open_new_task();
        }
        KeyCode::Enter => {
            app.open_details_for_selected();
            app.refresh_details(tx);
        }
        KeyCode::Char('?') => {
            app.show_help = !app.show_help;
        }
        _ => {}
    }

    Ok(false)
}

fn draw(f: &mut Frame<'_>, app: &mut App) {
    let size = f.area();

    // Base layout.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(size);

    draw_header(f, app, chunks[0]);
    draw_body(f, app, chunks[1]);
    draw_footer(f, app, chunks[2]);

    if app.show_help {
        draw_help(f, size);
    }

    // Overlays.
    let spinner = app.spinner_char();
    let base_url = app.base_url.clone();
    let envs = app.envs.clone();
    if let Some(details) = app.details.as_mut() {
        draw_details_overlay(f, &base_url, spinner, details, size);
    }
    if let Some(nt) = app.new_task.as_mut() {
        draw_new_task_overlay(f, spinner, nt, size);
    }
    if let Some(em) = app.env_modal.as_mut() {
        draw_env_modal(f, &envs, em, size);
    }
    if let Some(am) = app.attempts_modal.as_mut() {
        draw_attempts_modal(f, am, size);
    }
    if let Some(m) = app.apply_modal.as_mut() {
        draw_apply_modal(f, spinner, m, size);
    }
}

fn draw_header(f: &mut Frame<'_>, app: &App, area: Rect) {
    let env = match (&app.env_filter, &app.env_filter_label) {
        (None, _) => "env: (all)".to_string(),
        (Some(id), Some(lbl)) => format!("env: {lbl} ({id})"),
        (Some(id), None) => format!("env: {id}"),
    };
    let left = format!("cloudex  •  {env}");
    let right = if app.list_refresh_inflight {
        format!("{} refreshing", app.spinner_char())
    } else {
        "".to_string()
    };
    let line = if right.is_empty() {
        left
    } else {
        // crude right-align: just include both.
        format!("{left}    {right}")
    };
    f.render_widget(Paragraph::new(line), area);
}

fn draw_footer(f: &mut Frame<'_>, app: &App, area: Rect) {
    let mut line = app.status.clone();
    if line.is_empty() {
        line = "q: quit • o: env • n: new • enter: details • r: refresh • ?: help".to_string();
    }
    f.render_widget(Paragraph::new(line), area);
}

fn draw_body(f: &mut Frame<'_>, app: &mut App, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);

    draw_task_list(f, app, cols[0]);
    draw_preview(f, app, cols[1]);
}

fn draw_task_list(f: &mut Frame<'_>, app: &mut App, area: Rect) {
    let items: Vec<ListItem> = app
        .tasks
        .iter()
        .map(|t| {
            let status = format!("{:?}", t.status).to_uppercase();
            let title = truncate(&t.title, 80);
            let env = t
                .environment_label
                .clone()
                .or(t.environment_id.clone())
                .unwrap_or_else(|| "".to_string());
            let rel = format_relative_time_now(t.updated_at);
            let diff = if t.summary.files_changed > 0 {
                format!(
                    "+{}/-{} • {} files",
                    t.summary.lines_added, t.summary.lines_removed, t.summary.files_changed
                )
            } else {
                "no diff".to_string()
            };
            ListItem::new(vec![
                Line::from(format!("[{status}] {title}")),
                Line::from(format!("{env}  •  {rel}  •  {diff}")),
            ])
        })
        .collect();

    let block = Block::default().borders(Borders::ALL).title("Tasks");
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ratatui::widgets::ListState::default();
    state.select(Some(app.selected));
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_preview(f: &mut Frame<'_>, app: &mut App, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title("Preview");
    let Some(t) = app.selected_task() else {
        f.render_widget(Paragraph::new("(no tasks)").block(block), area);
        return;
    };
    let url = task_url(&app.base_url, &t.id.0);
    let env = t
        .environment_label
        .clone()
        .or(t.environment_id.clone())
        .unwrap_or_else(|| "".to_string());
    let rel = format_relative_time_now(t.updated_at);
    let mut lines = vec![
        format!("id: {}", t.id.0),
        format!("status: {:?}", t.status),
        format!("updated: {rel}"),
        format!("env: {env}"),
        format!(
            "diff: files={} +{} -{}",
            t.summary.files_changed, t.summary.lines_added, t.summary.lines_removed
        ),
        String::new(),
        format!("url: {url}"),
        String::new(),
        "enter: open details".to_string(),
    ];
    if t.attempt_total.unwrap_or(1) > 1 {
        lines.push(format!(
            "attempts: best-of-{}",
            t.attempt_total.unwrap_or(1)
        ));
    }
    let text = lines.join("\n");
    f.render_widget(
        Paragraph::new(text).block(block).wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_help(f: &mut Frame<'_>, area: Rect) {
    let popup = centered_rect(80, 70, area);
    f.render_widget(Clear, popup);
    let block = Block::default().borders(Borders::ALL).title("Help");
    let text = "\
Global\n\
  q / Esc           Quit\n\
  r                Refresh task list\n\
  o                Select environment filter\n\
  n                New task\n\
  enter            Open task details\n\
\n\
Task details\n\
  ←/→              Switch Prompt / Messages / Diff\n\
  Tab/Shift+Tab     Cycle attempts\n\
  [ / ]            Cycle attempts\n\
  j/k or ↑/↓        Scroll\n\
  a                Preflight/apply diff (supports worktrees + PR)\n\
\n\
New task\n\
  Ctrl+O           Select environment\n\
  Ctrl+N           Set agents (best-of-N)\n\
  Ctrl+Q           Toggle QA mode\n\
  Esc              Cancel\n\
\n\
Apply modal\n\
  w                Toggle worktree mode\n\
  c                Toggle create PR\n\
  p                Re-run preflight\n\
  y                Apply\n\
  Esc              Close\n\
";
    f.render_widget(
        Paragraph::new(text).block(block).wrap(Wrap { trim: false }),
        popup,
    );
}

fn draw_details_overlay(
    f: &mut Frame<'_>,
    base_url: &str,
    spinner: char,
    d: &mut DetailsState,
    area: Rect,
) {
    let popup = centered_rect(95, 90, area);
    f.render_widget(Clear, popup);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(popup);

    // Header.
    let env = d
        .env_label
        .clone()
        .or(d.env_id.clone())
        .unwrap_or_else(|| "".to_string());
    let url = task_url(base_url, &d.task_id.0);
    let rel = format_relative_time_now(d.updated_at);
    let header = format!(
        "{}\n{:?}  •  {}  •  {}\n{}",
        truncate(&d.title, 120),
        d.status,
        env,
        rel,
        url
    );
    f.render_widget(
        Paragraph::new(header)
            .block(Block::default().borders(Borders::ALL).title("Task"))
            .wrap(Wrap { trim: false }),
        chunks[0],
    );

    // Tabs.
    let tab_titles = vec!["Prompt", "Messages", "Diff"]
        .into_iter()
        .map(|t| Line::from(t))
        .collect::<Vec<_>>();
    let idx = match d.tab {
        DetailTab::Prompt => 0,
        DetailTab::Messages => 1,
        DetailTab::Diff => 2,
    };
    let tabs = Tabs::new(tab_titles)
        .select(idx)
        .block(Block::default().borders(Borders::ALL))
        .highlight_style(Style::default().add_modifier(Modifier::BOLD));
    f.render_widget(tabs, chunks[1]);

    // Content.
    let content_area = chunks[2];
    d.scroll.set_width(content_area.width.saturating_sub(2));
    d.scroll.set_viewport(content_area.height.saturating_sub(2));
    let visible = d
        .scroll
        .wrapped_lines()
        .iter()
        .skip(d.scroll.state.scroll as usize)
        .take(d.scroll.state.viewport_h as usize)
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    let title = match d.tab {
        DetailTab::Prompt => "Prompt",
        DetailTab::Messages => "Messages",
        DetailTab::Diff => "Diff",
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    f.render_widget(
        Paragraph::new(visible)
            .block(block)
            .wrap(Wrap { trim: false }),
        content_area,
    );

    // Attempts footer.
    let total = d.attempt_display_total();
    let cur = d.selected_attempt + 1;
    let mut footer =
        format!("attempt {cur}/{total}  •  a: apply  •  tab: next attempt  •  q: close");
    if d.loading {
        footer.push_str(&format!("   {spinner} loading…"));
    }
    f.render_widget(Paragraph::new(footer), chunks[3]);
}

fn draw_new_task_overlay(f: &mut Frame<'_>, spinner: char, nt: &mut NewTaskState, area: Rect) {
    let popup = centered_rect(95, 90, area);
    f.render_widget(Clear, popup);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Min(3)])
        .split(popup);

    let env = match (&nt.env_id, &nt.env_label) {
        (Some(id), Some(lbl)) => format!("{lbl} ({id})"),
        (Some(id), None) => id.clone(),
        (None, _) => "(none)".to_string(),
    };
    let head = format!(
        "New task\n\
env: {env}\n\
agents: {}\n\
ref: {}\n\
qa_mode: {}\n\
",
        nt.attempts, nt.git_ref, nt.qa_mode
    );
    let mut head_block = Block::default().borders(Borders::ALL).title("Create task");
    if nt.submitting {
        head_block = head_block.title(format!("Create task   {spinner} submitting…"));
    }
    f.render_widget(
        Paragraph::new(head)
            .block(head_block)
            .wrap(Wrap { trim: false }),
        chunks[0],
    );

    // Composer
    let comp_area = chunks[1];
    nt.composer.render_ref(comp_area, f.buffer_mut());
    if let Some((x, y)) = nt.composer.cursor_pos(comp_area) {
        f.set_cursor_position((x, y));
    }
}

fn draw_env_modal(
    f: &mut Frame<'_>,
    envs: &[env_api::Environment],
    em: &mut EnvModalState,
    area: Rect,
) {
    let popup = centered_rect(80, 70, area);
    f.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Select environment");
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);

    let q = format!("filter: {}", em.query);
    f.render_widget(Paragraph::new(q), parts[0]);

    let options = env_modal_options(envs, &em.query);
    let items: Vec<ListItem> = options
        .iter()
        .map(|opt| {
            if let Some(e) = opt {
                let pinned = if e.is_pinned.unwrap_or(false) {
                    "*"
                } else {
                    " "
                };
                let label = e.label.clone().unwrap_or_else(|| "".to_string());
                ListItem::new(Line::from(format!("{pinned} {}  {label}", e.id)))
            } else {
                ListItem::new(Line::from("(all environments)"))
            }
        })
        .collect();
    em.selected = em.selected.min(items.len().saturating_sub(1));
    let list = List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ratatui::widgets::ListState::default();
    state.select(Some(em.selected));
    f.render_stateful_widget(list, parts[1], &mut state);
}

fn draw_attempts_modal(f: &mut Frame<'_>, am: &mut AttemptsModalState, area: Rect) {
    let popup = centered_rect(40, 30, area);
    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Agents (best-of-N)");
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let items: Vec<ListItem> = (1..=4)
        .map(|n| ListItem::new(Line::from(format!("{n}"))))
        .collect();
    am.selected = am.selected.min(3);
    let list = List::new(items).highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ratatui::widgets::ListState::default();
    state.select(Some(am.selected));
    f.render_stateful_widget(list, inner, &mut state);
}

fn draw_apply_modal(f: &mut Frame<'_>, spinner: char, m: &mut ApplyModalState, area: Rect) {
    let popup = centered_rect(90, 70, area);
    f.render_widget(Clear, popup);

    let title = format!(
        "Apply • {} • attempt {}",
        truncate(&m.title, 60),
        m.attempt_label
    );
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(inner);

    let wt = if m.use_worktree { "on" } else { "off" };
    let pr = if m.create_pr { "on" } else { "off" };
    let head = format!("worktree: {wt}\ncreate_pr: {pr}\nbranch: {}\n", m.pr_branch);
    f.render_widget(Paragraph::new(head).wrap(Wrap { trim: false }), parts[0]);

    // Result text.
    let mut lines: Vec<String> = Vec::new();
    match m.stage {
        ApplyStage::PreflightRunning => {
            lines.push(format!("{spinner} preflighting…"));
        }
        ApplyStage::Applying => {
            lines.push(format!("{spinner} applying…"));
        }
        _ => {}
    }
    if let Some(wt) = &m.worktree_path {
        lines.push(format!("worktree_path: {}", wt.display()));
        lines.push(String::new());
    }
    if let Some(out) = &m.preflight {
        lines.push(format!("preflight: {:?}", out.status));
        lines.push(out.message.clone());
        if !out.skipped_paths.is_empty() {
            lines.push(format!("skipped: {}", out.skipped_paths.join(", ")));
        }
        if !out.conflict_paths.is_empty() {
            lines.push(format!("conflicts: {}", out.conflict_paths.join(", ")));
        }
        lines.push(String::new());
    }
    if let Some(out) = &m.apply {
        lines.push(format!("apply: {:?}", out.status));
        lines.push(out.message.clone());
        if !out.skipped_paths.is_empty() {
            lines.push(format!("skipped: {}", out.skipped_paths.join(", ")));
        }
        if !out.conflict_paths.is_empty() {
            lines.push(format!("conflicts: {}", out.conflict_paths.join(", ")));
        }
        lines.push(String::new());
    }
    if let Some(pr) = &m.pr {
        lines.push(format!("branch: {}", pr.branch));
        lines.push(format!("remote: {}", pr.remote));
        lines.push(format!("created_with_gh: {}", pr.used_gh));
        if let Some(url) = &pr.pr_url {
            lines.push(format!("PR: {url}"));
        } else {
            lines.push("PR: created (no URL detected)".to_string());
        }
        if !pr.stdout.trim().is_empty() {
            lines.push(String::new());
            lines.push("stdout:".to_string());
            lines.extend(pr.stdout.lines().map(|s| s.to_string()));
        }
        if !pr.stderr.trim().is_empty() {
            lines.push(String::new());
            lines.push("stderr:".to_string());
            lines.extend(pr.stderr.lines().map(|s| s.to_string()));
        }
    }
    if let Some(e) = &m.error {
        lines.push(format!("error: {e}"));
    }
    if lines.is_empty() {
        lines.push("(no output)".to_string());
    }
    f.render_widget(
        Paragraph::new(lines.join("\n"))
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title("Result")),
        parts[1],
    );

    // Footer.
    let mut footer = "w: worktree • c: PR • p: preflight • y: apply • esc: close".to_string();
    if matches!(m.stage, ApplyStage::PreflightRunning | ApplyStage::Applying) {
        footer = "(running…) esc: close disabled".to_string();
    }
    f.render_widget(
        Paragraph::new(footer).alignment(Alignment::Center),
        parts[2],
    );
}

fn env_modal_options<'a>(
    envs: &'a [env_api::Environment],
    query: &str,
) -> Vec<Option<&'a env_api::Environment>> {
    let mut out: Vec<Option<&env_api::Environment>> = Vec::new();
    out.push(None);

    let q = query.trim().to_lowercase();
    for e in envs {
        if q.is_empty() {
            out.push(Some(e));
            continue;
        }
        let id_hit = e.id.to_lowercase().contains(&q);
        let label_hit = e.label.as_deref().unwrap_or("").to_lowercase().contains(&q);
        if id_hit || label_hit {
            out.push(Some(e));
        }
    }
    out
}

async fn fetch_task_details(
    backend: &dyn CloudBackend,
    id: TaskId,
) -> anyhow::Result<TaskDetailsPayload> {
    let summary = backend.get_task_summary(id.clone()).await?;
    let text = backend.get_task_text(id.clone()).await.unwrap_or_default();
    let diff = backend.get_task_diff(id.clone()).await?;

    let mut attempts: Vec<AttemptInfo> = Vec::new();

    let base_turn_id = text.turn_id.clone();
    let base_placement = text.attempt_placement;

    attempts.push(AttemptInfo {
        placement: text.attempt_placement,
        turn_id: text.turn_id.clone(),
        status: text.attempt_status,
        prompt: text.prompt.clone(),
        messages: text.messages.clone(),
        diff: diff.clone(),
    });

    if let Some(turn_id) = text.turn_id {
        if let Ok(sibs) = backend.list_sibling_attempts(id.clone(), turn_id).await {
            for s in sibs {
                // Merge duplicates.
                if base_turn_id.as_deref().is_some_and(|t| t == s.turn_id)
                    || (base_placement.is_some() && base_placement == s.attempt_placement)
                {
                    if let Some(base) = attempts.first_mut() {
                        if base.diff.is_none() {
                            base.diff = s.diff;
                        }
                        if base.messages.is_empty() {
                            base.messages = s.messages;
                        }
                        base.status = s.status;
                    }
                    continue;
                }
                attempts.push(AttemptInfo {
                    placement: s.attempt_placement,
                    turn_id: Some(s.turn_id),
                    status: s.status,
                    prompt: None,
                    messages: s.messages,
                    diff: s.diff,
                });
            }
        }
    }

    // Stable ordering by placement when available.
    attempts.sort_by(|a, b| match (a.placement, b.placement) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.turn_id.cmp(&b.turn_id),
    });

    Ok(TaskDetailsPayload { summary, attempts })
}

fn spawn_load_envs(session: Session, tx: UnboundedSender<AppEvent>) {
    tokio::spawn(async move {
        let res = env_api::list_environments(&session)
            .await
            .map_err(|e| format!("{e}"));
        let _ = tx.send(AppEvent::EnvsLoaded(res));
    });
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut out = s[..max].to_string();
    out.push('…');
    out
}

fn format_relative_time_now(ts: DateTime<Utc>) -> String {
    let now = Utc::now();
    let mut secs = (now - ts).num_seconds();
    if secs < 0 {
        secs = 0;
    }
    if secs < 60 {
        return format!("{secs}s ago");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    ts.to_rfc3339()
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);
    let vertical = popup_layout[1];
    let popup_layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical);
    popup_layout[1]
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

fn current_git_ref() -> anyhow::Result<String> {
    // Prefer modern porcelain: git branch --show-current
    if let Ok(out) = std::process::Command::new("git")
        .args(["branch", "--show-current"])
        .output()
    {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return Ok(s);
            }
        }
    }

    if let Ok(out) = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
    {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() && s != "HEAD" {
                return Ok(s);
            }
        }
    }

    Ok("main".to_string())
}

// -----------------------------------------------------------------------------
// Scrollable text view (copied/adapted from codex-cloud-tasks' scrollable_diff).
// -----------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default)]
struct ScrollViewState {
    scroll: u16,
    viewport_h: u16,
    content_h: u16,
}

impl ScrollViewState {
    fn clamp(&mut self) {
        let max_scroll = self.content_h.saturating_sub(self.viewport_h);
        if self.scroll > max_scroll {
            self.scroll = max_scroll;
        }
    }
}

#[derive(Clone, Debug, Default)]
struct ScrollableText {
    raw: Vec<String>,
    wrapped: Vec<String>,
    wrap_cols: Option<u16>,
    state: ScrollViewState,
}

impl ScrollableText {
    fn new() -> Self {
        Self::default()
    }

    fn set_content(&mut self, lines: Vec<String>) {
        self.raw = lines;
        self.wrapped.clear();
        self.state.content_h = 0;
        self.wrap_cols = None;
    }

    fn set_width(&mut self, width: u16) {
        if self.wrap_cols == Some(width) {
            return;
        }
        self.wrap_cols = Some(width);
        self.rewrap(width);
        self.state.clamp();
    }

    fn set_viewport(&mut self, height: u16) {
        self.state.viewport_h = height;
        self.state.clamp();
    }

    fn wrapped_lines(&self) -> &[String] {
        &self.wrapped
    }

    fn scroll_by(&mut self, delta: i16) {
        let s = self.state.scroll as i32 + delta as i32;
        self.state.scroll = s.clamp(0, self.max_scroll() as i32) as u16;
    }

    fn page_by(&mut self, delta: i16) {
        self.scroll_by(delta);
    }

    fn to_top(&mut self) {
        self.state.scroll = 0;
    }

    fn to_bottom(&mut self) {
        self.state.scroll = self.max_scroll();
    }

    fn max_scroll(&self) -> u16 {
        self.state.content_h.saturating_sub(self.state.viewport_h)
    }

    fn rewrap(&mut self, width: u16) {
        if width == 0 {
            self.wrapped = self.raw.clone();
            self.state.content_h = self.wrapped.len() as u16;
            return;
        }
        let max_cols = width as usize;
        let mut out: Vec<String> = Vec::new();
        for raw in &self.raw {
            let raw = raw.replace('\t', "    ");
            if raw.is_empty() {
                out.push(String::new());
                continue;
            }
            let mut line = String::new();
            let mut line_cols = 0usize;
            let mut last_soft_idx: Option<usize> = None;
            for (_i, ch) in raw.char_indices() {
                if ch == '\n' {
                    out.push(std::mem::take(&mut line));
                    line_cols = 0;
                    last_soft_idx = None;
                    continue;
                }
                let w = UnicodeWidthChar::width(ch).unwrap_or(0);
                if line_cols.saturating_add(w) > max_cols {
                    if let Some(split) = last_soft_idx {
                        let (prefix, rest) = line.split_at(split);
                        out.push(prefix.trim_end().to_string());
                        line = rest.trim_start().to_string();
                        last_soft_idx = None;
                    } else if !line.is_empty() {
                        out.push(std::mem::take(&mut line));
                    }
                }
                if ch.is_whitespace()
                    || matches!(
                        ch,
                        ',' | ';' | '.' | ':' | ')' | ']' | '}' | '|' | '/' | '?' | '!' | '-' | '_'
                    )
                {
                    last_soft_idx = Some(line.len());
                }
                line.push(ch);
                line_cols = UnicodeWidthStr::width(line.as_str());
            }
            if !line.is_empty() {
                out.push(line);
            }
        }
        self.wrapped = out;
        self.state.content_h = self.wrapped.len() as u16;
    }
}
