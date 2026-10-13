//! Commands waiting their turn behind the one that is running.
//!
//! The rolls view runs one git command at a time — two racing on the same repo
//! is never what anyone wants — but refusing a second keypress outright made
//! the user wait and remember. Instead a mutating key pressed mid-job lands
//! here, and the view starts each entry as the one ahead of it finishes.
//!
//! A failure *holds* the queue rather than draining or discarding it: the
//! commands behind a failed one were chosen on the assumption it would succeed,
//! so running them anyway is wrong, and throwing them away loses the user's
//! plan. A held queue keeps its entries until the user resolves whatever went
//! wrong (typically a merge conflict, in lazygit via `gg`) and resumes it, or
//! drops it.
//!
//! Pure bookkeeping: this module never spawns anything. Starting a job and
//! drawing the result stay in `tui::rolls` and `tui::output`.

use std::collections::VecDeque;

use anyhow::Result;

use super::output::JobDone;

/// The work a queued command will do, boxed so commands of different shapes
/// can wait in one queue.
pub type JobBody = Box<dyn FnOnce() -> Result<JobDone> + Send + 'static>;

/// One command waiting to run.
pub struct Pending {
    /// The panel title it will run under, e.g. `"git push roll/3-x"`.
    pub title: String,
    /// Whether running it can change the checked-out branch. Keys that read
    /// HEAD at keypress time are refused while one of these is pending, since
    /// the branch they captured would not be the one they act on.
    pub moves_head: bool,
    pub body: JobBody,
}

impl Pending {
    pub fn new(title: impl Into<String>, moves_head: bool, body: JobBody) -> Self {
        Pending {
            title: title.into(),
            moves_head,
            body,
        }
    }
}

/// The commands queued behind the running one, in the order they will run.
#[derive(Default)]
pub struct Queue {
    items: VecDeque<Pending>,
    /// Why the queue stopped, while it is stopped. Never set on an empty queue:
    /// there is nothing to hold.
    held: Option<String>,
}

impl Queue {
    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Append `pending` to the back.
    pub fn push(&mut self, pending: Pending) {
        self.items.push_back(pending);
    }

    /// Put `pending` at the front, ahead of everything already waiting. For a
    /// command that answers the very thing that held the queue — a confirmed
    /// force push after a rejected one — and so belongs before what was
    /// waiting on it.
    pub fn push_front(&mut self, pending: Pending) {
        self.items.push_front(pending);
    }

    /// Stop the queue, recording why. A no-op on an empty queue, so a failure
    /// with nothing behind it leaves no stale hold for the next command to
    /// trip over.
    pub fn hold(&mut self, reason: impl Into<String>) {
        if !self.items.is_empty() {
            self.held = Some(reason.into());
        }
    }

    /// Why the queue is held, or `None` while it is free to run.
    pub fn held(&self) -> Option<&str> {
        self.held.as_deref()
    }

    /// Release a hold. Returns false when there was none to release.
    pub fn resume(&mut self) -> bool {
        self.held.take().is_some()
    }

    /// Take the next command to run, or `None` when the queue is empty or held.
    pub fn next(&mut self) -> Option<Pending> {
        if self.held.is_some() {
            return None;
        }
        self.items.pop_front()
    }

    /// Drop every waiting command, returning how many there were. Also clears
    /// a hold, which means nothing without entries.
    pub fn clear(&mut self) -> usize {
        self.held = None;
        let n = self.items.len();
        self.items.clear();
        n
    }

    /// The title of the first waiting command that can move HEAD, if any.
    pub fn first_head_mover(&self) -> Option<&str> {
        self.items
            .iter()
            .find(|p| p.moves_head)
            .map(|p| p.title.as_str())
    }

    /// Titles in run order.
    pub fn titles(&self) -> impl Iterator<Item = &str> {
        self.items.iter().map(|p| p.title.as_str())
    }
}

/// The one-line summary of a non-empty queue, for the status bar. `None` when
/// nothing is waiting.
///
/// Lives on the status bar rather than only in the output panel because the
/// panel can be dismissed with `esc`, and a held queue must stay visible after
/// that — it is waiting on the user, and an invisible wait is a trap.
pub fn summary(queue: &Queue) -> Option<String> {
    if queue.is_empty() {
        return None;
    }
    let n = queue.len();
    let noun = if n == 1 { "command" } else { "commands" };
    let order = queue.titles().collect::<Vec<_>>().join(" → ");
    Some(match queue.held() {
        Some(reason) => format!(
            "⏸ {n} queued {noun} held ({reason}) — resolve it (gg opens lazygit), then [s] resume · [S] drop: {order}"
        ),
        None => format!("⏳ {n} queued: {order}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(title: &str) -> Pending {
        Pending::new(title, false, Box::new(|| Ok(JobDone::default())))
    }

    fn head_job(title: &str) -> Pending {
        Pending::new(title, true, Box::new(|| Ok(JobDone::default())))
    }

    fn titles(q: &Queue) -> Vec<String> {
        q.titles().map(str::to_string).collect()
    }

    #[test]
    fn commands_run_in_the_order_they_were_queued() {
        let mut q = Queue::default();
        q.push(job("a"));
        q.push(job("b"));
        assert_eq!(q.next().unwrap().title, "a");
        assert_eq!(q.next().unwrap().title, "b");
        assert!(q.next().is_none());
    }

    #[test]
    fn a_held_queue_keeps_its_commands_until_resumed() {
        let mut q = Queue::default();
        q.push(job("a"));
        q.push(job("b"));
        q.hold("'x' failed");
        assert!(q.next().is_none(), "a held queue must not hand out work");
        assert_eq!(titles(&q), vec!["a", "b"], "nothing is lost while held");
        assert!(q.resume());
        assert_eq!(q.next().unwrap().title, "a");
    }

    #[test]
    fn holding_an_empty_queue_leaves_no_hold_behind() {
        let mut q = Queue::default();
        q.hold("'x' failed");
        assert_eq!(q.held(), None);
        // So the next command queued is not silently stuck behind it.
        q.push(job("a"));
        assert_eq!(q.next().unwrap().title, "a");
    }

    #[test]
    fn resume_reports_whether_there_was_a_hold() {
        let mut q = Queue::default();
        q.push(job("a"));
        assert!(!q.resume());
        q.hold("why");
        assert!(q.resume());
        assert!(!q.resume());
    }

    #[test]
    fn push_front_jumps_ahead_of_what_was_waiting() {
        let mut q = Queue::default();
        q.push(job("later"));
        q.push_front(job("now"));
        assert_eq!(titles(&q), vec!["now", "later"]);
    }

    #[test]
    fn clear_drops_everything_and_the_hold() {
        let mut q = Queue::default();
        q.push(job("a"));
        q.push(job("b"));
        q.hold("why");
        assert_eq!(q.clear(), 2);
        assert!(q.is_empty());
        assert_eq!(q.held(), None);
    }

    #[test]
    fn first_head_mover_finds_a_switch_anywhere_in_line() {
        let mut q = Queue::default();
        q.push(job("git fetch --prune"));
        assert_eq!(q.first_head_mover(), None);
        q.push(head_job("git switch main"));
        q.push(head_job("rf create"));
        assert_eq!(q.first_head_mover(), Some("git switch main"));
    }

    #[test]
    fn the_summary_names_every_command_in_order() {
        let mut q = Queue::default();
        assert_eq!(summary(&q), None);
        q.push(job("git fetch --prune"));
        q.push(job("rf create"));
        assert_eq!(
            summary(&q).unwrap(),
            "⏳ 2 queued: git fetch --prune → rf create"
        );
    }

    #[test]
    fn a_held_summary_says_why_and_how_to_continue() {
        let mut q = Queue::default();
        q.push(job("rf create"));
        q.hold("'rf integrate' failed");
        let s = summary(&q).unwrap();
        assert!(s.contains("1 queued command held"), "{s}");
        assert!(s.contains("'rf integrate' failed"), "{s}");
        assert!(s.contains("[s] resume"), "{s}");
        assert!(s.contains("[S] drop"), "{s}");
        assert!(s.ends_with("rf create"), "{s}");
    }
}
