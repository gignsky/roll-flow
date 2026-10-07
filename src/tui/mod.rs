use anyhow::Result;
use crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io::{self, Stdout};
use std::panic;
use std::sync::OnceLock;

pub mod output;
pub mod rolls;

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Whether this terminal understands the kitty keyboard-enhancement protocol,
/// which is what lets `Shift+Enter` arrive distinguishable from a plain
/// `Enter` (see `tui::rolls`' `:` command runner). Queried once and cached:
/// the query itself is a synchronous round trip with the terminal, which is
/// fine to pay once at startup but not on every suspend/resume, and the
/// panic hook below must not attempt it fresh — a query mid-panic is exactly
/// the kind of thing that can hang a process that is already in trouble.
fn keyboard_enhancement_supported() -> bool {
    *KEYBOARD_ENHANCEMENT
        .get_or_init(|| crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false))
}

static KEYBOARD_ENHANCEMENT: OnceLock<bool> = OnceLock::new();

pub fn enter() -> Result<Tui> {
    let prev = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        // Cached by the time any panic can happen — `enable_raw_mode` below
        // runs, and the query, before this hook can ever fire.
        if keyboard_enhancement_supported() {
            let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
        }
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        prev(info);
    }));
    enable_raw_mode()?;
    // Querying needs raw mode already on, to read the terminal's response
    // synchronously.
    let supports_enhancement = keyboard_enhancement_supported();
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    if supports_enhancement {
        execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
    }
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

pub fn exit(mut terminal: Tui) -> Result<()> {
    disable_raw_mode()?;
    if keyboard_enhancement_supported() {
        execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags)?;
    }
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}

/// Temporarily hand the terminal back to the shell: leave the alternate screen
/// and disable raw mode so a child process renders normally. Mirror of
/// [`enter`], but keeps the same `Terminal` alive so [`resume`] can pick back
/// up. The process-global panic hook installed by [`enter`] stays in force, so a
/// panic while suspended still restores the terminal (the extra restore it does
/// is idempotent).
///
/// Ops no longer use this — their output streams into
/// [`super::tui::output::Panel`] instead. It exists for `gg` (lazygit) and for
/// the `:` command runner's "run in the real shell" key: both are full-screen
/// or interactive enough that the child needs the terminal outright.
pub fn suspend(terminal: &mut Tui) -> Result<()> {
    disable_raw_mode()?;
    if keyboard_enhancement_supported() {
        execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags)?;
    }
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
    if keyboard_enhancement_supported() {
        execute!(
            terminal.backend_mut(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
    }
    terminal.hide_cursor()?;
    terminal.clear()?;
    Ok(())
}
