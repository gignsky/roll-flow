use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io::{self, Stdout};
use std::panic;

pub mod rolls;

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

pub fn enter() -> Result<Tui> {
    let prev = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        prev(info);
    }));
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

pub fn exit(mut terminal: Tui) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}

/// Temporarily hand the terminal back to the shell: leave the alternate screen
/// and disable raw mode so a child git/nix process (and our own prints) render
/// normally. Mirror of [`enter`], but keeps the same `Terminal` alive so
/// [`resume`] can pick back up. The process-global panic hook installed by
/// [`enter`] stays in force, so a panic while suspended still restores the
/// terminal (the extra restore it does is idempotent).
pub fn suspend(terminal: &mut Tui) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}

/// Re-enter the alternate screen and raw mode after [`suspend`], then clear so
/// the next draw repaints the whole screen.
pub fn resume(terminal: &mut Tui) -> Result<()> {
    enable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        EnableMouseCapture
    )?;
    terminal.hide_cursor()?;
    terminal.clear()?;
    Ok(())
}

/// Block until the user presses a key, used for the "press any key to continue"
/// prompt shown while suspended so op output stays on screen until the user is
/// ready. Briefly enables raw mode to capture a single keypress without
/// requiring Enter, then restores cooked mode.
pub fn wait_for_key() -> Result<()> {
    enable_raw_mode()?;
    let res = (|| -> Result<()> {
        loop {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    return Ok(());
                }
            }
        }
    })();
    let _ = disable_raw_mode();
    res
}
