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
    /// the overlay stays stable regardless of later list reloads.
    Detail {
        roll: RollInfo,
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
        let mut table = TableState::default();
        let initial = rolls.iter().position(|r| r.is_current).unwrap_or(0);
        if !rolls.is_empty() {
            table.select(Some(initial));
        }
        Self {
            config,
            current_branch,
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
                    } else if self.handle_browsing(key.code)? {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    /// Handle a keypress while browsing. Returns `Ok(true)` to quit.
    fn handle_browsing(&mut self, code: KeyCode) -> Result<bool> {
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
            KeyCode::Char('g') => self.request(Action::Graduate),
            KeyCode::Char('p') => self.request(Action::Promote),
            KeyCode::Char('u') => self.request(Action::Update),
            KeyCode::Enter => {
                if let Some(roll) = self.selected_roll() {
                    self.mode = Mode::Detail { roll: roll.clone() };
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

    fn selected_roll(&self) -> Option<&RollInfo> {
        self.table.selected().and_then(|i| self.rolls.get(i))
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
        if self.rolls.is_empty() {
            return;
        }
        let next = self
            .table
            .selected()
            .map(|i| (i + 1).min(self.rolls.len() - 1))
            .unwrap_or(0);
        self.table.select(Some(next));
    }

    fn select_prev(&mut self) {
        if self.rolls.is_empty() {
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
        }
        Ok(lines)
    }

    /// Rebuild the roll list and current-branch after an action, keeping the
    /// selection in bounds.
    fn reload(&mut self) -> Result<()> {
        self.current_branch = git::current_branch(&self.config.repo_root)?;
        self.rolls = branches::list_rolls(&self.config)?;
        let len = self.rolls.len();
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
                render_modal(f, area, &self.config, *action, target.as_deref());
            }
            Mode::Detail { roll } => render_detail(f, area, roll, &self.rolls),
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
        let rows: Vec<Row> = self
            .rolls
            .iter()
            .map(|roll| {
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
            })
            .collect();

        let table = Table::new(rows, col_constraints)
            .header(table_header)
            .block(Block::bordered().title(" rolls "))
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
            " [q] quit   [j/k ↑/↓] nav   [enter] detail   [c]reate   [g]raduate   [p]romote   [u]pdate   [r]efresh",
        );
        f.render_widget(Paragraph::new(vec![msg_line, hint_line]), area);
    }
}

/// Render the centered confirmation popup for a pending action.
fn render_modal(f: &mut Frame, area: Rect, config: &Config, action: Action, target: Option<&str>) {
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
fn render_detail(f: &mut Frame, area: Rect, roll: &RollInfo, all: &[RollInfo]) {
    let rows = dep_rows(roll, all);

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
            Span::raw(roll.location.symbol()),
        ]),
        Line::from(""),
    ];

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
    fn dep_rows_skip_unknown_dep_numbers() {
        let all = vec![roll_n(1, RollState::Active)];
        let mut selected = roll_n(4, RollState::Blocked);
        selected.deps = vec![1, 99]; // 99 not present in `all`
        let rows = dep_rows(&selected, &all);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].number, 1);
    }
}
