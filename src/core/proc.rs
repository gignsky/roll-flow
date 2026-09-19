//! Subprocess execution with an optional per-thread output sink.
//!
//! Every child process `rf` runs used to inherit stdio: git's progress lines and
//! gate output went straight to the terminal, which is correct for the CLI and
//! impossible for the TUI. The TUI's answer used to be to tear the alternate
//! screen down around each op (`tui::suspend`); it now streams that output into
//! a floating panel instead, and this module is how the output gets there.
//!
//! The sink is a thread-local rather than a parameter threaded through every
//! signature. `core::ops` is contractually print-free — its module header spells
//! out the one exception, child-process output from gates — so the only thing
//! the panel needs to intercept is the stdio of processes spawned deep inside
//! ops. Reaching those three spawn sites by parameter would mean changing dozens
//! of signatures that have no interest in output routing.
//!
//! With no sink installed, [`run`] behaves exactly like the `Command::status()`
//! calls it replaced, so every CLI path and every integration test is unaffected.

use std::cell::RefCell;
use std::io::{BufRead, BufReader};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::Sender;
use std::thread;

use crate::error::RfError;

/// One line of child output, tagged with the stream it came from so a renderer
/// can style stderr differently. Git writes progress and `remote:` lines to
/// stderr even on success, so both streams matter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutLine {
    Out(String),
    Err(String),
}

impl OutLine {
    pub fn text(&self) -> &str {
        match self {
            OutLine::Out(s) | OutLine::Err(s) => s,
        }
    }

    pub fn is_err(&self) -> bool {
        matches!(self, OutLine::Err(_))
    }
}

thread_local! {
    /// Where this thread's child output goes, if anywhere. `None` means inherit.
    static SINK: RefCell<Option<Sender<OutLine>>> = const { RefCell::new(None) };
}

/// Install `tx` as this thread's output sink for the duration of `body`.
///
/// Restores the previous sink on the way out — including on unwind, since the
/// guard's `Drop` does the restoring — so a panicking op cannot leave a stale
/// sender behind for whatever runs on this thread next.
pub fn with_sink<T>(tx: Sender<OutLine>, body: impl FnOnce() -> T) -> T {
    let _guard = SinkGuard::install(Some(tx));
    body()
}

struct SinkGuard {
    prev: Option<Sender<OutLine>>,
}

impl SinkGuard {
    fn install(next: Option<Sender<OutLine>>) -> Self {
        let prev = SINK.with(|s| std::mem::replace(&mut *s.borrow_mut(), next));
        SinkGuard { prev }
    }
}

impl Drop for SinkGuard {
    fn drop(&mut self) {
        let prev = self.prev.take();
        SINK.with(|s| *s.borrow_mut() = prev);
    }
}

/// Run `cmd` to completion, returning its exit status.
///
/// With a sink installed, stdout and stderr are piped and forwarded line by line
/// while the child runs; without one, stdio is inherited and this is a plain
/// `status()` call.
pub fn run(cmd: &mut Command) -> Result<ExitStatus, RfError> {
    match SINK.with(|s| s.borrow().clone()) {
        Some(tx) => run_piped(cmd, vec![tx]),
        None => Ok(cmd.status()?),
    }
}

/// Run `cmd` with its output relayed to `extra` *in addition to* the installed
/// sink, if any.
///
/// Unlike [`run`], this always pipes: the caller wants a copy of the output
/// regardless of whether anything is displaying it. Used by
/// `git::run_git_capturing_stderr`, which needs git's stderr text to classify a
/// push failure while the panel still shows that same text live.
pub fn run_teed(cmd: &mut Command, extra: Sender<OutLine>) -> Result<ExitStatus, RfError> {
    let mut sinks = vec![extra];
    sinks.extend(SINK.with(|s| s.borrow().clone()));
    run_piped(cmd, sinks)
}

/// Spawn `cmd` with both streams piped and relay each line to `tx`.
///
/// One reader thread per stream: interleaving stdout and stderr in a single
/// reader would require non-blocking reads or a pty, and git splits progress
/// (stderr) from data (stdout) in a way that makes reading only one of them lose
/// exactly the output worth showing. Both senders must be dropped before the
/// receiver sees a disconnect, which is what lets a caller drain to the end.
fn run_piped(cmd: &mut Command, sinks: Vec<Sender<OutLine>>) -> Result<ExitStatus, RfError> {
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // No reader is attached to the child's stdin, so leaving it inherited
        // would let a child steal keystrokes from the TUI's own event loop.
        .stdin(Stdio::null())
        .spawn()?;

    let out = child.stdout.take().map(|pipe| {
        let sinks = sinks.clone();
        thread::spawn(move || pump(pipe, sinks, false))
    });
    let err = child.stderr.take().map(|pipe| {
        let sinks = sinks.clone();
        thread::spawn(move || pump(pipe, sinks, true))
    });
    // The local handles would otherwise keep the channels open past both
    // readers, and a caller draining to disconnect would never see the end.
    drop(sinks);

    let status = child.wait()?;
    // Join after waiting: the pipes reach EOF when the child exits, so the
    // readers are already finishing, and joining first would deadlock on a child
    // that outlives its output.
    for handle in [out, err].into_iter().flatten() {
        let _ = handle.join();
    }
    Ok(status)
}

/// Read `pipe` line by line into `tx`, stopping at EOF or once the receiver is
/// gone. Invalid UTF-8 is replaced rather than dropped — git output is normally
/// UTF-8, and a lossy line is far more useful than a silent gap.
fn pump(pipe: impl std::io::Read, mut sinks: Vec<Sender<OutLine>>, is_err: bool) {
    let mut reader = BufReader::new(pipe);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let text = String::from_utf8_lossy(&buf)
            .trim_end_matches(['\n', '\r'])
            .to_string();
        let line = if is_err {
            OutLine::Err(text)
        } else {
            OutLine::Out(text)
        };
        // A receiver that hung up is dropped rather than ending the pump: the
        // other sink may still be listening, and draining the pipe to EOF is
        // what lets the child exit instead of blocking on a full pipe buffer.
        sinks.retain(|tx| tx.send(line.clone()).is_ok());
        if sinks.is_empty() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn sh(script: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script);
        cmd
    }

    #[test]
    fn without_a_sink_stdio_is_inherited_and_the_status_still_arrives() {
        let status = run(&mut sh("exit 3")).expect("spawn");
        assert_eq!(status.code(), Some(3));
    }

    #[test]
    fn with_a_sink_both_streams_are_relayed() {
        let (tx, rx) = mpsc::channel();
        let status = with_sink(tx, || {
            run(&mut sh("echo to-stdout; echo to-stderr >&2")).expect("spawn")
        });
        assert!(status.success());

        let lines: Vec<OutLine> = rx.iter().collect();
        assert!(
            lines.contains(&OutLine::Out("to-stdout".to_string())),
            "{lines:?}"
        );
        assert!(
            lines.contains(&OutLine::Err("to-stderr".to_string())),
            "{lines:?}"
        );
    }

    #[test]
    fn the_channel_closes_once_the_command_finishes() {
        let (tx, rx) = mpsc::channel();
        with_sink(tx, || run(&mut sh("echo one")).expect("spawn"));
        // Draining to disconnect must terminate, or a caller that waits for the
        // end of output would hang forever.
        assert_eq!(rx.iter().count(), 1);
    }

    #[test]
    fn the_sink_is_restored_after_with_sink_returns() {
        // A second command on the same thread must not keep writing into the
        // first job's channel.
        let (tx, rx) = mpsc::channel();
        with_sink(tx, || run(&mut sh("echo inside")).expect("spawn"));
        run(&mut sh("echo outside")).expect("spawn");
        let seen: Vec<String> = rx.iter().map(|l| l.text().to_string()).collect();
        assert_eq!(seen, vec!["inside".to_string()]);
    }

    #[test]
    fn carriage_returns_are_trimmed_from_line_ends() {
        let (tx, rx) = mpsc::channel();
        with_sink(tx, || run(&mut sh("printf 'a\\r\\n'")).expect("spawn"));
        assert_eq!(rx.recv().unwrap(), OutLine::Out("a".to_string()));
    }
}
