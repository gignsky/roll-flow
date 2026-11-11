//! `cc` — Claude Code in a floating pane that can be minimized.
//!
//! `gg` and `bb` hand the whole terminal to a child and stop drawing until it
//! exits. That is wrong for Claude Code, which is a long conversation you want
//! to step away from to check on a job. So this one does not suspend rf: the
//! child runs on a pseudo-terminal, a `vt100` parser turns its output into a
//! screen grid, and `tui-term` draws that grid as a popup over the table. The
//! event loop keeps running either way, so a minimized Claude and rf's own jobs
//! both stay live.
//!
//! Everything here is either pure (key encoding, `PATH` lookup, geometry — the
//! parts with unit tests) or the [`Session`] that owns the child. How the pane
//! behaves inside the keymap lives in `rolls.rs`.

use anyhow::{anyhow, Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use ratatui::layout::Rect;
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// How the minimize chord is spelled in titles and hints.
pub const MINIMIZE_LABEL: &str = "ctrl-\\";

/// Whether `key` is the minimize chord — the one key rf keeps for itself while
/// the pane has focus; everything else is forwarded to Claude. `Ctrl-\` because
/// no readline binding, Claude Code shortcut or shell uses it as input (it is
/// the terminal's SIGQUIT key, which raw mode has already disarmed).
pub fn is_minimize(key: &KeyEvent) -> bool {
    // A legacy terminal sends Ctrl-\ as the single byte 0x1c, which crossterm
    // reports as Ctrl-'4' (0x1c..=0x1f map to '4'..='7'); only terminals with
    // keyboard enhancement report it as Ctrl-'\\'. Accept both spellings.
    matches!(key.code, KeyCode::Char('\\') | KeyCode::Char('4'))
        && key.modifiers.contains(KeyModifiers::CONTROL)
}

/// Whether `command` names something runnable: an existing executable file if
/// it contains a `/`, otherwise a lookup through each directory of `path`.
/// `path` is a parameter so tests need not mutate the environment.
pub fn on_path(command: &str, path: Option<&OsStr>) -> bool {
    if command.is_empty() {
        return false;
    }
    if command.contains('/') {
        return is_executable(Path::new(command));
    }
    let Some(path) = path else {
        return false;
    };
    std::env::split_paths(path).any(|dir| is_executable(&dir.join(command)))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// xterm's modifier parameter: 1 + shift(1) + alt(2) + ctrl(4).
fn modifier_param(mods: KeyModifiers) -> u8 {
    1 + u8::from(mods.contains(KeyModifiers::SHIFT))
        + 2 * u8::from(mods.contains(KeyModifiers::ALT))
        + 4 * u8::from(mods.contains(KeyModifiers::CONTROL))
}

/// `CSI <final>` plain, or `CSI 1 ; <mods> <final>` when modified.
fn csi_final(final_byte: char, mods: KeyModifiers) -> Vec<u8> {
    match modifier_param(mods) {
        1 => format!("\x1b[{final_byte}").into_bytes(),
        m => format!("\x1b[1;{m}{final_byte}").into_bytes(),
    }
}

/// `CSI <n> ~`, with the modifier parameter when modified.
fn csi_tilde(n: u8, mods: KeyModifiers) -> Vec<u8> {
    match modifier_param(mods) {
        1 => format!("\x1b[{n}~").into_bytes(),
        m => format!("\x1b[{n};{m}~").into_bytes(),
    }
}

/// The bytes a terminal would send for `key`, or `None` for a key with no
/// encoding (a bare modifier). What the child on the pseudo-terminal reads.
pub fn encode_key(key: &KeyEvent) -> Option<Vec<u8>> {
    let mods = key.modifiers;
    let alt = mods.contains(KeyModifiers::ALT);
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    let esc_if_alt = |mut bytes: Vec<u8>| {
        if alt {
            bytes.insert(0, 0x1b);
        }
        bytes
    };
    Some(match key.code {
        KeyCode::Char(c) if ctrl => {
            let code = match c.to_ascii_lowercase() {
                l @ 'a'..='z' => l as u8 - b'a' + 1,
                ' ' | '@' | '2' => 0x00,
                '[' | '3' => 0x1b,
                '\\' | '4' => 0x1c,
                ']' | '5' => 0x1d,
                '^' | '6' => 0x1e,
                '_' | '7' | '/' => 0x1f,
                '8' => 0x7f,
                _ => return None,
            };
            esc_if_alt(vec![code])
        }
        KeyCode::Char(c) => esc_if_alt(c.to_string().into_bytes()),
        // Alt-enter is the newline Claude Code understands on every terminal;
        // shift-enter has no portable encoding, so it is sent as that.
        KeyCode::Enter if alt || mods.contains(KeyModifiers::SHIFT) => vec![0x1b, b'\r'],
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => esc_if_alt(vec![b'\t']),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => esc_if_alt(vec![0x7f]),
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => csi_final('A', mods),
        KeyCode::Down => csi_final('B', mods),
        KeyCode::Right => csi_final('C', mods),
        KeyCode::Left => csi_final('D', mods),
        KeyCode::Home => csi_final('H', mods),
        KeyCode::End => csi_final('F', mods),
        KeyCode::Insert => csi_tilde(2, mods),
        KeyCode::Delete => csi_tilde(3, mods),
        KeyCode::PageUp => csi_tilde(5, mods),
        KeyCode::PageDown => csi_tilde(6, mods),
        KeyCode::F(n @ 1..=4) => {
            let final_byte = [b'P', b'Q', b'R', b'S'][usize::from(n) - 1] as char;
            match modifier_param(mods) {
                1 => format!("\x1bO{final_byte}").into_bytes(),
                m => format!("\x1b[1;{m}{final_byte}").into_bytes(),
            }
        }
        KeyCode::F(n @ 5..=12) => {
            // The gaps (16, 22) are xterm's, not a typo.
            let code = [15, 17, 18, 19, 20, 21, 23, 24][usize::from(n) - 5];
            csi_tilde(code, mods)
        }
        _ => return None,
    })
}

/// The popup's rectangle inside `area`: most of it, but leaving a margin so the
/// table behind stays visible and it reads as a window, not a screen.
pub fn popup_rect(area: Rect) -> Rect {
    let margin_x = (area.width / 20).max(1).min(area.width / 2);
    let margin_y = (area.height / 20).max(1).min(area.height / 2);
    Rect::new(
        area.x + margin_x,
        area.y + margin_y,
        area.width.saturating_sub(2 * margin_x).max(1),
        area.height.saturating_sub(2 * margin_y).max(1),
    )
}

/// `(rows, cols)` of the terminal inside the popup's border, which is what the
/// child is told its window is. Never zero: a pty of zero size is an error.
pub fn inner_size(popup: Rect) -> (u16, u16) {
    (
        popup.height.saturating_sub(2).max(1),
        popup.width.saturating_sub(2).max(1),
    )
}

/// Everything the rolls view tracks about `cc`.
#[derive(Default)]
pub struct Pane {
    /// Whether `claude_command` resolves on this machine. Decided once at
    /// startup; when false `cc` does not exist and a lone `c` creates a roll at
    /// once rather than waiting out the chord window.
    pub available: bool,
    /// The running Claude, whether or not its pane is showing.
    pub session: Option<Session>,
    /// Whether the pane is up (true) or minimized (false). Only meaningful
    /// while there is a session.
    pub open: bool,
    /// When a bare `c` was pressed, waiting to see whether it becomes `cc`.
    pub pending: Option<Instant>,
}

impl Pane {
    /// A pane for a machine where `command`'s program is or is not installed.
    pub fn detect(command: &str) -> Self {
        let program = command.split_whitespace().next().unwrap_or("");
        Self {
            available: on_path(program, std::env::var_os("PATH").as_deref()),
            ..Self::default()
        }
    }

    /// Claude is on screen and owns the keyboard.
    pub fn visible(&self) -> bool {
        self.open && self.session.is_some()
    }

    /// Claude is running, shown or not.
    pub fn running(&self) -> bool {
        self.session.is_some()
    }
}

/// A running Claude: the child, its pseudo-terminal, and the screen its output
/// has painted so far. Dropping it kills the child.
pub struct Session {
    parser: Arc<Mutex<vt100::Parser>>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    size: (u16, u16),
}

impl Session {
    /// Start `command` (split on whitespace, no shell) in `cwd`, on a
    /// pseudo-terminal of `rows` x `cols`.
    pub fn spawn(command: &str, cwd: &Path, rows: u16, cols: u16) -> Result<Self> {
        let mut words = command.split_whitespace();
        let program = words.next().ok_or_else(|| anyhow!("empty command"))?;
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("could not open a pseudo-terminal")?;

        let mut cmd = CommandBuilder::new(program);
        cmd.args(words);
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        let child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("could not run '{program}'"))?;
        // Closing our copy of the slave is what lets the reader see EOF when
        // the child exits.
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 5000)));
        let feed = Arc::clone(&parser);
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                if let Ok(mut parser) = feed.lock() {
                    parser.process(&buf[..n]);
                }
            }
        });

        Ok(Self {
            parser,
            writer,
            master: pair.master,
            child,
            size: (rows, cols),
        })
    }

    /// Tell the child its window changed. A no-op when the size is the same,
    /// so calling it every frame is free.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        if self.size == (rows, cols) || rows == 0 || cols == 0 {
            return;
        }
        let resized = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        if resized.is_ok() {
            if let Ok(mut parser) = self.parser.lock() {
                parser.screen_mut().set_size(rows, cols);
            }
            self.size = (rows, cols);
        }
    }

    /// Forward bytes to the child's stdin.
    pub fn send(&mut self, bytes: &[u8]) {
        // A failed write means the child is gone; `exited` reports that.
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    /// `Some(description)` once the child has exited.
    pub fn exited(&mut self) -> Option<String> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(status.to_string()),
            Ok(None) => None,
            Err(err) => Some(err.to_string()),
        }
    }

    /// Run `f` on the current screen. The lock is held only for the call.
    pub fn with_screen<R>(&self, f: impl FnOnce(&vt100::Screen) -> R) -> Option<R> {
        self.parser.lock().ok().map(|p| f(p.screen()))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState};

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn plain(code: KeyCode) -> KeyEvent {
        key(code, KeyModifiers::NONE)
    }

    fn enc(code: KeyCode, mods: KeyModifiers) -> Vec<u8> {
        encode_key(&key(code, mods)).expect("encodable")
    }

    #[test]
    fn minimize_is_ctrl_backslash_and_nothing_else() {
        assert!(is_minimize(&key(
            KeyCode::Char('\\'),
            KeyModifiers::CONTROL
        )));
        // What crossterm reports for the raw 0x1c byte on a legacy terminal.
        assert!(is_minimize(&key(KeyCode::Char('4'), KeyModifiers::CONTROL)));
        assert!(!is_minimize(&plain(KeyCode::Char('4'))));
        assert!(!is_minimize(&plain(KeyCode::Char('\\'))));
        assert!(!is_minimize(&key(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL
        )));
        assert!(!is_minimize(&key(KeyCode::Esc, KeyModifiers::CONTROL)));
    }

    #[test]
    fn printable_characters_encode_as_utf8() {
        assert_eq!(enc(KeyCode::Char('a'), KeyModifiers::NONE), b"a");
        assert_eq!(enc(KeyCode::Char('A'), KeyModifiers::SHIFT), b"A");
        assert_eq!(enc(KeyCode::Char('é'), KeyModifiers::NONE), "é".as_bytes());
    }

    #[test]
    fn control_letters_encode_as_c0_codes() {
        assert_eq!(enc(KeyCode::Char('c'), KeyModifiers::CONTROL), [0x03]);
        assert_eq!(enc(KeyCode::Char('A'), KeyModifiers::CONTROL), [0x01]);
        assert_eq!(enc(KeyCode::Char('d'), KeyModifiers::CONTROL), [0x04]);
        assert_eq!(enc(KeyCode::Char('['), KeyModifiers::CONTROL), [0x1b]);
        assert_eq!(enc(KeyCode::Char(' '), KeyModifiers::CONTROL), [0x00]);
    }

    #[test]
    fn alt_prefixes_an_escape() {
        assert_eq!(enc(KeyCode::Char('b'), KeyModifiers::ALT), [0x1b, b'b']);
        assert_eq!(enc(KeyCode::Backspace, KeyModifiers::ALT), [0x1b, 0x7f]);
    }

    #[test]
    fn editing_keys() {
        assert_eq!(enc(KeyCode::Enter, KeyModifiers::NONE), b"\r");
        assert_eq!(enc(KeyCode::Tab, KeyModifiers::NONE), b"\t");
        assert_eq!(enc(KeyCode::BackTab, KeyModifiers::SHIFT), b"\x1b[Z");
        assert_eq!(enc(KeyCode::Backspace, KeyModifiers::NONE), [0x7f]);
        assert_eq!(enc(KeyCode::Esc, KeyModifiers::NONE), [0x1b]);
        assert_eq!(enc(KeyCode::Delete, KeyModifiers::NONE), b"\x1b[3~");
    }

    #[test]
    fn shift_enter_is_a_newline_not_a_submit() {
        // Claude Code reads ESC+CR (alt-enter) as "newline" on every terminal,
        // where the kitty `CSI 13;2u` form only works on some.
        assert_eq!(enc(KeyCode::Enter, KeyModifiers::SHIFT), b"\x1b\r");
    }

    #[test]
    fn arrows_and_navigation() {
        assert_eq!(enc(KeyCode::Up, KeyModifiers::NONE), b"\x1b[A");
        assert_eq!(enc(KeyCode::Down, KeyModifiers::NONE), b"\x1b[B");
        assert_eq!(enc(KeyCode::Right, KeyModifiers::NONE), b"\x1b[C");
        assert_eq!(enc(KeyCode::Left, KeyModifiers::NONE), b"\x1b[D");
        assert_eq!(enc(KeyCode::Home, KeyModifiers::NONE), b"\x1b[H");
        assert_eq!(enc(KeyCode::End, KeyModifiers::NONE), b"\x1b[F");
        assert_eq!(enc(KeyCode::PageUp, KeyModifiers::NONE), b"\x1b[5~");
        assert_eq!(enc(KeyCode::PageDown, KeyModifiers::NONE), b"\x1b[6~");
    }

    #[test]
    fn modified_arrows_carry_the_xterm_modifier_parameter() {
        // 1 + shift(1) + alt(2) + ctrl(4)
        assert_eq!(enc(KeyCode::Left, KeyModifiers::CONTROL), b"\x1b[1;5D");
        assert_eq!(enc(KeyCode::Right, KeyModifiers::ALT), b"\x1b[1;3C");
        assert_eq!(enc(KeyCode::Up, KeyModifiers::SHIFT), b"\x1b[1;2A");
    }

    #[test]
    fn function_keys() {
        assert_eq!(enc(KeyCode::F(1), KeyModifiers::NONE), b"\x1bOP");
        assert_eq!(enc(KeyCode::F(4), KeyModifiers::NONE), b"\x1bOS");
        assert_eq!(enc(KeyCode::F(5), KeyModifiers::NONE), b"\x1b[15~");
        assert_eq!(enc(KeyCode::F(12), KeyModifiers::NONE), b"\x1b[24~");
    }

    #[test]
    fn a_bare_modifier_has_no_encoding() {
        assert_eq!(encode_key(&plain(KeyCode::Null)), None);
    }

    #[cfg(unix)]
    #[test]
    fn on_path_finds_executables_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("claude");
        let data = dir.path().join("notes");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        std::fs::write(&data, "x").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o644)).unwrap();

        let path = std::env::join_paths([dir.path()]).unwrap();
        assert!(on_path("claude", Some(&path)));
        assert!(!on_path("notes", Some(&path)), "not executable");
        assert!(!on_path("missing", Some(&path)));
        assert!(!on_path("claude", None), "no PATH means nothing to search");
        assert!(!on_path("", Some(&path)));
        // A command with a slash is a path, not a lookup.
        assert!(on_path(exe.to_str().unwrap(), None));
        assert!(!on_path(data.to_str().unwrap(), None));
    }

    #[test]
    fn popup_sits_inside_the_screen_with_a_margin() {
        let area = Rect::new(0, 0, 120, 40);
        let popup = popup_rect(area);
        assert!(popup.x > 0 && popup.y > 0, "margin on the top left");
        assert!(popup.right() < area.right() && popup.bottom() < area.bottom());
        assert!(popup.width > area.width / 2 && popup.height > area.height / 2);
    }

    #[test]
    fn popup_survives_a_tiny_terminal() {
        let area = Rect::new(0, 0, 6, 3);
        let popup = popup_rect(area);
        assert!(popup.right() <= area.right() && popup.bottom() <= area.bottom());
        let (rows, cols) = inner_size(popup);
        assert!(rows >= 1 && cols >= 1, "a pty cannot be zero-sized");
    }

    #[test]
    fn inner_size_excludes_the_border() {
        assert_eq!(inner_size(Rect::new(3, 2, 50, 20)), (18, 48));
    }

    /// Poll `cond` for up to three seconds; a pty round trip is asynchronous.
    #[cfg(unix)]
    fn eventually(mut cond: impl FnMut() -> bool) -> bool {
        for _ in 0..60 {
            if cond() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        false
    }

    #[cfg(unix)]
    #[test]
    fn a_session_echoes_what_it_is_sent_onto_its_screen() {
        let mut session = Session::spawn("cat", Path::new("/"), 10, 40).unwrap();
        session.send(
            b"hello
",
        );
        assert!(
            eventually(|| session
                .with_screen(|s| s.contents().contains("hello"))
                .unwrap_or(false)),
            "cat's echo never reached the screen"
        );
        assert_eq!(session.exited(), None, "cat is still waiting for input");
    }

    #[cfg(unix)]
    #[test]
    fn a_session_reports_when_its_child_exits() {
        let mut session = Session::spawn("true", Path::new("/"), 10, 40).unwrap();
        assert!(eventually(|| session.exited().is_some()));
    }

    #[cfg(unix)]
    #[test]
    fn a_session_resizes_and_runs_in_the_directory_it_was_given() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::spawn("pwd", dir.path(), 10, 200).unwrap();
        let want = dir.path().canonicalize().unwrap();
        let want = want.to_str().unwrap().to_string();
        assert!(eventually(|| session
            .with_screen(|s| s.contents().contains(&want))
            .unwrap_or(false)));
        session.resize(20, 60);
        assert_eq!(session.with_screen(|s| s.size()), Some((20, 60)));
    }

    #[test]
    fn a_missing_command_is_an_error_not_a_panic() {
        assert!(Session::spawn("definitely-not-installed-xyz", Path::new("/"), 5, 20).is_err());
        assert!(Session::spawn("   ", Path::new("/"), 5, 20).is_err());
    }
}
