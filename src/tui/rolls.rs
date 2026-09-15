//! Interactive rolls view for `rf status` / `rf list`.
//!
//! Beyond navigation this drives workflow operations on the selected roll
//! (issues #20/#21): action keys open a confirmation modal, and on confirm the
//! op runs through [`crate::core::ops`] with the terminal suspended so git's
//! own output is visible, then the roll list reloads in place.

use std::io::{self, Write};
use std::time::Duration;

use anyhow::{anyhow, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Cell, Clear, Paragraph, Row, Table, TableState},
    Frame,
};

use crate::core::{
    branches::{self, BranchLocation, RollInfo, RollState},
    config::Config,
    git, ops,
};

/// A workflow operation reachable from the view. Navigation, quit and refresh
/// are handled directly; only these mutating ops go through the confirm modal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Action {
    /// Graduate the selected roll into the rolling branch.
    Graduate,
    /// Promote the rolling branch into stable.
    Promote,
    /// Update all active local rolls from stable.
    Update,
    /// Delete every promoted roll branch, locally and on origin.
    Prune,
}

/// App state driving the event loop and the optional modal overlay.
enum Mode {
    Browsing,
    Confirm {
        action: Action,
        /// The roll branch an action targets (graduate); `None` for repo-wide
        /// ops (promote / update).
        target: Option<String>,
    },
    /// Read-only drill-down for a single roll: its identity plus its dependency
    /// rows (issue #61). A snapshot of the selected roll is captured on open so
    /// the overlay stays stable regardless of later list reloads. For
    /// both-location rolls, `ahead_behind` holds the local-vs-`origin` divergence
    /// captured at open time (issue #99); `None` when not applicable/unknown.
    Detail {
        roll: RollInfo,
        ahead_behind: Option<(u32, u32)>,
    },
    /// Slug-input modal for creating a new roll (issue #79). Holds the
    /// in-progress text buffer; on Enter it runs `ops::create` through the same
    /// suspend/resume path as the other actions.
    CreateInput {
        slug: String,
    },
}

/// What a keystroke in the [`Mode::CreateInput`] modal asks the loop to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum InputOutcome {
    /// Buffer was (maybe) edited in place; stay in the input modal.
    Continue,
    /// Esc — discard the buffer and return to browsing.
    Cancel,
    /// Enter — attempt to create a roll from the buffer.
    Submit,
}

/// Which role a pinned base-branch row plays. These are the two long-lived
/// branches rolls flow through; they are listed above the rolls so the same
/// keys reach them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BaseRole {
    Stable,
    Rolling,
}

impl BaseRole {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            BaseRole::Stable => "stable",
            BaseRole::Rolling => "rolling",
        }
    }

    /// Matches the colours the header uses for the same two branches.
    fn color(&self) -> Color {
        match self {
            BaseRole::Stable => Color::Green,
            BaseRole::Rolling => Color::Cyan,
        }
    }
}

/// A pinned row for one of the configured base branches (stable / rolling).
/// Carries only what the table and the switch action need — base branches have
/// no roll number, state or dependencies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BaseBranch {
    pub role: BaseRole,
    pub branch: String,
    pub location: BranchLocation,
    pub is_current: bool,
}

/// Which table row a selection index lands on: one of the pinned base branches
/// at the top, or one of the rolls below them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RowKind {
    Base(usize),
    Roll(usize),
}

/// Map a flat table index onto the base-then-roll row layout. `None` when the
/// index is past the last row.
pub(crate) fn row_at(index: usize, base_count: usize, roll_count: usize) -> Option<RowKind> {
    if index < base_count {
        Some(RowKind::Base(index))
    } else if index - base_count < roll_count {
        Some(RowKind::Roll(index - base_count))
    } else {
        None
    }
}

/// Build the pinned base-branch rows, stable first then rolling. `exists`
/// answers whether a refspec resolves in the repo — injected so this stays pure
/// and unit-testable. A branch that is present neither locally nor on `origin`
/// is omitted, as is a rolling branch configured identically to stable.
pub(crate) fn base_branches(
    config: &Config,
    current_branch: &str,
    exists: impl Fn(&str) -> bool,
) -> Vec<BaseBranch> {
    let mut out: Vec<BaseBranch> = Vec::new();
    for (role, name) in [
        (BaseRole::Stable, &config.stable_branch),
        (BaseRole::Rolling, &config.rolling_branch),
    ] {
        if name.is_empty() || out.iter().any(|b| b.branch == *name) {
            continue;
        }
        let location = match (exists(name), exists(&format!("origin/{name}"))) {
            (true, true) => BranchLocation::Both,
            (true, false) => BranchLocation::Local,
            (false, true) => BranchLocation::Remote,
            (false, false) => continue,
        };
        out.push(BaseBranch {
            role,
            branch: name.clone(),
            location,
            is_current: current_branch == name,
        });
    }
    out
}

/// Initial table selection: the row for the current branch when it is on
/// screen (a base branch or one of the rolls), else the first row. `None` only
/// when there is nothing to select at all.
pub(crate) fn initial_selection(bases: &[BaseBranch], rolls: &[RollInfo]) -> Option<usize> {
    if bases.is_empty() && rolls.is_empty() {
        return None;
    }
    if let Some(i) = bases.iter().position(|b| b.is_current) {
        return Some(i);
    }
    if let Some(i) = rolls.iter().position(|r| r.is_current) {
        return Some(bases.len() + i);
    }
    Some(0)
}

/// One dependency row rendered in the [`Mode::Detail`] view. `is_blocker` marks
/// a dep that holds the roll back — one that is not yet graduated/promoted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DepRow {
    pub number: u32,
    pub branch: String,
    pub state: RollState,
    pub is_blocker: bool,
}

/// Owns everything needed to render and to *reload* after an action.
struct StatusApp {
    config: Config,
    current_branch: String,
    /// Pinned stable/rolling rows shown above the rolls; recomputed on reload
    /// so `is_current` tracks the branch actually checked out.
    bases: Vec<BaseBranch>,
    rolls: Vec<RollInfo>,
    show_deps: bool,
    table: TableState,
    mode: Mode,
    /// Transient one-line feedback (e.g. why an action was rejected), cleared on
    /// the next browsing keypress.
    message: Option<String>,
}

/// Entry point. Takes ownership of the data so the app can rebuild it after an
/// action mutates the repo.
pub fn run(
    config: Config,
    current_branch: String,
    rolls: Vec<RollInfo>,
    show_deps: bool,
) -> Result<()> {
    let mut terminal = super::enter()?;
    let mut app = StatusApp::new(config, current_branch, rolls, show_deps);
    let result = app.run_loop(&mut terminal);
    // Always restore the terminal, even if the loop returned an error.
    super::exit(terminal)?;
    result
}

// ── Pure decision logic (unit-tested) ───────────────────────────────────────

/// A roll can graduate only while it is active (or diverged and needs
/// re-graduation). Graduated / promoted / blocked rolls cannot.
pub(crate) fn can_graduate(state: &RollState) -> bool {
    matches!(state, RollState::Active | RollState::Diverged)
}

/// Promotion is offered when the rolling branch has something to carry to
/// stable — i.e. at least one graduated (or diverged) roll exists.
pub(crate) fn can_promote(rolls: &[RollInfo]) -> bool {
    rolls
        .iter()
        .any(|r| matches!(r.state, RollState::Graduated | RollState::Diverged))
}

/// Update is offered when there is at least one local, still-active roll to
/// merge stable into.
pub(crate) fn can_update(rolls: &[RollInfo]) -> bool {
    rolls.iter().any(|r| {
        matches!(r.state, RollState::Active | RollState::Blocked)
            && matches!(r.location, BranchLocation::Local | BranchLocation::Both)
    })
}

/// Prune is offered when at least one roll has been promoted to stable, i.e.
/// there is something whose branch has outlived its usefulness.
///
/// Whether a given branch is *safe* to delete is decided by `ops::prune_plan`,
/// which re-checks containment in stable; this only answers whether the action
/// is worth offering at all.
pub(crate) fn can_prune(rolls: &[RollInfo]) -> bool {
    rolls.iter().any(|r| matches!(r.state, RollState::Promoted))
}

/// How many rolls the prune action would consider — shown in the confirm modal.
pub(crate) fn prunable_count(rolls: &[RollInfo]) -> usize {
    rolls
        .iter()
        .filter(|r| matches!(r.state, RollState::Promoted))
        .count()
}

/// Build the dependency rows to show in the detail view for `selected`.
///
/// Each number in `selected.deps` is looked up in `all` to recover the
/// dependency's branch and state. A dep is flagged as a *blocker* when it is not
/// yet graduated/promoted (state is `Active`/`Blocked`/`Diverged`) — those are
/// what actually hold the roll back. Unknown dep numbers (not present in `all`)
/// are skipped. The empty result means "no dependencies / not blocked".
pub(crate) fn dep_rows(selected: &RollInfo, all: &[RollInfo]) -> Vec<DepRow> {
    selected
        .deps
        .iter()
        .filter_map(|num| all.iter().find(|r| r.number == *num))
        .map(|dep| DepRow {
            number: dep.number,
            branch: dep.branch.clone(),
            state: dep.state.clone(),
            is_blocker: !matches!(dep.state, RollState::Graduated | RollState::Promoted),
        })
        .collect()
}

/// Build the *reverse*-dependency rows to show in the detail view for `target`:
/// every roll whose `deps` contains `target.number` (i.e. the rolls that
/// integrated `target` and therefore depend on it). This is the inverse of
/// [`dep_rows`] and is *not* symmetric with it.
///
/// A row's `is_blocker` here is repurposed to mean "this dependent is still
/// gated by the target" — true while `target` has not yet graduated/promoted,
/// since until then the dependent cannot advance past it. The detail view does
/// not render a per-row blocker marker for dependents, so this flag is purely
/// informational, but it keeps the field meaningful and testable. A roll is
/// never its own dependent (a roll cannot list itself in its own `deps`).
pub(crate) fn dependent_rows(target: &RollInfo, all: &[RollInfo]) -> Vec<DepRow> {
    let target_gates = !matches!(target.state, RollState::Graduated | RollState::Promoted);
    all.iter()
        .filter(|r| r.number != target.number && r.deps.contains(&target.number))
        .map(|dep| DepRow {
            number: dep.number,
            branch: dep.branch.clone(),
            state: dep.state.clone(),
            is_blocker: target_gates,
        })
        .collect()
}

/// Render the ahead/behind divergence of a both-location roll versus its
/// `origin` remote for the detail overlay (issue #99). `None` in → `None` out
/// (nothing to show); otherwise a line like `ahead 2 / behind 1 vs origin`.
/// Pure, so the wording is unit-testable without a terminal.
pub(crate) fn format_ahead_behind(ahead_behind: Option<(u32, u32)>) -> Option<String> {
    ahead_behind.map(|(ahead, behind)| format!("ahead {ahead} / behind {behind} vs origin"))
}

/// Colour used to render a roll state consistently across the table and detail
/// view.
fn state_color(state: &RollState) -> Color {
    match state {
        RollState::Active => Color::Yellow,
        RollState::Graduated => Color::Green,
        RollState::Diverged => Color::Red,
        RollState::Promoted => Color::DarkGray,
        RollState::Blocked => Color::Magenta,
    }
}

/// Validate an action against the current selection/list. `Ok(())` means the
/// confirm modal may open; `Err(msg)` is a brief reason to surface instead.
pub(crate) fn validate_action(
    action: Action,
    selected: Option<&RollInfo>,
    rolls: &[RollInfo],
) -> Result<(), String> {
    match action {
        Action::Graduate => {
            let sel = selected.ok_or_else(|| "no roll selected".to_string())?;
            if can_graduate(&sel.state) {
                Ok(())
            } else {
                Err(format!(
                    "{} is {} — only active or diverged rolls can graduate",
                    sel.branch,
                    sel.state.label()
                ))
            }
        }
        Action::Promote => {
            if can_promote(rolls) {
                Ok(())
            } else {
                Err("nothing to promote — no graduated rolls on rolling".to_string())
            }
        }
        Action::Update => {
            if can_update(rolls) {
                Ok(())
            } else {
                Err("no active local rolls to update".to_string())
            }
        }
        Action::Prune => {
            if can_prune(rolls) {
                Ok(())
            } else {
                Err("nothing to prune — no promoted roll branches".to_string())
            }
        }
    }
}

/// Apply one keystroke to the create-input `buffer` and report what the loop
/// should do next. Printable characters append, Backspace deletes the last
/// character, Enter submits, Esc cancels; other keys are ignored. Control
/// characters are never inserted. Kept pure (mutates only the buffer) so it can
/// be unit-tested without a terminal.
pub(crate) fn handle_create_key(buffer: &mut String, code: KeyCode) -> InputOutcome {
    match code {
        KeyCode::Esc => InputOutcome::Cancel,
        KeyCode::Enter => InputOutcome::Submit,
        KeyCode::Backspace => {
            buffer.pop();
            InputOutcome::Continue
        }
        KeyCode::Char(c) if !c.is_control() => {
            buffer.push(c);
            InputOutcome::Continue
        }
        _ => InputOutcome::Continue,
    }
}

/// Whether the create-input buffer holds something worth handing to
/// `ops::create` — i.e. it is not blank. `ops::create` still does the real
/// slug normalization/validation; this only guards the empty case up front.
pub(crate) fn is_submittable_slug(buffer: &str) -> bool {
    !buffer.trim().is_empty()
}

// ── App ─────────────────────────────────────────────────────────────────────

impl StatusApp {
    fn new(config: Config, current_branch: String, rolls: Vec<RollInfo>, show_deps: bool) -> Self {
        let bases = base_branches(&config, &current_branch, |refspec| {
            git::ref_exists(&config.repo_root, refspec)
        });
        let mut table = TableState::default();
        table.select(initial_selection(&bases, &rolls));
        Self {
            config,
            current_branch,
            bases,
            rolls,
            show_deps,
            table,
            mode: Mode::Browsing,
            message: None,
        }
    }

    fn run_loop(&mut self, terminal: &mut super::Tui) -> Result<()> {
        loop {
            terminal.draw(|f| self.render(f))?;

            if event::poll(Duration::from_millis(50))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    if matches!(self.mode, Mode::Confirm { .. }) {
                        self.handle_confirm(terminal, key.code)?;
                    } else if matches!(self.mode, Mode::Detail { .. }) {
                        self.handle_detail(key.code);
                    } else if matches!(self.mode, Mode::CreateInput { .. }) {
                        self.handle_create_input(terminal, key.code)?;
                    } else if self.handle_browsing(terminal, key.code)? {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    /// Handle a keypress while browsing. Returns `Ok(true)` to quit.
    fn handle_browsing(&mut self, terminal: &mut super::Tui, code: KeyCode) -> Result<bool> {
        self.message = None;
        match code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
            KeyCode::Down | KeyCode::Char('j') => self.select_next(),
            KeyCode::Up | KeyCode::Char('k') => self.select_prev(),
            KeyCode::Char('r') => {
                self.reload()?;
                self.message = Some("refreshed".to_string());
            }
            KeyCode::Char('c') => {
                self.mode = Mode::CreateInput {
                    slug: String::new(),
                }
            }
            KeyCode::Char(' ') => match self.selected_row() {
                Some(RowKind::Roll(i)) => {
                    let roll = self.rolls[i].clone();
                    self.execute_switch(terminal, roll.branch, roll.location)?;
                }
                Some(RowKind::Base(i)) => {
                    let base = self.bases[i].clone();
                    self.execute_switch(terminal, base.branch, base.location)?;
                }
                None => self.message = Some("no branch selected".to_string()),
            },
            KeyCode::Char('g') => self.request(Action::Graduate),
            KeyCode::Char('p') => self.request(Action::Promote),
            KeyCode::Char('u') => self.request(Action::Update),
            KeyCode::Char('x') => self.request(Action::Prune),
            KeyCode::Enter => {
                if let Some(RowKind::Base(i)) = self.selected_row() {
                    // Base branches have no roll detail to drill into.
                    self.message = Some(format!("'{}' is a base branch", self.bases[i].branch));
                } else if let Some(roll) = self.selected_roll() {
                    let roll = roll.clone();
                    // Capture the branch's divergence from origin for the overlay
                    // (issue #99), only meaningful when it exists on both sides.
                    let ahead_behind = if matches!(roll.location, BranchLocation::Both) {
                        git::ahead_behind(&self.config.repo_root, &roll.branch).ok()
                    } else {
                        None
                    };
                    self.mode = Mode::Detail { roll, ahead_behind };
                }
            }
            _ => {}
        }
        Ok(false)
    }

    /// Handle a keypress while the read-only detail overlay is open. Only close
    /// keys apply; everything else is ignored so action keys can't fire here.
    fn handle_detail(&mut self, code: KeyCode) {
        if matches!(code, KeyCode::Char('q') | KeyCode::Esc) {
            self.mode = Mode::Browsing;
        }
    }

    /// Handle a keypress while the create-input modal is open: edit the buffer,
    /// cancel back to browsing, or submit. Submitting an empty buffer surfaces a
    /// message instead of invoking `ops::create`.
    fn handle_create_input(&mut self, terminal: &mut super::Tui, code: KeyCode) -> Result<()> {
        let outcome = if let Mode::CreateInput { slug } = &mut self.mode {
            handle_create_key(slug, code)
        } else {
            return Ok(());
        };
        match outcome {
            InputOutcome::Continue => {}
            InputOutcome::Cancel => self.mode = Mode::Browsing,
            InputOutcome::Submit => {
                let slug = match std::mem::replace(&mut self.mode, Mode::Browsing) {
                    Mode::CreateInput { slug } => slug,
                    _ => String::new(),
                };
                if is_submittable_slug(&slug) {
                    self.execute_create(terminal, slug)?;
                } else {
                    self.message = Some("slug cannot be empty".to_string());
                }
            }
        }
        Ok(())
    }

    /// Handle a keypress while the confirm modal is open.
    fn handle_confirm(&mut self, terminal: &mut super::Tui, code: KeyCode) -> Result<()> {
        match code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if let Mode::Confirm { action, target } =
                    std::mem::replace(&mut self.mode, Mode::Browsing)
                {
                    self.execute(terminal, action, target)?;
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.mode = Mode::Browsing;
            }
            _ => {}
        }
        Ok(())
    }

    /// Total number of table rows: the pinned base branches plus the rolls.
    fn row_count(&self) -> usize {
        self.bases.len() + self.rolls.len()
    }

    /// Which row the cursor is on, or `None` when the table is empty.
    fn selected_row(&self) -> Option<RowKind> {
        let index = self.table.selected()?;
        row_at(index, self.bases.len(), self.rolls.len())
    }

    /// The selected roll, or `None` when the cursor is on a base-branch row —
    /// roll-only actions treat that the same as no selection.
    fn selected_roll(&self) -> Option<&RollInfo> {
        match self.selected_row()? {
            RowKind::Roll(i) => self.rolls.get(i),
            RowKind::Base(_) => None,
        }
    }

    /// Validate an action and either open the confirm modal or set a message.
    fn request(&mut self, action: Action) {
        let selected = self.selected_roll();
        let validation = validate_action(action, selected, &self.rolls);
        let target = match action {
            Action::Graduate => selected.map(|r| r.branch.clone()),
            _ => None,
        };
        match validation {
            Ok(()) => self.mode = Mode::Confirm { action, target },
            Err(msg) => self.message = Some(msg),
        }
    }

    fn select_next(&mut self) {
        let rows = self.row_count();
        if rows == 0 {
            return;
        }
        let next = self
            .table
            .selected()
            .map(|i| (i + 1).min(rows - 1))
            .unwrap_or(0);
        self.table.select(Some(next));
    }

    fn select_prev(&mut self) {
        if self.row_count() == 0 {
            return;
        }
        let prev = self
            .table
            .selected()
            .map(|i| i.saturating_sub(1))
            .unwrap_or(0);
        self.table.select(Some(prev));
    }

    /// Suspend the TUI, run the op, surface its outcome (or error) on the normal
    /// terminal, wait for a keypress, resume, and reload the list.
    fn execute(
        &mut self,
        terminal: &mut super::Tui,
        action: Action,
        target: Option<String>,
    ) -> Result<()> {
        self.with_suspended(terminal, |app| {
            Ok((app.run_op(action, target.as_deref())?, ()))
        })?;
        Ok(())
    }

    /// Create a roll from `slug` through the same suspended execution path as the
    /// other actions, then reload and select the freshly created roll if it is
    /// present. An `ops::create` error (e.g. an invalid slug) is shown like any
    /// other action error and never aborts the TUI.
    fn execute_create(&mut self, terminal: &mut super::Tui, slug: String) -> Result<()> {
        let created = self.with_suspended(terminal, |app| {
            let outcome = ops::create(&app.config, &slug, None, false)?;
            Ok((vec![format!("Created {}", outcome.branch)], outcome.branch))
        })?;
        if let Some(branch) = created {
            if let Some(idx) = self.rolls.iter().position(|r| r.branch == branch) {
                self.table.select(Some(idx));
            }
        }
        Ok(())
    }

    /// Switch the working tree to `branch` (issue #99), through the same
    /// suspended path as the other actions so git's own output — including a
    /// conflict refusal — is visible, then reload so the dashboard reflects the
    /// new current branch. Git natively carries clean uncommitted changes forward
    /// and refuses (non-zero) when they would conflict; either way the TUI never
    /// crashes and the error is surfaced. Serves both roll rows and the pinned
    /// base-branch rows.
    fn execute_switch(
        &mut self,
        terminal: &mut super::Tui,
        branch: String,
        location: BranchLocation,
    ) -> Result<()> {
        self.with_suspended(terminal, |app| {
            Ok((app.run_switch(&branch, &location)?, ()))
        })?;
        Ok(())
    }

    /// Perform the branch switch, returning printable status lines. A
    /// remote-only branch is fetched first so `git switch` can DWIM-create a
    /// local tracking branch from `origin/<branch>`.
    fn run_switch(&self, branch: &str, location: &BranchLocation) -> Result<Vec<String>> {
        let repo = &self.config.repo_root;
        if branch == self.current_branch {
            return Ok(vec![format!("Already on '{branch}'")]);
        }
        let mut lines = Vec::new();
        if matches!(location, BranchLocation::Remote) {
            git::run_git(repo, &["fetch", "origin", branch])?;
            lines.push(format!("Fetched origin/{branch}"));
        }
        git::run_git(repo, &["switch", branch])?;
        lines.push(format!("Switched to '{branch}'"));
        Ok(lines)
    }

    /// Shared suspend → run → show → resume → reload wrapper. Runs `body` with the
    /// TUI suspended so git's own output shows on the normal terminal, prints the
    /// resulting lines (or the error chain) exactly like the actions do, waits for
    /// a keypress, resumes, and reloads the list regardless of success. Returns
    /// the value `body` produced on success, or `None` if it errored.
    fn with_suspended<T>(
        &mut self,
        terminal: &mut super::Tui,
        body: impl FnOnce(&Self) -> Result<(Vec<String>, T)>,
    ) -> Result<Option<T>> {
        super::suspend(terminal)?;

        let outcome = body(self);
        println!();
        let value = match outcome {
            Ok((lines, value)) => {
                for line in &lines {
                    println!("{line}");
                }
                Some(value)
            }
            Err(err) => {
                eprintln!("Error: {err}");
                for cause in err.chain().skip(1) {
                    eprintln!("  caused by: {cause}");
                }
                None
            }
        };
        println!();
        print!("Press any key to continue...");
        let _ = io::stdout().flush();

        let waited = super::wait_for_key();
        super::resume(terminal)?;
        waited?;

        // Reflect the new repo state in place regardless of op success/failure.
        self.reload()?;
        Ok(value)
    }

    /// Drive the actual operation through `core::ops`, rendering its structured
    /// outcome into printable lines. Never runs dry and never forces.
    fn run_op(&self, action: Action, target: Option<&str>) -> Result<Vec<String>> {
        let force = ops::ForceOpts::new(false, None)?;
        let mut lines = Vec::new();
        match action {
            Action::Graduate => {
                let roll = target.ok_or_else(|| anyhow!("no roll selected"))?;
                ops::ensure_clean_state(&self.config)?;
                let o = ops::graduate(&self.config, roll, false, &force)?;
                push_gate_notices(&mut lines, &o.gate_notices);
                lines.push(format!("Graduated '{}' into '{}'", o.roll, o.rolling));
            }
            Action::Promote => {
                ops::ensure_clean_state(&self.config)?;
                let o = ops::promote(&self.config, false, &force)?;
                push_gate_notices(&mut lines, &o.gate_notices);
                lines.push(format!("Promoted '{}' into '{}'", o.rolling, o.stable));
            }
            Action::Update => match ops::update(&self.config, false)? {
                ops::UpdateOutcome::NoActiveRolls => {
                    lines.push("no active local rolls to update".to_string());
                }
                ops::UpdateOutcome::Ran { stable, items } => {
                    for item in items {
                        match item {
                            ops::UpdateItem::AlreadyUpToDate { roll } => {
                                lines.push(format!(
                                    "'{roll}' is already up to date with '{stable}'"
                                ));
                            }
                            ops::UpdateItem::WouldMerge { roll, behind } => {
                                lines.push(format!(
                                    "would merge '{stable}' into '{roll}' ({behind} ahead)"
                                ));
                            }
                            ops::UpdateItem::Updated { roll } => {
                                lines.push(format!("updated '{roll}' with '{stable}'"));
                            }
                        }
                    }
                }
            },
            Action::Prune => {
                // The modal was the confirmation, so plan and apply run back to
                // back here. `PruneScope::both` never forces: a branch holding
                // commits stable lacks is reported as skipped, and clearing it
                // needs `rf prune --force` from the CLI, deliberately.
                let plan = ops::prune_plan(&self.config, &ops::PruneScope::both())?;
                if plan.is_empty() {
                    lines.push("no promoted roll branches to prune".to_string());
                } else {
                    for result in ops::prune_apply(&self.config, &plan)? {
                        if result.errors.is_empty() {
                            let mut where_ = Vec::new();
                            if result.local_deleted {
                                where_.push("local");
                            }
                            if result.remote_deleted {
                                where_.push("origin");
                            }
                            lines.push(format!(
                                "deleted '{}' ({})",
                                result.branch,
                                where_.join(", ")
                            ));
                        } else {
                            for err in &result.errors {
                                lines.push(format!("failed to delete '{}': {err}", result.branch));
                            }
                        }
                    }
                }
                for skip in &plan.skipped {
                    lines.push(format!("skipped '{}': {}", skip.branch, skip.reason));
                }
            }
        }
        Ok(lines)
    }

    /// Rebuild the roll list and current-branch after an action, keeping the
    /// selection in bounds.
    fn reload(&mut self) -> Result<()> {
        self.current_branch = git::current_branch(&self.config.repo_root)?;
        self.bases = base_branches(&self.config, &self.current_branch, |refspec| {
            git::ref_exists(&self.config.repo_root, refspec)
        });
        self.rolls = branches::list_rolls(&self.config)?;
        let len = self.row_count();
        if len == 0 {
            self.table.select(None);
        } else {
            let sel = self.table.selected().unwrap_or(0).min(len - 1);
            self.table.select(Some(sel));
        }
        Ok(())
    }

    fn render(&mut self, f: &mut Frame) {
        let area = f.area();

        let chunks = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .split(area);

        self.render_header(f, chunks[0]);
        self.render_table(f, chunks[1]);
        self.render_status_bar(f, chunks[2]);

        match &self.mode {
            Mode::Confirm { action, target } => {
                render_modal(
                    f,
                    area,
                    &self.config,
                    *action,
                    target.as_deref(),
                    &self.rolls,
                );
            }
            Mode::Detail { roll, ahead_behind } => {
                render_detail(f, area, roll, *ahead_behind, &self.rolls)
            }
            Mode::CreateInput { slug } => render_create_input(f, area, &self.config, slug),
            Mode::Browsing => {}
        }
    }

    fn render_header(&self, f: &mut Frame, area: Rect) {
        let header_line = Line::from(vec![
            Span::styled("Branch: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(self.current_branch.as_str()),
            Span::raw("   Rolling: "),
            Span::styled(
                self.config.rolling_branch.as_str(),
                Style::default().fg(Color::Cyan),
            ),
            Span::raw("   Stable: "),
            Span::styled(
                self.config.stable_branch.as_str(),
                Style::default().fg(Color::Green),
            ),
        ]);
        f.render_widget(
            Paragraph::new(header_line).block(Block::bordered().title(" roll-flow ")),
            area,
        );
    }

    fn render_table(&mut self, f: &mut Frame, area: Rect) {
        let mut col_constraints = vec![
            Constraint::Length(4),
            Constraint::Fill(1),
            Constraint::Length(3),
            Constraint::Length(13),
        ];
        if self.show_deps {
            col_constraints.push(Constraint::Length(8));
        }

        let mut header_cells = vec![
            Cell::from("#").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("branch").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("loc").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("state").style(Style::default().add_modifier(Modifier::BOLD)),
        ];
        if self.show_deps {
            header_cells
                .push(Cell::from("deps").style(Style::default().add_modifier(Modifier::BOLD)));
        }
        let table_header = Row::new(header_cells)
            .style(Style::default().add_modifier(Modifier::UNDERLINED))
            .height(1);

        let show_deps = self.show_deps;
        // Base branches are pinned above the rolls: no number and no state, the
        // `state` column carrying their role instead.
        let mut rows: Vec<Row> = self
            .bases
            .iter()
            .map(|base| {
                let base_style = if base.is_current {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let mut cells = vec![
                    Cell::from(""),
                    Cell::from(base.branch.clone()).style(base_style.fg(base.role.color())),
                    Cell::from(base.location.symbol()).style(base_style),
                    Cell::from(base.role.label()).style(Style::default().fg(base.role.color())),
                ];
                if show_deps {
                    cells.push(Cell::from(""));
                }
                Row::new(cells)
            })
            .collect();
        rows.extend(self.rolls.iter().map(|roll| {
            let row_state_color = state_color(&roll.state);
            let base_style = if roll.is_current {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let mut cells = vec![
                Cell::from(roll.number.to_string()).style(base_style),
                Cell::from(roll.branch.clone()).style(base_style),
                Cell::from(roll.location.symbol()).style(base_style),
                Cell::from(roll.state.label()).style(Style::default().fg(row_state_color)),
            ];
            if show_deps {
                let deps_str = if roll.deps.is_empty() {
                    String::new()
                } else {
                    roll.deps
                        .iter()
                        .map(|n| n.to_string())
                        .collect::<Vec<_>>()
                        .join(",")
                };
                cells.push(Cell::from(deps_str));
            }
            Row::new(cells)
        }));

        let table = Table::new(rows, col_constraints)
            .header(table_header)
            .block(Block::bordered().title(" branches "))
            .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .highlight_symbol("▶ ");

        f.render_stateful_widget(table, area, &mut self.table);
    }

    fn render_status_bar(&self, f: &mut Frame, area: Rect) {
        let msg_line = match &self.message {
            Some(m) => Line::from(Span::styled(
                format!(" {m}"),
                Style::default().fg(Color::Yellow),
            )),
            None => Line::from(""),
        };
        let hint_line = Line::from(
            " [q] quit   [j/k ↑/↓] nav   [space] switch   [enter] detail   [c]reate   [g]raduate   [p]romote   [u]pdate   [x] prune   [r]efresh",
        );
        f.render_widget(Paragraph::new(vec![msg_line, hint_line]), area);
    }
}

/// Render the centered confirmation popup for a pending action.
fn render_modal(
    f: &mut Frame,
    area: Rect,
    config: &Config,
    action: Action,
    target: Option<&str>,
    rolls: &[RollInfo],
) {
    let prompt = match action {
        Action::Graduate => format!(
            "Graduate {} into {}?",
            target.unwrap_or("(selected roll)"),
            config.rolling_branch
        ),
        Action::Promote => format!(
            "Promote {} into {}?",
            config.rolling_branch, config.stable_branch
        ),
        Action::Update => format!(
            "Update all active local rolls from {}?",
            config.stable_branch
        ),
        Action::Prune => {
            let n = prunable_count(rolls);
            format!(
                "Delete {n} promoted roll branch{} (local + origin)?",
                if n == 1 { "" } else { "es" }
            )
        }
    };
    let hint = "[y] confirm    [n] cancel";

    let width = (prompt.chars().count().max(hint.len()) as u16) + 4;
    let modal = centered_rect(area, width, 4);

    f.render_widget(Clear, modal);
    let body = Paragraph::new(vec![Line::from(prompt), Line::from(hint)])
        .alignment(Alignment::Center)
        .block(Block::bordered().title(" confirm "));
    f.render_widget(body, modal);
}

/// Render the centered slug-input popup for creating a new roll. Shows the
/// prompt, the current buffer with a trailing caret, and the key hints.
fn render_create_input(f: &mut Frame, area: Rect, config: &Config, buffer: &str) {
    let prompt = format!("New roll slug (branched from {}):", config.stable_branch);
    let input_line = format!("{buffer}_");
    let hint = "[enter] create    [esc] cancel";

    let width = prompt
        .chars()
        .count()
        .max(hint.len())
        .max(input_line.chars().count()) as u16
        + 4;
    let modal = centered_rect(area, width.max(40), 6);

    f.render_widget(Clear, modal);
    let body = Paragraph::new(vec![
        Line::from(prompt),
        Line::from(""),
        Line::from(Span::styled(input_line, Style::default().fg(Color::Cyan))),
        Line::from(""),
        Line::from(Span::styled(hint, Style::default().fg(Color::DarkGray))),
    ])
    .alignment(Alignment::Center)
    .block(Block::bordered().title(" create roll "));
    f.render_widget(body, modal);
}

/// Render the centered read-only detail popup for a single roll: its identity
/// and its dependency rows, with blockers clearly marked.
fn render_detail(
    f: &mut Frame,
    area: Rect,
    roll: &RollInfo,
    ahead_behind: Option<(u32, u32)>,
    all: &[RollInfo],
) {
    let rows = dep_rows(roll, all);
    let dependents = dependent_rows(roll, all);

    let mut lines = vec![
        Line::from(vec![
            Span::styled("roll #", Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(
                roll.number.to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(roll.branch.clone(), Style::default().fg(Color::Cyan)),
        ]),
        Line::from(vec![
            Span::raw("state: "),
            Span::styled(
                roll.state.label(),
                Style::default().fg(state_color(&roll.state)),
            ),
            Span::raw("    location: "),
            Span::raw(roll.location.label()),
        ]),
    ];

    // Local-vs-origin divergence, only for both-location rolls (issue #99).
    if let Some(text) = format_ahead_behind(ahead_behind) {
        lines.push(Line::from(Span::styled(
            text,
            Style::default().fg(Color::Yellow),
        )));
    }

    lines.push(Line::from(""));

    if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            "no dependencies / not blocked",
            Style::default().fg(Color::Green),
        )));
    } else {
        let blockers = rows.iter().filter(|r| r.is_blocker).count();
        let header = if blockers > 0 {
            format!("dependencies ({blockers} blocking):")
        } else {
            "dependencies (all graduated):".to_string()
        };
        lines.push(Line::from(Span::styled(
            header,
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for r in &rows {
            let (marker, marker_style) = if r.is_blocker {
                ("⛔ blocker", Style::default().fg(Color::Red))
            } else {
                ("✓ ok", Style::default().fg(Color::Green))
            };
            lines.push(Line::from(vec![
                Span::raw(format!("  #{}  ", r.number)),
                Span::styled(r.branch.clone(), Style::default().fg(Color::Cyan)),
                Span::raw("  ["),
                Span::styled(r.state.label(), Style::default().fg(state_color(&r.state))),
                Span::raw("]  "),
                Span::styled(marker, marker_style),
            ]));
        }
    }

    // Dependents (reverse dependencies): rolls that integrated this one. Shown
    // as its own section below dependencies since the relation is not symmetric.
    lines.push(Line::from(""));
    if dependents.is_empty() {
        lines.push(Line::from(Span::styled(
            "no dependents",
            Style::default().fg(Color::Green),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            format!("dependents ({}):", dependents.len()),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for r in &dependents {
            lines.push(Line::from(vec![
                Span::raw(format!("  #{}  ", r.number)),
                Span::styled(r.branch.clone(), Style::default().fg(Color::Cyan)),
                Span::raw("  ["),
                Span::styled(r.state.label(), Style::default().fg(state_color(&r.state))),
                Span::raw("]"),
            ]));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "[q/esc] back",
        Style::default().fg(Color::DarkGray),
    )));

    let width = lines.iter().map(|l| l.width()).max().unwrap_or(20) as u16 + 4;
    let height = lines.len() as u16 + 2;
    let popup = centered_rect(area, width.max(32), height);

    f.render_widget(Clear, popup);
    let body = Paragraph::new(lines).block(Block::bordered().title(" roll detail "));
    f.render_widget(body, popup);
}

fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    }
}

/// Append the gate-run notices as readable lines (mirrors `main.rs`).
fn push_gate_notices(lines: &mut Vec<String>, notices: &[ops::GateNotice]) {
    for notice in notices {
        match notice {
            ops::GateNotice::NoGates => lines.push("No gates configured".to_string()),
            ops::GateNotice::DryRun(gate) => lines.push(format!("Dry-run gate: {gate}")),
            ops::GateNotice::DryRunHost(gate) => lines.push(format!("Dry-run host gate: {gate}")),
            ops::GateNotice::Bypassed { gate, code } => lines.push(format!(
                "warning: gate failed but bypassed (--force): {gate} ({})",
                ops::exit_desc(*code)
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roll(state: RollState, location: BranchLocation) -> RollInfo {
        RollInfo {
            branch: "roll/1-0101-x".to_string(),
            number: 1,
            state,
            location,
            is_current: false,
            deps: Vec::new(),
        }
    }

    fn config(stable: &str, rolling: &str) -> Config {
        Config {
            config_version: 1,
            repo_root: std::path::PathBuf::from("/tmp/repo"),
            rolling_branch: rolling.to_string(),
            stable_branch: stable.to_string(),
            roll_prefix: "roll/".to_string(),
            mode: Default::default(),
            username: String::new(),
            hosts: Vec::new(),
            host_active: Default::default(),
            roll_to_rolling_gates: Vec::new(),
            rolling_to_main_gates: Vec::new(),
            host_gates: Vec::new(),
            clean_protect: Vec::new(),
        }
    }

    #[test]
    fn base_rows_list_stable_then_rolling() {
        let cfg = config("main", "develop");
        // Both exist locally only.
        let bases = base_branches(&cfg, "roll/1-0101-x", |r| !r.starts_with("origin/"));
        assert_eq!(bases.len(), 2);
        assert_eq!(bases[0].role, BaseRole::Stable);
        assert_eq!(bases[0].branch, "main");
        assert_eq!(bases[0].location, BranchLocation::Local);
        assert_eq!(bases[1].role, BaseRole::Rolling);
        assert_eq!(bases[1].branch, "develop");
        // Neither is checked out here.
        assert!(bases.iter().all(|b| !b.is_current));
    }

    #[test]
    fn base_rows_track_location_and_current_branch() {
        let cfg = config("main", "develop");
        // main on both sides, develop only on origin.
        let bases = base_branches(&cfg, "main", |r| {
            matches!(r, "main" | "origin/main" | "origin/develop")
        });
        assert_eq!(bases[0].location, BranchLocation::Both);
        assert!(bases[0].is_current);
        assert_eq!(bases[1].location, BranchLocation::Remote);
        assert!(!bases[1].is_current);
    }

    #[test]
    fn base_rows_omit_missing_and_duplicate_branches() {
        let cfg = config("main", "develop");
        // develop exists nowhere → only stable is shown.
        let bases = base_branches(&cfg, "main", |r| r == "main");
        assert_eq!(bases.len(), 1);
        assert_eq!(bases[0].branch, "main");

        // Rolling configured the same as stable collapses to one row.
        let same = config("main", "main");
        let bases = base_branches(&same, "main", |_| true);
        assert_eq!(bases.len(), 1);
        assert_eq!(bases[0].role, BaseRole::Stable);

        // Nothing resolves at all → no pinned rows.
        assert!(base_branches(&cfg, "main", |_| false).is_empty());
    }

    #[test]
    fn row_at_maps_indices_over_bases_then_rolls() {
        assert_eq!(row_at(0, 2, 3), Some(RowKind::Base(0)));
        assert_eq!(row_at(1, 2, 3), Some(RowKind::Base(1)));
        assert_eq!(row_at(2, 2, 3), Some(RowKind::Roll(0)));
        assert_eq!(row_at(4, 2, 3), Some(RowKind::Roll(2)));
        // Past the last row.
        assert_eq!(row_at(5, 2, 3), None);
        // No bases → rolls start at 0; no rolls → only bases.
        assert_eq!(row_at(0, 0, 1), Some(RowKind::Roll(0)));
        assert_eq!(row_at(1, 1, 0), None);
        assert_eq!(row_at(0, 0, 0), None);
    }

    #[test]
    fn initial_selection_prefers_the_current_branch() {
        let cfg = config("main", "develop");
        let bases = base_branches(&cfg, "develop", |_| true);
        let mut rolls = vec![roll_n(1, RollState::Active), roll_n(2, RollState::Active)];

        // Current branch is the rolling base → its own row.
        assert_eq!(initial_selection(&bases, &rolls), Some(1));

        // Current branch is a roll → offset past the bases.
        let off_bases = base_branches(&cfg, "roll/2-0101-x", |_| true);
        rolls[1].is_current = true;
        assert_eq!(initial_selection(&off_bases, &rolls), Some(3));

        // Nothing current → first row.
        rolls[1].is_current = false;
        assert_eq!(initial_selection(&off_bases, &rolls), Some(0));

        // Rolls but no bases still selects the first roll.
        assert_eq!(initial_selection(&[], &rolls), Some(0));

        // Nothing at all → no selection.
        assert_eq!(initial_selection(&[], &[]), None);
    }

    fn roll_n(number: u32, state: RollState) -> RollInfo {
        RollInfo {
            branch: format!("roll/{number}-0101-x"),
            number,
            state,
            location: BranchLocation::Local,
            is_current: false,
            deps: Vec::new(),
        }
    }

    #[test]
    fn graduate_valid_only_for_active_or_diverged() {
        assert!(can_graduate(&RollState::Active));
        assert!(can_graduate(&RollState::Diverged));
        assert!(!can_graduate(&RollState::Graduated));
        assert!(!can_graduate(&RollState::Promoted));
        assert!(!can_graduate(&RollState::Blocked));
    }

    #[test]
    fn prune_valid_only_when_a_promoted_roll_exists() {
        let unpromoted = vec![
            roll_n(1, RollState::Active),
            roll_n(2, RollState::Graduated),
            roll_n(3, RollState::Diverged),
            roll_n(4, RollState::Blocked),
        ];
        assert!(!can_prune(&unpromoted));
        assert_eq!(prunable_count(&unpromoted), 0);
        assert!(validate_action(Action::Prune, None, &unpromoted).is_err());

        let mut with_promoted = unpromoted.clone();
        with_promoted.push(roll_n(5, RollState::Promoted));
        with_promoted.push(roll_n(6, RollState::Promoted));
        assert!(can_prune(&with_promoted));
        assert_eq!(prunable_count(&with_promoted), 2);
        // Prune is repo-wide, so it validates with no selection.
        assert!(validate_action(Action::Prune, None, &with_promoted).is_ok());
    }

    #[test]
    fn promote_valid_when_a_graduated_roll_exists() {
        let none = vec![roll(RollState::Active, BranchLocation::Local)];
        assert!(!can_promote(&none));

        let graduated = vec![roll(RollState::Graduated, BranchLocation::Both)];
        assert!(can_promote(&graduated));

        let diverged = vec![roll(RollState::Diverged, BranchLocation::Both)];
        assert!(can_promote(&diverged));
    }

    #[test]
    fn update_valid_for_local_active_rolls_only() {
        assert!(can_update(&[roll(
            RollState::Active,
            BranchLocation::Local
        )]));
        assert!(can_update(&[roll(
            RollState::Blocked,
            BranchLocation::Both
        )]));
        // Remote-only active roll cannot be updated locally.
        assert!(!can_update(&[roll(
            RollState::Active,
            BranchLocation::Remote
        )]));
        // Graduated rolls are not update candidates.
        assert!(!can_update(&[roll(
            RollState::Graduated,
            BranchLocation::Both
        )]));
    }

    #[test]
    fn validate_action_reports_reasons() {
        let active = vec![roll(RollState::Active, BranchLocation::Local)];
        let graduated = vec![roll(RollState::Graduated, BranchLocation::Both)];

        // Graduate needs a valid selection.
        assert!(validate_action(Action::Graduate, None, &active).is_err());
        assert!(validate_action(Action::Graduate, Some(&active[0]), &active).is_ok());
        assert!(validate_action(Action::Graduate, Some(&graduated[0]), &graduated).is_err());

        // Promote needs a graduated roll on rolling.
        assert!(validate_action(Action::Promote, None, &active).is_err());
        assert!(validate_action(Action::Promote, None, &graduated).is_ok());

        // Update needs a local active roll.
        assert!(validate_action(Action::Update, None, &active).is_ok());
        assert!(validate_action(Action::Update, None, &graduated).is_err());
    }

    #[test]
    fn dep_rows_flag_only_ungraduated_as_blockers() {
        let all = vec![
            roll_n(1, RollState::Graduated),
            roll_n(2, RollState::Active),
            roll_n(3, RollState::Diverged),
            roll_n(4, RollState::Promoted),
        ];
        let mut selected = roll_n(5, RollState::Blocked);
        selected.deps = vec![1, 2, 3, 4];

        let rows = dep_rows(&selected, &all);
        assert_eq!(rows.len(), 4);
        // Graduated / promoted deps are satisfied — not blockers.
        assert!(!rows.iter().find(|r| r.number == 1).unwrap().is_blocker);
        assert!(!rows.iter().find(|r| r.number == 4).unwrap().is_blocker);
        // Active / diverged deps hold the roll back.
        assert!(rows.iter().find(|r| r.number == 2).unwrap().is_blocker);
        assert!(rows.iter().find(|r| r.number == 3).unwrap().is_blocker);

        let blockers: Vec<u32> = rows
            .iter()
            .filter(|r| r.is_blocker)
            .map(|r| r.number)
            .collect();
        assert_eq!(blockers, vec![2, 3]);
    }

    #[test]
    fn dep_rows_empty_when_no_deps() {
        let all = vec![roll_n(1, RollState::Graduated)];
        let selected = roll_n(2, RollState::Active); // deps left empty
        assert!(dep_rows(&selected, &all).is_empty());
    }

    #[test]
    fn dep_rows_all_graduated_has_zero_blockers() {
        let all = vec![
            roll_n(1, RollState::Graduated),
            roll_n(2, RollState::Promoted),
        ];
        let mut selected = roll_n(3, RollState::Active);
        selected.deps = vec![1, 2];

        let rows = dep_rows(&selected, &all);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| !r.is_blocker));
    }

    #[test]
    fn dependent_rows_lists_every_roll_that_integrated_target() {
        // rolls 17 and 18 both integrated roll 14 → both are 14's dependents.
        let mut r17 = roll_n(17, RollState::Active);
        r17.deps = vec![14];
        let mut r18 = roll_n(18, RollState::Blocked);
        r18.deps = vec![14, 15];
        let target = roll_n(14, RollState::Active);
        let all = vec![target.clone(), r17, r18, roll_n(15, RollState::Graduated)];

        let mut nums: Vec<u32> = dependent_rows(&target, &all)
            .iter()
            .map(|r| r.number)
            .collect();
        nums.sort_unstable();
        assert_eq!(nums, vec![17, 18]);
        // Target is ungraduated, so it still gates its dependents.
        assert!(dependent_rows(&target, &all).iter().all(|r| r.is_blocker));
    }

    #[test]
    fn dependent_rows_empty_when_nobody_depends() {
        let target = roll_n(14, RollState::Graduated);
        let all = vec![
            target.clone(),
            roll_n(15, RollState::Active), // no deps
            roll_n(16, RollState::Active), // no deps
        ];
        assert!(dependent_rows(&target, &all).is_empty());
    }

    #[test]
    fn dependent_rows_never_lists_target_itself() {
        // A self-referential deps entry must not turn the roll into its own
        // dependent.
        let mut target = roll_n(14, RollState::Active);
        target.deps = vec![14];
        let all = vec![target.clone()];
        assert!(dependent_rows(&target, &all).is_empty());
    }

    #[test]
    fn create_key_edits_buffer_and_reports_intent() {
        let mut buf = String::new();
        // Printable chars append.
        assert_eq!(
            handle_create_key(&mut buf, KeyCode::Char('a')),
            InputOutcome::Continue
        );
        assert_eq!(
            handle_create_key(&mut buf, KeyCode::Char('b')),
            InputOutcome::Continue
        );
        assert_eq!(buf, "ab");
        // Backspace deletes the last char.
        assert_eq!(
            handle_create_key(&mut buf, KeyCode::Backspace),
            InputOutcome::Continue
        );
        assert_eq!(buf, "a");
        // Backspace on an empty buffer is harmless.
        buf.clear();
        assert_eq!(
            handle_create_key(&mut buf, KeyCode::Backspace),
            InputOutcome::Continue
        );
        assert_eq!(buf, "");
        // Enter submits, Esc cancels — neither mutates the buffer.
        buf.push_str("theme");
        assert_eq!(
            handle_create_key(&mut buf, KeyCode::Enter),
            InputOutcome::Submit
        );
        assert_eq!(
            handle_create_key(&mut buf, KeyCode::Esc),
            InputOutcome::Cancel
        );
        assert_eq!(buf, "theme");
        // Non-text keys are ignored without editing.
        assert_eq!(
            handle_create_key(&mut buf, KeyCode::Left),
            InputOutcome::Continue
        );
        assert_eq!(buf, "theme");
    }

    #[test]
    fn submittable_slug_requires_non_blank() {
        assert!(!is_submittable_slug(""));
        assert!(!is_submittable_slug("   "));
        assert!(is_submittable_slug("theme"));
        assert!(is_submittable_slug("  theme  "));
    }

    #[test]
    fn format_ahead_behind_wording() {
        assert_eq!(
            format_ahead_behind(Some((2, 1))).as_deref(),
            Some("ahead 2 / behind 1 vs origin")
        );
        assert_eq!(
            format_ahead_behind(Some((0, 0))).as_deref(),
            Some("ahead 0 / behind 0 vs origin")
        );
        // Nothing to show when the divergence is unknown / not applicable.
        assert_eq!(format_ahead_behind(None), None);
    }

    #[test]
    fn dep_rows_skip_unknown_dep_numbers() {
        let all = vec![roll_n(1, RollState::Active)];
        let mut selected = roll_n(4, RollState::Blocked);
        selected.deps = vec![1, 99]; // 99 not present in `all`
        let rows = dep_rows(&selected, &all);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].number, 1);
    }

    /// The pinned base rows render above the rolls, with the role in the
    /// `state` column and no roll number, and the cursor starts on the checked
    /// out base branch.
    #[test]
    fn base_rows_render_above_the_rolls() {
        use ratatui::{backend::TestBackend, Terminal};

        let cfg = config("main", "develop");
        let bases = base_branches(&cfg, "develop", |_| true);
        let mut table = TableState::default();
        table.select(initial_selection(&bases, &[]));
        let mut app = StatusApp {
            config: cfg,
            current_branch: "develop".to_string(),
            bases,
            rolls: vec![roll_n(1, RollState::Active)],
            show_deps: false,
            table,
            mode: Mode::Browsing,
            message: None,
        };

        let mut term = Terminal::new(TestBackend::new(60, 12)).unwrap();
        term.draw(|f| app.render(f)).unwrap();
        let line = |row: u16| -> String {
            (0..60)
                .map(|x| term.backend().buffer()[(x, row)].symbol().to_string())
                .collect::<String>()
                .trim_end()
                .to_string()
        };

        // Row 4 is the table header, then stable, rolling, and the roll.
        assert!(line(5).contains("main"), "{}", line(5));
        assert!(line(5).contains("stable"), "{}", line(5));
        assert!(line(6).contains("develop"), "{}", line(6));
        assert!(line(6).contains("rolling"), "{}", line(6));
        assert!(line(7).contains("roll/1-0101-x"), "{}", line(7));
        // Cursor sits on the current branch (the rolling base), not the roll.
        assert!(line(6).contains('▶'), "{}", line(6));
        assert!(!line(5).contains('▶') && !line(7).contains('▶'));
    }
}
