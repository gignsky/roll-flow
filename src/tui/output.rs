//! Background jobs and the floating output panel they stream into.
//!
//! Every mutating action in the rolls view used to suspend the whole TUI, let
//! git write to the real terminal, and wait for a keypress before redrawing.
//! That is a heavy interruption for a two-second command, so instead each action
//! now runs on a worker thread with a [`crate::core::proc`] sink installed, and
//! its output lands in a [`Panel`] anchored to the bottom-right of the table
//! while the table itself stays live and navigable.
//!
//! The panel is deliberately *not* a `Mode`: modes route all input to whatever
//! overlay is open, and the whole point here is that `j`/`k` keep working while
//! a push runs. It is an independent piece of state that outlives the job whose
//! output it holds.

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

use anyhow::Result;
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph},
    Frame,
};

use crate::core::proc::{self, OutLine};

/// How many lines of output a panel retains. Enough to hold a verbose gate run
/// (`cargo test` output) without growing without bound over a long session.
const SCROLLBACK: usize = 500;

/// Braille spinner frames, advanced once per draw while a job runs.
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// What a finished job asks the view to do next, beyond the reload every job
/// triggers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Followup {
    /// Move the selection to this branch if the reload turned it up — used after
    /// creating a roll.
    SelectBranch(String),
    /// The push was rejected as non-fast-forward. Ask whether to force it.
    OfferForcePush { branch: String, remote: String },
}

/// A successful job's result: lines to append to the panel, plus an optional
/// follow-up.
#[derive(Debug, Default)]
pub struct JobDone {
    pub lines: Vec<String>,
    pub next: Option<Followup>,
}

impl JobDone {
    pub fn lines(lines: Vec<String>) -> Self {
        JobDone { lines, next: None }
    }

    pub fn with_next(lines: Vec<String>, next: Followup) -> Self {
        JobDone {
            lines,
            next: Some(next),
        }
    }
}

/// A job running on a worker thread.
///
/// Two channels rather than one tagged stream: `lines` is the sink the
/// subprocess machinery already speaks, and `done` carries the typed result.
/// Their ordering is what makes draining safe — `with_sink` drops the line
/// sender before the worker sends its result, so by the time `done` yields, the
/// line channel is already disconnected and a drain-to-end always terminates.
pub struct Job {
    lines: Receiver<OutLine>,
    done: Receiver<Result<JobDone>>,
}

/// What draining a job produced this tick.
pub enum JobProgress {
    /// Still running; any output received has been appended to the panel.
    Running,
    /// Finished. Carries the follow-up, if the job asked for one.
    Finished(Option<Followup>),
}

impl Job {
    /// Run `body` on a worker thread with its child output routed to this job.
    pub fn spawn(body: impl FnOnce() -> Result<JobDone> + Send + 'static) -> Job {
        let (tx_line, lines) = mpsc::channel();
        let (tx_done, done) = mpsc::channel();
        thread::spawn(move || {
            let result = proc::with_sink(tx_line, body);
            // A send failure means the view is gone (the user quit); there is
            // nothing to report it to, so drop it.
            let _ = tx_done.send(result);
        });
        Job { lines, done }
    }

    /// Move whatever the worker has produced into `panel`, and report whether
    /// the job is finished.
    ///
    /// Called once per event-loop tick. The loop already polls at 50 ms, so this
    /// needs no timing of its own.
    pub fn drain(&mut self, panel: &mut Panel) -> JobProgress {
        while let Ok(line) = self.lines.try_recv() {
            panel.push_out(line);
        }
        match self.done.try_recv() {
            Err(TryRecvError::Empty) => JobProgress::Running,
            // The worker died without sending — a panic in the body. Treat it as
            // a failure rather than hanging the view on a job that will never
            // report.
            Err(TryRecvError::Disconnected) => {
                panel.finish_failed(vec!["the operation panicked".to_string()]);
                JobProgress::Finished(None)
            }
            Ok(result) => {
                // Both senders are dropped by now, so this terminates at
                // disconnect and cannot lose the tail of the output.
                for line in self.lines.iter() {
                    panel.push_out(line);
                }
                match result {
                    Ok(done) => {
                        panel.finish_ok(done.lines);
                        JobProgress::Finished(done.next)
                    }
                    Err(err) => {
                        panel.finish_failed(error_lines(&err));
                        JobProgress::Finished(None)
                    }
                }
            }
        }
    }
}

/// Flatten an `anyhow` chain into displayable lines, matching the shape the
/// suspended execution path used to print.
pub fn error_lines(err: &anyhow::Error) -> Vec<String> {
    let mut lines = vec![format!("Error: {err}")];
    for cause in err.chain().skip(1) {
        lines.push(format!("  caused by: {cause}"));
    }
    lines
}

/// Where a job got to, which is what colours the panel's border.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Running,
    Ok,
    Failed,
}

/// One retained line, tagged so stderr, results and errors can be styled apart.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    Stdout,
    Stderr,
    Result,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PanelLine {
    kind: Kind,
    text: String,
}

/// The floating log. Survives the job that filled it, so the user can read a
/// result at leisure and dismiss it with `esc`.
pub struct Panel {
    /// Border title, e.g. `"git push"`.
    pub title: String,
    lines: VecDeque<PanelLine>,
    status: Status,
    /// Lines scrolled up from the tail. 0 means the tail is visible.
    scroll: usize,
    /// While set, new output keeps the view pinned to the tail.
    autoscroll: bool,
    tick: usize,
}

impl Panel {
    pub fn new(title: impl Into<String>) -> Self {
        Panel {
            title: title.into(),
            lines: VecDeque::new(),
            status: Status::Running,
            scroll: 0,
            autoscroll: true,
            tick: 0,
        }
    }

    /// Read only by tests today; the renderer reaches `self.status` directly.
    #[cfg(test)]
    pub fn status(&self) -> Status {
        self.status
    }

    pub fn is_running(&self) -> bool {
        self.status == Status::Running
    }

    fn push(&mut self, kind: Kind, text: String) {
        if self.lines.len() == SCROLLBACK {
            self.lines.pop_front();
        }
        self.lines.push_back(PanelLine { kind, text });

        if self.autoscroll {
            self.scroll = 0;
            return;
        }
        // `scroll` is an offset from the tail, so every line appended below the
        // window would otherwise slide the visible text down by one — output
        // arriving while the user reads scrollback must not move what they are
        // reading. Growing the offset in step keeps the window over the same
        // lines. Dropping the oldest line needs no adjustment of its own: it
        // shortens the buffer and shifts every index down together.
        self.scroll = (self.scroll + 1).min(self.lines.len().saturating_sub(1));
    }

    fn push_out(&mut self, line: OutLine) {
        let kind = if line.is_err() {
            Kind::Stderr
        } else {
            Kind::Stdout
        };
        self.push(kind, line.text().to_string());
    }

    fn finish_ok(&mut self, lines: Vec<String>) {
        for line in lines {
            self.push(Kind::Result, line);
        }
        self.status = Status::Ok;
    }

    fn finish_failed(&mut self, lines: Vec<String>) {
        for line in lines {
            self.push(Kind::Error, line);
        }
        self.status = Status::Failed;
    }

    /// Mark the panel failed with `lines`, for failures decided outside a job
    /// (a command we refused to start, a missing binary).
    pub fn fail(&mut self, lines: Vec<String>) {
        self.finish_failed(lines);
    }

    /// Advance the spinner. Called once per draw.
    pub fn tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
    }

    /// Scroll `n` lines toward the start, pinning to the tail off.
    pub fn scroll_up(&mut self, n: usize) {
        self.autoscroll = false;
        let max = self.lines.len().saturating_sub(1);
        self.scroll = (self.scroll + n).min(max);
    }

    /// Scroll `n` lines toward the end, re-arming autoscroll at the tail.
    pub fn scroll_down(&mut self, n: usize) {
        self.scroll = self.scroll.saturating_sub(n);
        if self.scroll == 0 {
            self.autoscroll = true;
        }
    }

    /// Jump to the tail and resume following new output.
    pub fn scroll_to_end(&mut self) {
        self.scroll = 0;
        self.autoscroll = true;
    }

    /// The slice of lines that fits in `height` rows, honouring the scroll
    /// position. Pure, so the windowing is testable without a terminal.
    fn window(&self, height: usize) -> impl Iterator<Item = &PanelLine> {
        let end = self.lines.len().saturating_sub(self.scroll);
        let start = end.saturating_sub(height);
        self.lines.iter().skip(start).take(end - start)
    }
}

/// Border colour and title suffix for a status. Split out so the mapping is
/// testable and stays consistent between the two things it drives.
fn status_style(status: Status, spinner: char) -> (Color, String) {
    match status {
        Status::Running => (Color::Yellow, format!(" {spinner} ")),
        Status::Ok => (Color::Green, " ✓ ".to_string()),
        Status::Failed => (Color::Red, " ✗ ".to_string()),
    }
}

/// Anchor a panel to the bottom-right of `area`, inset by one cell, sized to hold
/// `content_lines` up to a cap.
///
/// Deliberately not `centered_rect`: a centred overlay covers the selected row,
/// which is the one thing that has to stay visible while a job runs on it. The
/// caller passes the *table* area rather than the whole frame, so the panel never
/// hides the key hints either.
///
/// The height follows the content so a one-line result is a three-row box rather
/// than a mostly-empty one, and only grows to the cap once there is output to
/// fill it.
pub fn panel_rect(area: Rect, content_lines: usize) -> Rect {
    let wanted = u16::try_from(content_lines)
        .unwrap_or(u16::MAX)
        .saturating_add(2);
    let width = area.width.saturating_sub(2).clamp(24, 60);
    let height = wanted.clamp(3, area.height.saturating_sub(2).clamp(3, 12));
    // A frame too small for even the clamped minimum gets whatever is left; the
    // widget clips rather than panicking.
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(width + 1),
        y: area.y + area.height.saturating_sub(height + 1),
        width,
        height,
    }
}

/// Draw `panel` over the bottom-right of `area`.
pub fn render(f: &mut Frame, area: Rect, panel: &Panel) {
    let rect = panel_rect(area, panel.lines.len());
    let spinner = SPINNER[(panel.tick / 3) % SPINNER.len()];
    let (color, mark) = status_style(panel.status, spinner);

    let body_height = rect.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = panel
        .window(body_height)
        .map(|line| {
            let style = match line.kind {
                Kind::Stdout => Style::default(),
                Kind::Stderr => Style::default().fg(Color::Gray),
                Kind::Result => Style::default().fg(Color::Green),
                Kind::Error => Style::default().fg(Color::Red),
            };
            Line::from(Span::styled(line.text.clone(), style))
        })
        .collect();

    let hint = if panel.is_running() {
        String::new()
    } else {
        " [esc] close ".to_string()
    };
    let block = Block::bordered()
        .border_style(Style::default().fg(color))
        .title(Span::styled(
            format!("{mark}{} ", panel.title),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Span::styled(hint, Style::default().fg(Color::DarkGray)));

    f.render_widget(Clear, rect);
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn panel_with(n: usize) -> Panel {
        let mut p = Panel::new("git push");
        for i in 0..n {
            p.push(Kind::Stdout, format!("line {i}"));
        }
        p
    }

    fn texts(panel: &Panel, height: usize) -> Vec<String> {
        panel.window(height).map(|l| l.text.clone()).collect()
    }

    #[test]
    fn the_window_shows_the_tail_by_default() {
        let p = panel_with(10);
        assert_eq!(texts(&p, 3), vec!["line 7", "line 8", "line 9"]);
    }

    #[test]
    fn fewer_lines_than_the_window_shows_them_all() {
        let p = panel_with(2);
        assert_eq!(texts(&p, 5), vec!["line 0", "line 1"]);
    }

    #[test]
    fn scrolling_up_moves_the_window_back_and_stops_following() {
        let mut p = panel_with(10);
        p.scroll_up(2);
        assert_eq!(texts(&p, 3), vec!["line 5", "line 6", "line 7"]);
        // New output must not yank the view back to the tail while scrolled up.
        p.push(Kind::Stdout, "line 10".to_string());
        assert_eq!(texts(&p, 3), vec!["line 5", "line 6", "line 7"]);
    }

    #[test]
    fn scrolling_back_to_the_tail_resumes_following() {
        let mut p = panel_with(10);
        p.scroll_up(4);
        p.scroll_down(4);
        p.push(Kind::Stdout, "line 10".to_string());
        assert_eq!(texts(&p, 1), vec!["line 10"]);
    }

    #[test]
    fn scroll_up_cannot_run_past_the_start() {
        let mut p = panel_with(3);
        p.scroll_up(99);
        assert_eq!(texts(&p, 3), vec!["line 0"]);
    }

    #[test]
    fn end_jumps_to_the_tail_from_anywhere() {
        let mut p = panel_with(10);
        p.scroll_up(5);
        p.scroll_to_end();
        assert_eq!(texts(&p, 2), vec!["line 8", "line 9"]);
    }

    #[test]
    fn scrollback_is_capped_and_drops_the_oldest_lines() {
        let p = panel_with(SCROLLBACK + 5);
        assert_eq!(p.lines.len(), SCROLLBACK);
        assert_eq!(p.lines.front().unwrap().text, "line 5");
    }

    #[test]
    fn dropping_an_old_line_keeps_a_scrolled_reader_on_the_same_text() {
        let mut p = panel_with(SCROLLBACK);
        p.scroll_up(10);
        let before = texts(&p, 1);
        p.push(Kind::Stdout, "overflow".to_string());
        assert_eq!(texts(&p, 1), before);
    }

    #[test]
    fn status_drives_both_the_colour_and_the_mark() {
        assert_eq!(status_style(Status::Ok, '⠋'), (Color::Green, " ✓ ".into()));
        assert_eq!(
            status_style(Status::Failed, '⠋'),
            (Color::Red, " ✗ ".into())
        );
        assert_eq!(
            status_style(Status::Running, '⠹'),
            (Color::Yellow, " ⠹ ".into())
        );
    }

    #[test]
    fn the_panel_sits_in_the_bottom_right_inset_by_one() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 40,
        };
        let r = panel_rect(area, 50);
        assert_eq!(r.width, 60, "clamped to the maximum width");
        assert_eq!(r.height, 12, "clamped to the maximum height");
        assert_eq!(r.x + r.width, 99, "one cell of right margin");
        assert_eq!(r.y + r.height, 39, "one cell of bottom margin");
    }

    #[test]
    fn the_panel_shrinks_to_its_content_and_stays_anchored() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 40,
        };
        // One line of output should not draw a mostly-empty twelve-row box.
        let one = panel_rect(area, 1);
        assert_eq!(one.height, 3, "one line plus two borders");
        assert_eq!(one.y + one.height, 39, "still hugs the bottom");
        // An empty panel still needs a border and a row to draw into.
        assert_eq!(panel_rect(area, 0).height, 3);
        assert!(panel_rect(area, 4).height < panel_rect(area, 8).height);
    }

    #[test]
    fn a_tiny_area_yields_a_rect_that_still_fits_inside_it() {
        // A panel wider than its frame would panic inside ratatui rather than
        // clipping, so the clamp has to hold at every size.
        for (w, h) in [(10u16, 4u16), (1, 1), (24, 6), (0, 0), (100, 2)] {
            let area = Rect {
                x: 2,
                y: 3,
                width: w,
                height: h,
            };
            let r = panel_rect(area, 40);
            assert!(r.width <= area.width, "{w}x{h} width {}", r.width);
            assert!(r.height <= area.height, "{w}x{h} height {}", r.height);
            assert!(
                r.x + r.width <= area.x + area.width,
                "{w}x{h} overflows right"
            );
            assert!(
                r.y + r.height <= area.y + area.height,
                "{w}x{h} overflows bottom"
            );
        }
    }

    #[test]
    fn a_finished_job_appends_its_result_lines_and_flips_the_status() {
        let mut p = panel_with(1);
        p.finish_ok(vec!["Pushed roll/1".to_string()]);
        assert_eq!(p.status(), Status::Ok);
        assert!(!p.is_running());
        assert_eq!(p.lines.back().unwrap().text, "Pushed roll/1");
        assert_eq!(p.lines.back().unwrap().kind, Kind::Result);
    }

    #[test]
    fn error_lines_flatten_the_whole_anyhow_chain() {
        let err = anyhow::anyhow!("root cause")
            .context("middle")
            .context("top");
        let lines = error_lines(&err);
        assert_eq!(lines[0], "Error: top");
        assert_eq!(lines[1], "  caused by: middle");
        assert_eq!(lines[2], "  caused by: root cause");
    }

    #[test]
    fn a_job_streams_output_then_reports_its_result() {
        let mut panel = Panel::new("test");
        let mut job = Job::spawn(|| {
            let mut cmd = std::process::Command::new("sh");
            cmd.arg("-c").arg("echo streamed; echo noise >&2");
            proc::run(&mut cmd)?;
            Ok(JobDone::lines(vec!["done".to_string()]))
        });

        let next = loop {
            match job.drain(&mut panel) {
                JobProgress::Running => std::thread::yield_now(),
                JobProgress::Finished(next) => break next,
            }
        };

        assert_eq!(next, None);
        assert_eq!(panel.status(), Status::Ok);
        let all: Vec<String> = panel.lines.iter().map(|l| l.text.clone()).collect();
        assert!(all.contains(&"streamed".to_string()), "{all:?}");
        assert!(
            all.contains(&"noise".to_string()),
            "stderr missing: {all:?}"
        );
        assert!(all.contains(&"done".to_string()), "{all:?}");
    }

    #[test]
    fn a_failing_job_lands_as_a_failed_panel_carrying_the_error() {
        let mut panel = Panel::new("test");
        let mut job = Job::spawn(|| anyhow::bail!("nope"));
        loop {
            if let JobProgress::Finished(_) = job.drain(&mut panel) {
                break;
            }
            std::thread::yield_now();
        }
        assert_eq!(panel.status(), Status::Failed);
        assert!(panel.lines.back().unwrap().text.contains("nope"));
    }

    #[test]
    fn a_job_that_panics_finishes_rather_than_hanging_the_view() {
        let mut panel = Panel::new("test");
        let mut job = Job::spawn(|| panic!("boom"));
        // Without the Disconnected arm this loop would never terminate, and the
        // view would refuse every mutating key forever.
        loop {
            if let JobProgress::Finished(_) = job.drain(&mut panel) {
                break;
            }
            std::thread::yield_now();
        }
        assert_eq!(panel.status(), Status::Failed);
    }

    #[test]
    fn a_followup_is_handed_back_to_the_caller() {
        let mut panel = Panel::new("test");
        let mut job = Job::spawn(|| {
            Ok(JobDone::with_next(
                vec!["rejected".to_string()],
                Followup::OfferForcePush {
                    branch: "roll/1".to_string(),
                    remote: "origin".to_string(),
                },
            ))
        });
        let next = loop {
            if let JobProgress::Finished(next) = job.drain(&mut panel) {
                break next;
            }
            std::thread::yield_now();
        };
        assert_eq!(
            next,
            Some(Followup::OfferForcePush {
                branch: "roll/1".to_string(),
                remote: "origin".to_string()
            })
        );
    }
}
