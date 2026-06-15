use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode};
use ratatui::{
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Cell, Paragraph, Row, Table, TableState},
    Frame,
};

use crate::core::{
    branches::{RollInfo, RollState},
    config::Config,
};

pub struct TuiContext<'a> {
    pub config: &'a Config,
    pub current_branch: &'a str,
    pub rolls: &'a [RollInfo],
    pub show_deps: bool,
}

pub fn run(ctx: TuiContext<'_>) -> Result<()> {
    let mut terminal = super::enter()?;
    let result = run_loop(&mut terminal, &ctx);
    super::exit(terminal)?;
    result
}

fn run_loop(terminal: &mut super::Tui, ctx: &TuiContext<'_>) -> Result<()> {
    let mut state = TableState::default();
    let initial = ctx.rolls.iter().position(|r| r.is_current).unwrap_or(0);
    if !ctx.rolls.is_empty() {
        state.select(Some(initial));
    }

    loop {
        terminal.draw(|f| render(f, &mut state, ctx))?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Down | KeyCode::Char('j') => {
                        let next = state
                            .selected()
                            .map(|i| (i + 1).min(ctx.rolls.len().saturating_sub(1)))
                            .unwrap_or(0);
                        state.select(Some(next));
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        let prev = state.selected().map(|i| i.saturating_sub(1)).unwrap_or(0);
                        state.select(Some(prev));
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

fn render(f: &mut Frame, state: &mut TableState, ctx: &TuiContext<'_>) {
    let area = f.area();

    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .split(area);

    // ── Header ────────────────────────────────────────────────────────────────
    let header_line = Line::from(vec![
        Span::styled("Branch: ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(ctx.current_branch),
        Span::raw("   Rolling: "),
        Span::styled(
            ctx.config.rolling_branch.as_str(),
            Style::default().fg(Color::Cyan),
        ),
        Span::raw("   Stable: "),
        Span::styled(
            ctx.config.stable_branch.as_str(),
            Style::default().fg(Color::Green),
        ),
    ]);
    f.render_widget(
        Paragraph::new(header_line).block(Block::bordered().title(" roll-flow ")),
        chunks[0],
    );

    // ── Rolls table ───────────────────────────────────────────────────────────
    let mut col_constraints = vec![
        Constraint::Length(4),
        Constraint::Fill(1),
        Constraint::Length(3),
        Constraint::Length(13),
    ];
    if ctx.show_deps {
        col_constraints.push(Constraint::Length(8));
    }

    let mut header_cells = vec![
        Cell::from("#").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("branch").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("loc").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("state").style(Style::default().add_modifier(Modifier::BOLD)),
    ];
    if ctx.show_deps {
        header_cells.push(Cell::from("deps").style(Style::default().add_modifier(Modifier::BOLD)));
    }
    let table_header = Row::new(header_cells)
        .style(Style::default().add_modifier(Modifier::UNDERLINED))
        .height(1);

    let rows: Vec<Row> = ctx
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
            if ctx.show_deps {
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
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▶ ");

    f.render_stateful_widget(table, chunks[1], state);

    // ── Status bar ────────────────────────────────────────────────────────────
    f.render_widget(
        Paragraph::new(" [q/Esc] quit   [j/k ↑/↓] navigate"),
        chunks[2],
    );
}
