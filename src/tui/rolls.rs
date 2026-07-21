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
            KeyCode::Char('g') => self.request(Action::Graduate),
            KeyCode::Char('p') => self.request(Action::Promote),
            KeyCode::Char('u') => self.request(Action::Update),
            _ => {}
        }
        Ok(false)
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
        super::suspend(terminal)?;

        let outcome = self.run_op(action, target.as_deref());
        println!();
        match &outcome {
            Ok(lines) => {
                for line in lines {
                    println!("{line}");
                }
            }
            Err(err) => {
                eprintln!("Error: {err}");
                for cause in err.chain().skip(1) {
                    eprintln!("  caused by: {cause}");
                }
            }
        }
        println!();
        print!("Press any key to continue...");
        let _ = io::stdout().flush();

        let waited = super::wait_for_key();
        super::resume(terminal)?;
        waited?;

        // Reflect the new repo state in place regardless of op success/failure.
        self.reload()?;
        Ok(())
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

        if let Mode::Confirm { action, target } = &self.mode {
            render_modal(f, area, &self.config, *action, target.as_deref());
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
                let state_color = match roll.state {
                    RollState::Active => Color::Yellow,
                    RollState::Graduated => Color::Green,
                    RollState::Diverged => Color::Red,
                    RollState::Promoted => Color::DarkGray,
                    RollState::Blocked => Color::Magenta,
                };
                let base_style = if roll.is_current {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let mut cells = vec![
                    Cell::from(roll.number.to_string()).style(base_style),
                    Cell::from(roll.branch.clone()).style(base_style),
                    Cell::from(roll.location.symbol()).style(base_style),
                    Cell::from(roll.state.label()).style(Style::default().fg(state_color)),
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
        let hint_line =
            Line::from(" [q] quit   [j/k ↑/↓] nav   [g]raduate   [p]romote   [u]pdate   [r]efresh");
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
}
