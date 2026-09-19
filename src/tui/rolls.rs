//! Interactive rolls view for `rf status` / `rf list`.
//!
//! Beyond navigation this drives workflow operations on the selected roll
//! (issues #20/#21): action keys open a confirmation modal, and on confirm the
//! op runs on a worker thread whose child output streams into a floating
//! [`super::output::Panel`], then the roll list reloads in place. The view stays
//! drawn and navigable throughout — an op no longer tears the screen down and
//! waits for a keypress to give it back.
//!
//! It also carries lazygit's sync keys: `[p]` pull, `[P]` push, `[f]` fetch, and
//! `gg` to hand the terminal to lazygit itself. The keymap is deliberately
//! lazygit's rather than one of our own — `[G]raduate` and `[m] promote` moved
//! aside to make room for it.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Cell, Clear, Paragraph, Row, Table, TableState},
    Frame,
};

use super::output::{self, Followup, JobDone, JobProgress};
use crate::core::{
    branches::{self, BranchLocation, RollInfo, RollState},
    config::Config,
    git::{self, TrackState},
    ops,
    sync::{self, PullPlan, PushOutcome, SyncTarget},
    version::{self, BumpLevel, Semver, VersionCheck, VersionStatus},
};

/// A workflow operation reachable from the view. Navigation, quit and refresh
/// are handled directly; only these mutating ops go through the confirm modal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Action {
    /// Graduate the selected roll into the rolling branch.
    Graduate,
    /// Merge the *selected* roll into the *checked-out* one. The only action here
    /// whose subject and object are different rows: everything else acts on the
    /// row under the cursor, this one acts on HEAD using that row as the source.
    Integrate,
    /// Promote the rolling branch into stable.
    Promote,
    /// Update all active local rolls from stable.
    Update,
    /// Delete every promoted roll branch, locally and on origin.
    Prune,
    /// Delete the local copy of every graduated or promoted roll branch,
    /// leaving `origin` untouched.
    Tidy,
}

impl Action {
    /// Panel title for this action, naming the roll when one is targeted so two
    /// consecutive graduations are distinguishable in the log.
    fn job_title(&self, target: Option<&str>) -> String {
        let verb = match self {
            Action::Graduate => "graduate",
            Action::Integrate => "integrate",
            Action::Promote => "promote",
            Action::Update => "update",
            Action::Prune => "prune",
            Action::Tidy => "tidy",
        };
        match target {
            Some(t) => format!("rf {verb} {t}"),
            None => format!("rf {verb}"),
        }
    }
}

/// How far `PageUp`/`PageDown` move the output panel. A fixed step rather than a
/// page: the panel's height is a render-time detail the key handler does not see,
/// and a step that overshoots a short panel reads as a jump to the start.
const PANEL_SCROLL_STEP: usize = 5;

/// Which way a scroll key moves the output panel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PanelScroll {
    Up,
    Down,
    End,
}

/// What a keystroke in the force-push confirmation asks for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ForcePushOutcome {
    /// Unbound here — stay open, change nothing.
    Ignore,
    Cancel,
    Force,
}

/// What a keystroke in the bump modal asks for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BumpOutcome {
    Ignore,
    Cancel,
    Level(BumpLevel),
}

/// Decide a keystroke in the bump modal.
///
/// Digits rather than initials, which is a break from the house style used by the
/// delete modal (`l`/`r`/`b`). The three level names give `p`, `m` and `M` as
/// initials, and distinguishing minor from major by the shift key alone — for
/// choices an order of magnitude apart — is a mistake waiting to happen. Digits
/// also carry the ordering, so `1`/`2`/`3` reads as least-to-most significant.
pub(crate) fn bump_key(code: KeyCode) -> BumpOutcome {
    match code {
        KeyCode::Char('1') => BumpOutcome::Level(BumpLevel::Patch),
        KeyCode::Char('2') => BumpOutcome::Level(BumpLevel::Minor),
        KeyCode::Char('3') => BumpOutcome::Level(BumpLevel::Major),
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => BumpOutcome::Cancel,
        _ => BumpOutcome::Ignore,
    }
}

/// The three levels and what each would produce, in the order the modal lists
/// them. Shared by the renderer and its tests so the preview can never disagree
/// with what a keypress actually applies.
pub(crate) fn bump_previews(current: Semver) -> [(BumpLevel, Semver); 3] {
    [
        (BumpLevel::Patch, current.bump(BumpLevel::Patch)),
        (BumpLevel::Minor, current.bump(BumpLevel::Minor)),
        (BumpLevel::Major, current.bump(BumpLevel::Major)),
    ]
}

/// Whether `[b]` can do anything here, or why not.
///
/// A repo with no `Cargo.toml` has no version to raise — the same condition that
/// makes the whole release path a no-op (`VersionStatus::NotApplicable`), so
/// refusing here keeps the key honest rather than opening a modal over nothing.
pub(crate) fn bump_gate(current: Option<Semver>) -> Result<Semver, String> {
    current.ok_or_else(|| "no readable version in Cargo.toml — nothing to bump".to_string())
}

/// Decide a keystroke in the force-push confirmation.
///
/// Deliberately narrow: only an explicit `y` forces. `Enter` is *not* bound,
/// because this modal can open unprompted the moment `[P]` lands on a branch
/// that is behind, and a stray Enter must never overwrite a remote.
pub(crate) fn force_push_key(code: KeyCode) -> ForcePushOutcome {
    match code {
        KeyCode::Char('y') | KeyCode::Char('Y') => ForcePushOutcome::Force,
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => ForcePushOutcome::Cancel,
        _ => ForcePushOutcome::Ignore,
    }
}

/// Turn a [`sync::SyncFailure`] into an `anyhow` error carrying git's own text
/// and, when we have one, a line of advice on what to do about it.
fn sync_error(err: sync::SyncFailure) -> anyhow::Error {
    match &err.hint {
        Some(hint) => anyhow!("{}\n{hint}", err.failure),
        None => anyhow!("{}", err.failure),
    }
}

/// The `sync` column's glyph and colour for one branch.
///
/// `track` is `None` when the branch has no local copy, which is not the same as
/// having no upstream — a remote-only roll has nothing to compare, so it shows a
/// dash rather than claiming to be in sync. Pure, so the whole table is testable
/// without a terminal.
pub(crate) fn sync_cell(track: Option<TrackState>) -> (String, Color) {
    match track {
        None | Some(TrackState::NoUpstream) => ("—".to_string(), Color::DarkGray),
        Some(TrackState::Gone) => ("gone".to_string(), Color::Red),
        Some(TrackState::InSync) => ("✓".to_string(), Color::Green),
        Some(TrackState::Ahead(n)) => (format!("↑{n}"), Color::Yellow),
        Some(TrackState::Behind(n)) => (format!("↓{n}"), Color::Yellow),
        Some(TrackState::Diverged { ahead, behind }) => {
            (format!("↑{ahead}↓{behind}"), Color::Yellow)
        }
    }
}

/// The marker shown against the checked-out branch.
///
/// Distinct from the table's selection cursor (`▶`), which follows the keyboard:
/// these answer different questions and both have to be readable at once.
pub(crate) fn current_marker(is_current: bool) -> &'static str {
    if is_current {
        "›"
    } else {
        ""
    }
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
    /// Destructive per-row branch deletion. Deliberately not a `Confirm`: the
    /// prompt shape depends on where the branch exists, and the decision
    /// produces a *scope* (which copies), neither of which `Action` can carry.
    Delete {
        preview: DeletePreview,
    },
    /// Pick a semver level to raise. Holds the version read when the modal
    /// opened, so the previews it shows and the bump it applies agree.
    Bump {
        current: Semver,
    },
    /// `?`: the searchable keymap. Holds the filter text and the cursor's
    /// position in the *filtered* list, which is why editing the query resets
    /// it — see [`handle_help_key`].
    Help {
        query: String,
        cursor: usize,
    },
    /// A push was refused as non-fast-forward (or is already known to be behind
    /// its upstream). Asks whether to force it.
    ///
    /// Its own variant rather than a `Confirm` action: the answer carries the
    /// branch and remote to retry against, and forcing is destructive enough
    /// that it should not share a code path with the routine confirmations.
    ForcePush {
        branch: String,
        remote: String,
    },
}

/// Which copies of a roll branch a `[d]elete` targets.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DeleteScope {
    Local,
    Remote,
    Both,
}

/// The shape of the delete modal, decided from the roll's location when the
/// modal opens and advanced by the user's answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DeletePrompt {
    /// Exactly one copy is deletable — a y/N confirmation defaulting to No.
    Single(DeleteScope),
    /// Both copies exist — local / origin / both / neither.
    Choice,
    /// Second stage: the chosen scope touches a copy holding commits stable
    /// lacks. y/N again, now stating what would be lost.
    ForceConfirm(DeleteScope),
}

/// What a keystroke in the delete modal asks the loop to do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DeleteOutcome {
    /// Key is unbound in this prompt shape — stay open, change nothing.
    Ignore,
    /// `n`/`N`/`Esc`, or "neither" — close without deleting.
    Cancel,
    /// Proceed with exactly these copies.
    Confirm(DeleteScope),
}

/// Everything the delete modal needs to render and to decide whether the second
/// confirmation is required. Captured once when the modal opens, like
/// [`Mode::Detail`]'s snapshot.
///
/// The counts are *advisory*: they drive the warning, not the permission. The
/// real decision is re-derived inside `ops::delete_branch_plan` at apply time,
/// so a repo that changed underneath the modal is still refused by the core.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeletePreview {
    pub branch: String,
    pub prompt: DeletePrompt,
    /// Commits on the local copy not in stable/`origin/<stable>`; `None` when
    /// there is no local copy or the count could not be taken.
    pub local_unmerged: Option<u32>,
    /// The same for `origin/<branch>`.
    pub remote_unmerged: Option<u32>,
    /// Set when a both-location roll degraded to origin-only because its local
    /// copy is the checked-out branch — worth saying out loud in the modal.
    pub local_is_checked_out: bool,
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
    /// Upstream tracking state per local branch, refreshed on reload. Sourced in
    /// one `for-each-ref` rather than an `ahead_behind` call per row.
    tracking: HashMap<String, git::LocalBranch>,
    /// The `[package]` version on the checked-out branch, refreshed on reload.
    /// `None` in repos with no `Cargo.toml`, where the header omits it and `[b]`
    /// refuses.
    version: Option<Semver>,
    table: TableState,
    mode: Mode,
    /// Transient one-line feedback (e.g. why an action was rejected), cleared on
    /// the next browsing keypress.
    message: Option<String>,
    /// The command currently running, if any. Mutating keys are refused while
    /// this is set; navigation is not.
    job: Option<output::Job>,
    /// The output log. Outlives its job so a result stays readable until `esc`.
    panel: Option<output::Panel>,
    /// True after a bare `g`, waiting to see whether the next key makes it `gg`.
    /// `g` has no action of its own, so this needs no timeout.
    pending_g: bool,
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

/// The roll states `[t]` tidies, which are `rf tidy --state`'s default. The TUI
/// has no way to type a state selection, so it takes that default rather than
/// inventing a second one.
pub(crate) const TIDY_STATES: [RollState; 2] = [RollState::Graduated, RollState::Promoted];

/// Tidy is offered when at least one roll in [`TIDY_STATES`] still has a local
/// branch. The local copy is the only thing tidy deletes, so a roll that exists
/// only on `origin` is nothing for it to do.
///
/// As with [`can_prune`], whether a given branch is *safe* to delete is
/// `ops::tidy_plan`'s call — it re-checks containment against stable, rolling
/// and the branch's own `origin/<branch>`; this only answers whether the action
/// is worth offering.
pub(crate) fn can_tidy(rolls: &[RollInfo]) -> bool {
    rolls.iter().any(is_tidy_candidate)
}

/// How many rolls the tidy action would consider — shown in the confirm modal.
pub(crate) fn tidyable_count(rolls: &[RollInfo]) -> usize {
    rolls.iter().filter(|r| is_tidy_candidate(r)).count()
}

fn is_tidy_candidate(roll: &RollInfo) -> bool {
    TIDY_STATES.contains(&roll.state)
        && matches!(roll.location, BranchLocation::Local | BranchLocation::Both)
}

// ── Keymap ──────────────────────────────────────────────────────────────────

/// One entry in the keymap.
///
/// The table below is the single source of truth for what keys exist: the status
/// bar renders the `basic` ones, `?` lists all of them, and `replay` is what
/// pressing enter in that list feeds back through the browsing handler. Adding a
/// key means adding a row here and an arm in [`StatusApp::handle_browsing`] —
/// nothing else, and in particular no hand-written hint string to fall out of
/// date. Before this the bindings were spelled out in four footer lines that
/// each had to be re-measured against an 80-column terminal every time one was
/// added.
pub(crate) struct Binding {
    /// As shown to the user: `"P"`, `"gg"`, `"j / ↓"`.
    pub keys: &'static str,
    pub label: &'static str,
    /// Coarse grouping, shown dim and searchable — typing `sync` finds them all.
    pub group: &'static str,
    /// The status bar's fragment for this key — `Some("[q] quit")` — when it is
    /// one of the basics someone needs before they know `?` exists. `None`
    /// keeps the key to the `?` list. It is a whole fragment rather than a word
    /// so one row can stand for a pair: `j` carries `[j/k ↑/↓] nav` and `k`
    /// carries nothing, instead of the bar saying "nav" twice.
    pub hint: Option<&'static str>,
    /// Keystrokes `?` replays to run this binding, in order. Two entries for a
    /// chord: replaying `g` then `g` arms and completes it exactly as typing it
    /// would, so there is no second dispatch path to keep in step. Empty means
    /// there is nothing to run and enter merely closes the list.
    pub replay: &'static [KeyCode],
}

/// Every key the browsing view answers to.
pub(crate) const BINDINGS: &[Binding] = &[
    Binding {
        keys: "j / ↓",
        label: "move down",
        group: "navigate",
        hint: Some("[j/k ↑/↓] nav"),
        replay: &[KeyCode::Char('j')],
    },
    Binding {
        keys: "k / ↑",
        label: "move up",
        group: "navigate",
        hint: None,
        replay: &[KeyCode::Char('k')],
    },
    Binding {
        keys: "space",
        label: "switch to the selected branch",
        group: "navigate",
        hint: Some("[space] switch"),
        replay: &[KeyCode::Char(' ')],
    },
    Binding {
        keys: "enter",
        label: "roll detail: dependencies and divergence",
        group: "navigate",
        hint: Some("[enter] detail"),
        replay: &[KeyCode::Enter],
    },
    Binding {
        keys: "r",
        label: "reload the roll list",
        group: "navigate",
        hint: None,
        replay: &[KeyCode::Char('r')],
    },
    Binding {
        keys: "?",
        label: "search every key",
        group: "navigate",
        hint: Some("[?] keys"),
        // Nothing to replay: enter here would only reopen the list it is in.
        replay: &[],
    },
    Binding {
        keys: "q",
        label: "quit",
        group: "navigate",
        hint: Some("[q] quit"),
        replay: &[KeyCode::Char('q')],
    },
    Binding {
        keys: "p",
        label: "pull the selected branch",
        group: "sync",
        hint: None,
        replay: &[KeyCode::Char('p')],
    },
    Binding {
        keys: "P",
        label: "push the selected branch",
        group: "sync",
        hint: None,
        replay: &[KeyCode::Char('P')],
    },
    Binding {
        keys: "f",
        label: "fetch and prune remote-tracking refs",
        group: "sync",
        hint: None,
        replay: &[KeyCode::Char('f')],
    },
    Binding {
        keys: "gg",
        label: "hand the terminal to lazygit",
        group: "sync",
        hint: None,
        replay: &[KeyCode::Char('g'), KeyCode::Char('g')],
    },
    Binding {
        keys: "c",
        label: "create a new roll",
        group: "roll",
        hint: None,
        replay: &[KeyCode::Char('c')],
    },
    Binding {
        keys: "i",
        label: "integrate the selected roll into this one",
        group: "roll",
        hint: None,
        replay: &[KeyCode::Char('i')],
    },
    Binding {
        keys: "v",
        label: "verify the checked-out branch",
        group: "roll",
        hint: None,
        replay: &[KeyCode::Char('v')],
    },
    Binding {
        keys: "G",
        label: "graduate the selected roll into rolling",
        group: "roll",
        hint: None,
        replay: &[KeyCode::Char('G')],
    },
    Binding {
        keys: "m",
        label: "promote to stable",
        group: "roll",
        hint: None,
        replay: &[KeyCode::Char('m')],
    },
    Binding {
        keys: "u",
        label: "update active rolls from stable",
        group: "roll",
        hint: None,
        replay: &[KeyCode::Char('u')],
    },
    Binding {
        keys: "b",
        label: "bump the version on the checked-out branch",
        group: "roll",
        hint: None,
        replay: &[KeyCode::Char('b')],
    },
    Binding {
        keys: "d",
        label: "delete the selected branch",
        group: "branches",
        hint: None,
        replay: &[KeyCode::Char('d')],
    },
    Binding {
        keys: "x",
        label: "prune promoted roll branches, local and origin",
        group: "branches",
        hint: None,
        replay: &[KeyCode::Char('x')],
    },
    Binding {
        keys: "t",
        label: "tidy local roll branches, leaving origin alone",
        group: "branches",
        hint: None,
        replay: &[KeyCode::Char('t')],
    },
    Binding {
        keys: "esc",
        label: "close the output panel",
        group: "output",
        hint: None,
        replay: &[KeyCode::Esc],
    },
    Binding {
        keys: "PgUp",
        label: "scroll the output panel up",
        group: "output",
        hint: None,
        replay: &[KeyCode::PageUp],
    },
    Binding {
        keys: "PgDn",
        label: "scroll the output panel down",
        group: "output",
        hint: None,
        replay: &[KeyCode::PageDown],
    },
    Binding {
        keys: "End",
        label: "follow new output again",
        group: "output",
        hint: None,
        replay: &[KeyCode::End],
    },
];

/// Score `needle` against `haystack` as a fuzzy subsequence match, `None` when
/// it does not match at all. Higher is better; an empty needle matches
/// everything at zero.
///
/// Deliberately fzf-shaped rather than a substring test: the useful query here
/// is a half-remembered word (`push`, `del`, `ver`) against a label the user
/// never read, and the ranking is what makes the first row the right one. Three
/// signals, in the order they matter:
///
/// - a **run bonus** for characters matched adjacently, so `prune` scores far
///   above the same five letters scattered across a sentence;
/// - a **word-start bonus**, so the acronym `pb` finds "push branch";
/// - a **gap penalty** per break in the match, which prefers the tighter of two
///   otherwise equal matches.
///
/// The gap penalty is charged once per gap rather than per character skipped,
/// and that is what makes acronyms work: `pb` against "push branch" skips four
/// characters to reach the second word, and a per-character cost would make
/// that lose to any contiguous `pb` buried mid-word.
pub(crate) fn fuzzy_score(haystack: &str, needle: &str) -> Option<i32> {
    let hay: Vec<char> = haystack.to_lowercase().chars().collect();
    let pat: Vec<char> = needle.to_lowercase().chars().collect();
    if pat.is_empty() {
        return Some(0);
    }
    let mut score = 0;
    let mut at = 0usize;
    let mut last: Option<usize> = None;
    let mut first: Option<usize> = None;
    for want in pat {
        if want.is_whitespace() {
            continue;
        }
        let found = hay[at..].iter().position(|&c| c == want)? + at;
        if last == Some(found.wrapping_sub(1)) {
            score += 8;
        } else {
            score -= 1;
        }
        if found == 0 || !hay[found - 1].is_alphanumeric() {
            score += 6;
        }
        first.get_or_insert(found);
        last = Some(found);
        at = found + 1;
    }
    // Break a tie toward the match that starts earlier. Two labels can both
    // contain the typed word outright — "prune promoted roll branches" and
    // "fetch and prune remote-tracking refs" both do — and the one that leads
    // with it is the one that is about it. Divided down so it only ever settles
    // ties and never outweighs a run.
    score -= first.unwrap_or(0) as i32 / 4;
    Some(score)
}

/// The bindings matching `query`, best first, as indices into [`BINDINGS`].
///
/// Keys, label and group are matched as one string, so `sync` lists a whole
/// group and `gg` finds lazygit by the key nobody remembers the name of. Sorting
/// is stable, so an empty query — every score zero — leaves the table's own
/// order, which is grouped the way the old status bar was.
pub(crate) fn filter_bindings(query: &str) -> Vec<usize> {
    let mut scored: Vec<(usize, i32)> = BINDINGS
        .iter()
        .enumerate()
        .filter_map(|(i, b)| {
            let hay = format!("{} {} {}", b.keys, b.label, b.group);
            fuzzy_score(&hay, query).map(|score| (i, score))
        })
        .collect();
    scored.sort_by_key(|&(_, score)| std::cmp::Reverse(score));
    scored.into_iter().map(|(i, _)| i).collect()
}

/// What a keypress in the `?` list does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HelpOutcome {
    /// Query or cursor changed (or the key meant nothing); keep the list open.
    Continue,
    Close,
    /// Run this [`BINDINGS`] entry and close.
    Run(usize),
}

/// Apply one keystroke to the `?` list, mutating the query and cursor in place.
///
/// fzf's shape, because that is what the list is: printable characters type into
/// the filter rather than navigating, so movement is the arrow keys. Editing the
/// query resets the cursor to the top — the row under it is about to be a
/// different row, and leaving the cursor at index 3 of a list that just changed
/// means enter runs something the user never looked at.
///
/// Pure (mutates only its arguments), so the whole interaction is testable
/// without a terminal.
pub(crate) fn handle_help_key(
    query: &mut String,
    cursor: &mut usize,
    code: KeyCode,
) -> HelpOutcome {
    match code {
        KeyCode::Esc => HelpOutcome::Close,
        KeyCode::Enter => match filter_bindings(query).get(*cursor) {
            Some(&index) => HelpOutcome::Run(index),
            // Enter on "no match" closes rather than doing nothing, so the list
            // is never a trap.
            None => HelpOutcome::Close,
        },
        KeyCode::Down => {
            let len = filter_bindings(query).len();
            *cursor = (*cursor + 1).min(len.saturating_sub(1));
            HelpOutcome::Continue
        }
        KeyCode::Up => {
            *cursor = cursor.saturating_sub(1);
            HelpOutcome::Continue
        }
        KeyCode::Backspace => {
            query.pop();
            *cursor = 0;
            HelpOutcome::Continue
        }
        KeyCode::Char(c) if !c.is_control() => {
            query.push(c);
            *cursor = 0;
            HelpOutcome::Continue
        }
        _ => HelpOutcome::Continue,
    }
}

/// The route `[v]` would check, as `(source, target)`, or `None` when the
/// checked-out branch is not on one.
///
/// Verify reads HEAD rather than the row under the cursor, for the same reason
/// `[b]` does: the gates run in the working tree, so the branch they judge is
/// the checked-out one whatever the cursor is on. Deferring to
/// [`ops::infer_route`] keeps the TUI and `rf verify` agreeing on what a branch
/// tier means — there is one definition of the route, not two.
pub(crate) fn verify_route_for(config: &Config, current_branch: &str) -> Option<(String, String)> {
    match ops::infer_route(config, current_branch)? {
        ops::Route::Graduate { roll } => Some((roll, config.rolling_branch.clone())),
        ops::Route::Promote => Some((config.rolling_branch.clone(), config.stable_branch.clone())),
    }
}

/// Render a version check the way `main.rs` prints it, so the TUI and the CLI
/// report the same comparison in the same words.
fn push_version_check(lines: &mut Vec<String>, check: &VersionCheck, source: &str, target: &str) {
    let verdict = match check.status {
        VersionStatus::Ok => "OK",
        VersionStatus::Unchanged => "UNCHANGED",
        VersionStatus::Lower => "LOWER",
        VersionStatus::Unreadable => "UNREADABLE",
        // Repos with no `Cargo.toml`, and repos with the gate switched off, have
        // nothing to say here — the same silence `rf verify` keeps.
        VersionStatus::NotApplicable => return,
    };
    let head = check
        .head
        .map(|v| v.to_string())
        .unwrap_or_else(|| "<unreadable>".to_string());
    let base = check
        .base
        .map(|v| v.to_string())
        .unwrap_or_else(|| "none".to_string());
    lines.push(format!(
        "Version: {head} on '{source}' (base '{target}' {base}) {verdict}"
    ));
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
/// the rolls that integrated `target` and therefore depend on it. This is the
/// inverse of [`dep_rows`] and is *not* symmetric with it.
///
/// Reads `target.dependents`, the reverse index `branches::list_rolls` builds in
/// one pass, rather than rescanning every roll's `deps` per call — this runs on
/// every frame the detail overlay is open. Unknown numbers (not present in
/// `all`) are skipped, mirroring [`dep_rows`].
///
/// A row's `is_blocker` here is repurposed to mean "this dependent is still
/// gated by the target" — true while `target` has not yet graduated/promoted,
/// since until then the dependent cannot advance past it. The detail view does
/// not render a per-row blocker marker for dependents, so this flag is purely
/// informational, but it keeps the field meaningful and testable. A roll is
/// never its own dependent, even if a self-referential entry somehow appears.
pub(crate) fn dependent_rows(target: &RollInfo, all: &[RollInfo]) -> Vec<DepRow> {
    let target_gates = !matches!(target.state, RollState::Graduated | RollState::Promoted);
    target
        .dependents
        .iter()
        .filter(|num| **num != target.number)
        .filter_map(|num| all.iter().find(|r| r.number == *num))
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

/// What `[p]` should promote, given the current selection.
///
/// `Ok(None)` means the whole rolling branch — one merge behind one gate run,
/// the long-standing behaviour. `Ok(Some(branch))` means that one graduated
/// roll, promoted by advancing stable to its graduation commit.
///
/// Selecting a base branch or nothing at all yields `None`: both pinned rows are
/// about the branch as a whole, and an empty selection has no narrower intent to
/// honour. A roll row that cannot be promoted yields the reason rather than
/// quietly widening to the whole branch, which would promote far more than the
/// keystroke asked for.
pub(crate) fn promote_target_for(
    selected: Option<&RollInfo>,
    rolls: &[RollInfo],
) -> Result<Option<String>, String> {
    let Some(sel) = selected else {
        return if can_promote(rolls) {
            Ok(None)
        } else {
            Err("nothing to promote — no graduated rolls on rolling".to_string())
        };
    };

    match sel.state {
        RollState::Graduated | RollState::Diverged => Ok(Some(sel.branch.clone())),
        RollState::Promoted => Err(format!("{} is already promoted", sel.branch)),
        RollState::Active | RollState::Blocked => Err(format!(
            "{} is {} — only graduated rolls can be promoted",
            sel.branch,
            sel.state.label()
        )),
    }
}

/// The roll `[i]` would merge into the current branch, or the reason it cannot.
///
/// Unlike every other action, integration reads *two* rows: the source is the one
/// under the cursor and the destination is whatever is checked out. That is why
/// this needs `current_branch` when the other gates do not, and why "no roll
/// selected" is only one of several ways it can be refused.
pub(crate) fn integrate_target_for(
    current_branch: &str,
    roll_prefix: &str,
    selected: Option<&RollInfo>,
) -> Result<String, String> {
    // Mirrors the check inside `ops::integrate`, so the key is never offered for
    // something the core would refuse anyway.
    if !current_branch.starts_with(roll_prefix) {
        return Err(format!(
            "'{current_branch}' is not a roll branch — integrate merges into a roll"
        ));
    }
    let sel = selected.ok_or_else(|| "no roll selected".to_string())?;
    if sel.branch == current_branch {
        return Err(format!("'{}' is already the current branch", sel.branch));
    }
    // `ops::integrate` resolves the source with a bare `ref_exists`, so a
    // remote-only roll would fail there as "branch not found" — a confusing way to
    // say "fetch it first".
    if !matches!(sel.location, BranchLocation::Local | BranchLocation::Both) {
        return Err(format!(
            "'{}' exists only on origin — press [space] or [p] to get it locally first",
            sel.branch
        ));
    }
    Ok(sel.branch.clone())
}

/// Validate an action against the current selection/list. `Ok(())` means the
/// confirm modal may open; `Err(msg)` is a brief reason to surface instead.
pub(crate) fn validate_action(
    action: Action,
    selected: Option<&RollInfo>,
    rolls: &[RollInfo],
    current_branch: &str,
    roll_prefix: &str,
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
        Action::Integrate => {
            integrate_target_for(current_branch, roll_prefix, selected).map(|_| ())
        }
        Action::Promote => promote_target_for(selected, rolls).map(|_| ()),
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
        Action::Tidy => {
            if can_tidy(rolls) {
                Ok(())
            } else {
                Err("nothing to tidy — no local graduated or promoted roll branches".to_string())
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

/// Decide the delete-modal shape for `roll`, or reject the request outright.
///
/// `Both` yields the four-way choice; `Local`/`Remote` a plain y/N. The
/// checked-out branch's local copy is never deletable, so a both-location
/// current roll degrades to a remote-only y/N and a local-only current roll is
/// refused. `Neither` is refused too — the row is stale, there is nothing there.
pub(crate) fn delete_prompt(roll: &RollInfo) -> Result<DeletePrompt, String> {
    match (&roll.location, roll.is_current) {
        (BranchLocation::Neither, _) => Err(format!("{} no longer exists", roll.branch)),
        (BranchLocation::Local, true) => Err(format!(
            "{} is checked out — switch away before deleting it",
            roll.branch
        )),
        (BranchLocation::Local, false) => Ok(DeletePrompt::Single(DeleteScope::Local)),
        (BranchLocation::Remote, _) => Ok(DeletePrompt::Single(DeleteScope::Remote)),
        // A checked-out both-location roll keeps its origin copy on the table;
        // only the local one is off limits.
        (BranchLocation::Both, true) => Ok(DeletePrompt::Single(DeleteScope::Remote)),
        (BranchLocation::Both, false) => Ok(DeletePrompt::Choice),
    }
}

/// Whether `scope` touches a copy that holds commits stable lacks, and so needs
/// the second explicit confirmation before anything is deleted.
///
/// An unknown count (`None` for a copy that is in scope) counts as needing the
/// confirmation: not knowing what a delete costs is not a reason to skip the
/// warning.
pub(crate) fn needs_force_confirm(scope: DeleteScope, preview: &DeletePreview) -> bool {
    let dirty = |count: Option<u32>| count.is_none_or(|n| n > 0);
    match scope {
        DeleteScope::Local => dirty(preview.local_unmerged),
        DeleteScope::Remote => dirty(preview.remote_unmerged),
        DeleteScope::Both => dirty(preview.local_unmerged) || dirty(preview.remote_unmerged),
    }
}

/// The largest number of commits any copy in `scope` would lose, for the
/// warning line. `None` when no count is known.
pub(crate) fn unmerged_for_scope(scope: DeleteScope, preview: &DeletePreview) -> Option<u32> {
    match scope {
        DeleteScope::Local => preview.local_unmerged,
        DeleteScope::Remote => preview.remote_unmerged,
        DeleteScope::Both => preview
            .local_unmerged
            .into_iter()
            .chain(preview.remote_unmerged)
            .max(),
    }
}

/// Map one keystroke to a delete decision for `prompt`. Pure — the same
/// contract as [`handle_create_key`] — so every prompt shape is unit-testable
/// without a terminal.
///
/// `Single`/`ForceConfirm`: `y` confirms, `n`/`Esc` cancels, every other key is
/// ignored. That is exactly what "defaults to No" means here — nothing at all
/// happens without a deliberate `y`, and Enter is not a shortcut for it.
///
/// `Choice`: `l` local, `r` origin, `b` both, `n`/`Esc` neither. `y` is
/// deliberately unbound: with two copies there is no obvious "yes", and mapping
/// it to "both" would let muscle memory delete more than the user was looking
/// at.
pub(crate) fn handle_delete_key(prompt: DeletePrompt, code: KeyCode) -> DeleteOutcome {
    match prompt {
        DeletePrompt::Single(scope) | DeletePrompt::ForceConfirm(scope) => match code {
            KeyCode::Char('y') | KeyCode::Char('Y') => DeleteOutcome::Confirm(scope),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => DeleteOutcome::Cancel,
            _ => DeleteOutcome::Ignore,
        },
        DeletePrompt::Choice => match code {
            KeyCode::Char('l') | KeyCode::Char('L') => DeleteOutcome::Confirm(DeleteScope::Local),
            KeyCode::Char('r') | KeyCode::Char('R') => DeleteOutcome::Confirm(DeleteScope::Remote),
            KeyCode::Char('b') | KeyCode::Char('B') => DeleteOutcome::Confirm(DeleteScope::Both),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => DeleteOutcome::Cancel,
            _ => DeleteOutcome::Ignore,
        },
    }
}

/// The `ops::PruneScope` a delete scope asks for.
///
/// `fetch` is on exactly when the remote is in scope, so containment of
/// `origin/<branch>` is never judged against a stale remote-tracking ref — a
/// stale one names an old tip, and deleting against it would destroy commits
/// the check never saw.
pub(crate) fn prune_scope_for(scope: DeleteScope, force: bool) -> ops::PruneScope {
    let remote = matches!(scope, DeleteScope::Remote | DeleteScope::Both);
    ops::PruneScope {
        local: matches!(scope, DeleteScope::Local | DeleteScope::Both),
        remote,
        force,
        fetch: remote,
        ..ops::PruneScope::both()
    }
}

// ── App ─────────────────────────────────────────────────────────────────────

impl StatusApp {
    fn new(config: Config, current_branch: String, rolls: Vec<RollInfo>, show_deps: bool) -> Self {
        let bases = base_branches(&config, &current_branch, |refspec| {
            git::ref_exists(&config.repo_root, refspec)
        });
        let mut table = TableState::default();
        table.select(initial_selection(&bases, &rolls));
        let tracking = load_tracking(&config);
        let version = version::read_version(&config.repo_root).unwrap_or(None);
        Self {
            config,
            current_branch,
            bases,
            rolls,
            show_deps,
            tracking,
            version,
            table,
            mode: Mode::Browsing,
            message: None,
            job: None,
            panel: None,
            pending_g: false,
        }
    }

    fn run_loop(&mut self, terminal: &mut super::Tui) -> Result<()> {
        loop {
            terminal.draw(|f| self.render(f))?;
            // Before reading input, so a job that finished during the poll is
            // reflected in this frame rather than the next one.
            self.poll_job()?;

            if event::poll(Duration::from_millis(50))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    if matches!(self.mode, Mode::Confirm { .. }) {
                        self.handle_confirm(key.code);
                    } else if matches!(self.mode, Mode::ForcePush { .. }) {
                        self.handle_force_push(key.code);
                    } else if matches!(self.mode, Mode::Bump { .. }) {
                        self.handle_bump(key.code);
                    } else if matches!(self.mode, Mode::Detail { .. }) {
                        self.handle_detail(key.code);
                    } else if matches!(self.mode, Mode::CreateInput { .. }) {
                        self.handle_create_input(key.code);
                    } else if matches!(self.mode, Mode::Delete { .. }) {
                        self.handle_delete(key.code);
                    } else if matches!(self.mode, Mode::Help { .. }) {
                        // Takes the terminal because a replayed `gg` hands it to
                        // lazygit, exactly as typing `gg` would.
                        if self.handle_help(terminal, key.code)? {
                            break;
                        }
                    } else if self.handle_browsing(terminal, key.code)? {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    /// Move a running job's output into the panel, and act on its result once it
    /// finishes.
    fn poll_job(&mut self) -> Result<()> {
        // Split field borrows: the job writes into the panel, and both live on
        // `self`.
        let progress = match (&mut self.job, &mut self.panel) {
            (Some(job), Some(panel)) => job.drain(panel),
            _ => return Ok(()),
        };
        if let Some(panel) = self.panel.as_mut() {
            panel.tick();
        }
        let JobProgress::Finished(next) = progress else {
            return Ok(());
        };
        self.job = None;
        // Reload regardless of outcome: a failed op may still have changed the
        // repo, and a stale table is worse than a redundant refresh.
        self.reload()?;
        match next {
            Some(Followup::SelectBranch(branch)) => {
                if let Some(idx) = self.rolls.iter().position(|r| r.branch == branch) {
                    self.table.select(Some(self.bases.len() + idx));
                }
            }
            Some(Followup::OfferForcePush { branch, remote }) => {
                self.mode = Mode::ForcePush { branch, remote };
            }
            None => {}
        }
        Ok(())
    }

    /// Start a background job, replacing any previous panel.
    ///
    /// Replacing rather than appending keeps one panel to one operation, so its
    /// border colour means something: a green panel is *this* command's success,
    /// not the last one's.
    fn start_job(
        &mut self,
        title: impl Into<String>,
        body: impl FnOnce() -> Result<JobDone> + Send + 'static,
    ) {
        self.panel = Some(output::Panel::new(title));
        self.job = Some(output::Job::spawn(body));
    }

    /// Refuse a mutating key while a job is in flight, so two commands cannot
    /// race on the same repo. Returns true when the caller should stop.
    fn busy(&mut self) -> bool {
        if self.job.is_some() {
            self.message = Some("a git command is already running".to_string());
            return true;
        }
        false
    }

    /// Handle a keypress while browsing. Returns `Ok(true)` to quit.
    fn handle_browsing(&mut self, terminal: &mut super::Tui, code: KeyCode) -> Result<bool> {
        self.message = None;

        // `gg` opens lazygit. `g` alone does nothing, so a pending `g` needs no
        // timeout: any other key clears it and is then handled normally.
        if std::mem::take(&mut self.pending_g) {
            if code == KeyCode::Char('g') {
                self.launch_lazygit(terminal)?;
                return Ok(false);
            }
        } else if code == KeyCode::Char('g') {
            self.pending_g = true;
            return Ok(false);
        }

        match code {
            KeyCode::Char('q') => return Ok(true),
            // `esc` dismisses the output panel when one is up, and only quits
            // when there is nothing left to dismiss.
            KeyCode::Esc => {
                if self.panel.is_some() && self.job.is_none() {
                    self.panel = None;
                } else if self.panel.is_none() {
                    return Ok(true);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => self.select_next(),
            KeyCode::Up | KeyCode::Char('k') => self.select_prev(),
            // Scrollback in the panel, which stays readable while a job runs.
            KeyCode::PageUp => self.scroll_panel(PanelScroll::Up),
            KeyCode::PageDown => self.scroll_panel(PanelScroll::Down),
            KeyCode::End => self.scroll_panel(PanelScroll::End),
            KeyCode::Char('r') => {
                self.reload()?;
                self.message = Some("refreshed".to_string());
            }
            KeyCode::Char('c') => {
                if self.busy() {
                    return Ok(false);
                }
                self.mode = Mode::CreateInput {
                    slug: String::new(),
                }
            }
            KeyCode::Char(' ') => {
                if self.busy() {
                    return Ok(false);
                }
                match self.selected_row() {
                    Some(RowKind::Roll(i)) => {
                        let roll = self.rolls[i].clone();
                        self.execute_switch(roll.branch, roll.location);
                    }
                    Some(RowKind::Base(i)) => {
                        let base = self.bases[i].clone();
                        self.execute_switch(base.branch, base.location);
                    }
                    None => self.message = Some("no branch selected".to_string()),
                }
            }
            KeyCode::Char('p') => self.start_pull(),
            KeyCode::Char('P') => self.start_push(),
            KeyCode::Char('f') => self.start_fetch(),
            KeyCode::Char('v') => self.start_verify(),
            KeyCode::Char('G') => self.request(Action::Graduate),
            KeyCode::Char('i') => self.request_integrate(),
            KeyCode::Char('b') => self.request_bump(),
            KeyCode::Char('m') => self.request(Action::Promote),
            KeyCode::Char('u') => self.request(Action::Update),
            KeyCode::Char('x') => self.request(Action::Prune),
            KeyCode::Char('t') => self.request(Action::Tidy),
            KeyCode::Char('d') => self.request_delete()?,
            // No busy guard: the list changes nothing, and every binding it can
            // run carries its own.
            KeyCode::Char('?') => {
                self.mode = Mode::Help {
                    query: String::new(),
                    cursor: 0,
                }
            }
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
    fn handle_create_input(&mut self, code: KeyCode) {
        let outcome = if let Mode::CreateInput { slug } = &mut self.mode {
            handle_create_key(slug, code)
        } else {
            return;
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
                    self.execute_create(slug);
                } else {
                    self.message = Some("slug cannot be empty".to_string());
                }
            }
        }
    }

    /// Handle a keypress while the confirm modal is open.
    fn handle_confirm(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if let Mode::Confirm { action, target } =
                    std::mem::replace(&mut self.mode, Mode::Browsing)
                {
                    self.execute(action, target);
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.mode = Mode::Browsing;
            }
            _ => {}
        }
    }

    /// Drive the `?` list. Returns `Ok(true)` to quit, since a replayed `q`
    /// means exactly what typing it would.
    fn handle_help(&mut self, terminal: &mut super::Tui, code: KeyCode) -> Result<bool> {
        let Mode::Help { query, cursor } = &mut self.mode else {
            return Ok(false);
        };
        match handle_help_key(query, cursor, code) {
            HelpOutcome::Continue => Ok(false),
            HelpOutcome::Close => {
                self.mode = Mode::Browsing;
                Ok(false)
            }
            HelpOutcome::Run(index) => {
                // Closed *before* replaying, so the binding sees the browsing
                // view it expects — one that can open a modal of its own.
                self.mode = Mode::Browsing;
                for code in BINDINGS[index].replay {
                    if self.handle_browsing(terminal, *code)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
        }
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
        let validation = validate_action(
            action,
            selected,
            &self.rolls,
            &self.current_branch,
            &self.config.roll_prefix,
        );
        let target = match action {
            Action::Graduate => selected.map(|r| r.branch.clone()),
            Action::Integrate => {
                integrate_target_for(&self.current_branch, &self.config.roll_prefix, selected).ok()
            }
            // `None` here means "the whole rolling branch", not "no target".
            Action::Promote => promote_target_for(selected, &self.rolls).unwrap_or(None),
            _ => None,
        };
        match validation {
            Ok(()) => self.mode = Mode::Confirm { action, target },
            Err(msg) => self.message = Some(msg),
        }
    }

    /// Open the delete modal for the selected roll, or say why it cannot be
    /// deleted. The per-copy unmerged counts are taken here, once, so the modal
    /// can state what a delete would cost without re-shelling out on every draw.
    /// `[i]` — merge the roll under the cursor into the checked-out branch.
    ///
    /// Not routed through [`Self::request`] because the row under the cursor is
    /// the *source* here, and because a base row needs its own answer: `main` and
    /// `rolling` are selectable, but merging them into a roll is a different
    /// operation with its own key.
    fn request_integrate(&mut self) {
        if self.busy() {
            return;
        }
        if let Some(RowKind::Base(i)) = self.selected_row() {
            self.message = Some(format!(
                "'{}' is a base branch — [u]pdate brings stable into your rolls",
                self.bases[i].branch
            ));
            return;
        }
        match integrate_target_for(
            &self.current_branch,
            &self.config.roll_prefix,
            self.selected_roll(),
        ) {
            Ok(branch) => {
                self.mode = Mode::Confirm {
                    action: Action::Integrate,
                    target: Some(branch),
                }
            }
            Err(msg) => self.message = Some(msg),
        }
    }

    /// `[b]` — open the version-bump picker for the checked-out branch.
    ///
    /// Always the current branch, never the row under the cursor: a bump is a
    /// commit, and it has to land where the merge that needs it will be made from.
    fn request_bump(&mut self) {
        if self.busy() {
            return;
        }
        match bump_gate(self.version) {
            Ok(current) => self.mode = Mode::Bump { current },
            Err(msg) => self.message = Some(msg),
        }
    }

    /// Handle a keypress while the bump picker is open.
    fn handle_bump(&mut self, code: KeyCode) {
        match bump_key(code) {
            BumpOutcome::Ignore => {}
            BumpOutcome::Cancel => self.mode = Mode::Browsing,
            BumpOutcome::Level(level) => {
                self.mode = Mode::Browsing;
                self.execute_bump(level);
            }
        }
    }

    /// Write the bumped version, refresh the lockfile and commit, as a job.
    fn execute_bump(&mut self, level: BumpLevel) {
        let config = self.config.clone();
        let branch = self.current_branch.clone();
        self.start_job(format!("rf bump {level}"), move || {
            // A bump is a commit, so anything else staged would be swept into it:
            // `git::commit_paths` stages its two files and then commits the whole
            // index. Graduate and promote take the same precaution.
            ops::ensure_clean_state(&config)?;
            let (from, to) = ops::apply_version_bump(&config, level, &branch)?;
            Ok(JobDone::lines(vec![format!(
                "Bumped {from} → {to} ({level}) on '{branch}'"
            )]))
        });
    }

    fn request_delete(&mut self) -> Result<()> {
        let Some(roll) = self.selected_roll() else {
            self.message = Some("no roll selected".to_string());
            return Ok(());
        };
        let roll = roll.clone();

        let prompt = match delete_prompt(&roll) {
            Ok(prompt) => prompt,
            Err(msg) => {
                self.message = Some(msg);
                return Ok(());
            }
        };

        let (local_unmerged, remote_unmerged) =
            ops::unmerged_commit_counts(&self.config, &roll.branch);

        self.mode = Mode::Delete {
            preview: DeletePreview {
                branch: roll.branch,
                prompt,
                local_unmerged,
                remote_unmerged,
                local_is_checked_out: roll.is_current
                    && matches!(roll.location, BranchLocation::Both),
            },
        };
        Ok(())
    }

    /// Handle a keypress while the delete modal is open.
    ///
    /// A confirmation that would touch a copy holding commits stable lacks does
    /// *not* delete: it advances the modal to [`DeletePrompt::ForceConfirm`],
    /// which states the cost and demands a fresh `y`. That second `y` is the
    /// only thing that ever sets `force`.
    fn handle_delete(&mut self, code: KeyCode) {
        let Mode::Delete { preview } = &self.mode else {
            return;
        };
        let prompt = preview.prompt;

        match handle_delete_key(prompt, code) {
            DeleteOutcome::Ignore => {}
            DeleteOutcome::Cancel => self.mode = Mode::Browsing,
            DeleteOutcome::Confirm(scope) => {
                let already_forced = matches!(prompt, DeletePrompt::ForceConfirm(_));
                if !already_forced && needs_force_confirm(scope, preview) {
                    if let Mode::Delete { preview } = &mut self.mode {
                        preview.prompt = DeletePrompt::ForceConfirm(scope);
                    }
                    return;
                }
                let Mode::Delete { preview } = std::mem::replace(&mut self.mode, Mode::Browsing)
                else {
                    return;
                };
                self.execute_delete(preview.branch, scope, already_forced);
            }
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

    /// Run a workflow op as a background job.
    fn execute(&mut self, action: Action, target: Option<String>) {
        let config = self.config.clone();
        let title = action.job_title(target.as_deref());
        self.start_job(title, move || {
            Ok(JobDone::lines(run_op(&config, action, target.as_deref())?))
        });
    }

    /// Create a roll from `slug`, selecting it once the reload turns it up. An
    /// `ops::create` error (e.g. an invalid slug) lands in the panel like any
    /// other failure and never aborts the TUI.
    fn execute_create(&mut self, slug: String) {
        let config = self.config.clone();
        self.start_job("rf create", move || {
            let outcome = ops::create(&config, &slug, None, false)?;
            Ok(JobDone::with_next(
                vec![format!("Created {}", outcome.branch)],
                Followup::SelectBranch(outcome.branch),
            ))
        });
    }

    /// Switch the working tree to `branch` (issue #99). Git natively carries
    /// clean uncommitted changes forward and refuses (non-zero) when they would
    /// conflict; either way the refusal shows in the panel and the TUI survives.
    /// Serves both roll rows and the pinned base-branch rows.
    fn execute_switch(&mut self, branch: String, location: BranchLocation) {
        let config = self.config.clone();
        let current = self.current_branch.clone();
        self.start_job(format!("git switch {branch}"), move || {
            Ok(JobDone::lines(run_switch(
                &config, &current, &branch, &location,
            )?))
        });
    }

    /// Delete `branch`, so git's own failures are visible and the list reloads
    /// after.
    fn execute_delete(&mut self, branch: String, scope: DeleteScope, force: bool) {
        let config = self.config.clone();
        self.start_job(format!("rf delete {branch}"), move || {
            Ok(JobDone::lines(run_delete(&config, &branch, scope, force)?))
        });
    }

    // ── Sync (lazygit's p / P / f) ──────────────────────────────────────────

    /// Build a [`SyncTarget`] for the selected row, or `None` when nothing is
    /// selected. Serves roll rows and the pinned base rows alike — `main` and
    /// `rolling` are branches like any other as far as syncing goes.
    fn sync_target(&self) -> Option<SyncTarget> {
        let (branch, location) = match self.selected_row()? {
            RowKind::Roll(i) => (self.rolls[i].branch.clone(), self.rolls[i].location.clone()),
            RowKind::Base(i) => (self.bases[i].branch.clone(), self.bases[i].location.clone()),
        };
        Some(SyncTarget::resolve(
            &branch,
            &self.current_branch,
            location,
            self.tracking.get(&branch),
        ))
    }

    /// `[p]` — pull the selected branch. What that means depends on whether it is
    /// checked out; see [`sync::pull_plan`].
    fn start_pull(&mut self) {
        if self.busy() {
            return;
        }
        let Some(target) = self.sync_target() else {
            self.message = Some("no branch selected".to_string());
            return;
        };
        let repo = self.config.repo_root.clone();
        let (args, title) = match sync::pull_plan(&target, self.config.pull_mode) {
            PullPlan::Refused { reason } => {
                self.message = Some(reason);
                return;
            }
            PullPlan::Pull { args } => (args, format!("git pull {}", target.branch)),
            PullPlan::FastForward { args } => (args, format!("fast-forward {}", target.branch)),
            PullPlan::FetchRemote { args } => (args, format!("git fetch {}", target.branch)),
        };
        self.start_job(title, move || {
            sync::run_pull(&repo, &args).map_err(sync_error)?;
            Ok(JobDone::lines(vec!["up to date".to_string()]))
        });
    }

    /// `[P]` — push the selected branch, offering a force when git refuses.
    ///
    /// A branch already known to be behind skips the doomed plain attempt and
    /// goes straight to the confirmation, which is what lazygit does: there is no
    /// point spending a round-trip to be told what the tracking ref already says.
    fn start_push(&mut self) {
        if self.busy() {
            return;
        }
        let Some(target) = self.sync_target() else {
            self.message = Some("no branch selected".to_string());
            return;
        };
        if !matches!(
            target.location,
            BranchLocation::Local | BranchLocation::Both
        ) {
            self.message = Some(format!(
                "'{}' exists only on origin — nothing local to push",
                target.branch
            ));
            return;
        }
        if sync::needs_force_prompt(&target) {
            self.mode = Mode::ForcePush {
                branch: target.branch.clone(),
                remote: target.remote.clone(),
            };
            return;
        }
        self.push_job(target, false);
    }

    /// Run one push attempt. On a non-fast-forward refusal the job finishes
    /// *successfully* carrying a [`Followup::OfferForcePush`] — the refusal is an
    /// expected answer to be acted on, not an error to report and stop at.
    fn push_job(&mut self, target: SyncTarget, force: bool) {
        let repo = self.config.repo_root.clone();
        let title = if force {
            format!("git push --force-with-lease {}", target.branch)
        } else {
            format!("git push {}", target.branch)
        };
        self.start_job(title, move || {
            match sync::run_push(&repo, &target, force).map_err(sync_error)? {
                PushOutcome::Pushed => Ok(JobDone::lines(vec![format!(
                    "Pushed '{}' to {}",
                    target.branch, target.remote
                )])),
                PushOutcome::Rejected { .. } => Ok(JobDone::with_next(
                    vec![format!(
                        "'{}' was rejected by {}",
                        target.branch, target.remote
                    )],
                    Followup::OfferForcePush {
                        branch: target.branch.clone(),
                        remote: target.remote.clone(),
                    },
                )),
            }
        });
    }

    /// `[f]` — refresh every remote-tracking ref and drop the ones whose upstream
    /// is gone, so the sync column and every containment check that follows are
    /// judged against current data.
    fn start_fetch(&mut self) {
        if self.busy() {
            return;
        }
        let repo = self.config.repo_root.clone();
        self.start_job("git fetch --prune", move || {
            sync::run_fetch(&repo, "origin").map_err(sync_error)?;
            Ok(JobDone::lines(vec!["fetched origin".to_string()]))
        });
    }

    /// `[v]` — check whether the checked-out branch could graduate or promote,
    /// running the configured gates.
    ///
    /// Not an [`Action`] and deliberately not behind the confirm modal: that
    /// modal exists for the ops that mutate the repo, and verify is the one key
    /// whose entire output is a verdict. The route is resolved here rather than
    /// inside the job so the panel title names it before the gates start — on a
    /// repo with `cargo test` gates that is the difference between a title the
    /// user can trust and several silent minutes.
    fn start_verify(&mut self) {
        if self.busy() {
            return;
        }
        let Some((source, target)) = verify_route_for(&self.config, &self.current_branch) else {
            self.message = Some(format!(
                "'{}' is not promotable — check out {} or a {}branch to verify",
                self.current_branch, self.config.rolling_branch, self.config.roll_prefix
            ));
            return;
        };
        let config = self.config.clone();
        self.start_job(format!("rf verify {source} → {target}"), move || {
            Ok(JobDone::lines(run_verify(&config)?))
        });
    }

    /// `gg` — hand the terminal to lazygit, then take it back.
    ///
    /// The one action that still suspends: lazygit is a full-screen application
    /// and owns the terminal outright, so there is nothing to stream into a
    /// panel. The list reloads afterwards because anything at all may have
    /// happened inside it.
    fn launch_lazygit(&mut self, terminal: &mut super::Tui) -> Result<()> {
        if self.busy() {
            return Ok(());
        }
        super::suspend(terminal)?;
        let spawned = std::process::Command::new(&self.config.lazygit_command)
            .arg("-p")
            .arg(&self.config.repo_root)
            .status();
        super::resume(terminal)?;

        match spawned {
            Ok(status) if status.success() => self.message = None,
            Ok(status) => {
                self.message = Some(format!(
                    "{} exited with {status}",
                    self.config.lazygit_command
                ))
            }
            // A missing binary is the common case and deserves the actionable
            // message rather than a raw io error.
            Err(err) => {
                let mut panel = output::Panel::new(self.config.lazygit_command.clone());
                panel.fail(vec![
                    format!("could not run '{}': {err}", self.config.lazygit_command),
                    "set lazygit_command in .roll-flow.toml to override".to_string(),
                ]);
                self.panel = Some(panel);
            }
        }
        self.reload()
    }

    /// Move the output panel's scrollback, if one is up.
    fn scroll_panel(&mut self, how: PanelScroll) {
        let Some(panel) = self.panel.as_mut() else {
            return;
        };
        match how {
            PanelScroll::Up => panel.scroll_up(PANEL_SCROLL_STEP),
            PanelScroll::Down => panel.scroll_down(PANEL_SCROLL_STEP),
            PanelScroll::End => panel.scroll_to_end(),
        }
    }

    /// Handle a keypress while the force-push confirmation is open.
    fn handle_force_push(&mut self, code: KeyCode) {
        match force_push_key(code) {
            ForcePushOutcome::Ignore => {}
            ForcePushOutcome::Cancel => self.mode = Mode::Browsing,
            ForcePushOutcome::Force => {
                let Mode::ForcePush { branch, .. } =
                    std::mem::replace(&mut self.mode, Mode::Browsing)
                else {
                    return;
                };
                // Re-resolve rather than reusing the target captured when the
                // modal opened: the tracking data may have moved underneath it,
                // and a force push is the last place to act on a stale view.
                let location = self.location_of(&branch);
                let target = SyncTarget::resolve(
                    &branch,
                    &self.current_branch,
                    location,
                    self.tracking.get(&branch),
                );
                self.push_job(target, true);
            }
        }
    }

    /// Where `branch` currently exists, for a branch named by an open modal
    /// rather than by the selection.
    fn location_of(&self, branch: &str) -> BranchLocation {
        if let Some(roll) = self.rolls.iter().find(|r| r.branch == branch) {
            return roll.location.clone();
        }
        if let Some(base) = self.bases.iter().find(|b| b.branch == branch) {
            return base.location.clone();
        }
        BranchLocation::Local
    }

    /// Rebuild the roll list and current-branch after an action, keeping the
    /// selection in bounds.
    fn reload(&mut self) -> Result<()> {
        self.current_branch = git::current_branch(&self.config.repo_root)?;
        self.bases = base_branches(&self.config, &self.current_branch, |refspec| {
            git::ref_exists(&self.config.repo_root, refspec)
        });
        self.rolls = branches::list_rolls(&self.config)?;
        self.tracking = load_tracking(&self.config);
        // Read from the worktree, so a bump — or a branch switch that changes it —
        // shows in the header straight away.
        self.version = version::read_version(&self.config.repo_root).unwrap_or(None);
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
            // One message line plus the single hint line.
            Constraint::Length(2),
        ])
        .split(area);

        self.render_header(f, chunks[0]);
        self.render_table(f, chunks[1]);
        self.render_status_bar(f, chunks[2]);

        // Over the table, never over the hints: the panel is passed the table's
        // area so the keymap stays readable while a job runs.
        if let Some(panel) = &self.panel {
            output::render(f, chunks[1], panel);
        }

        match &self.mode {
            Mode::Confirm { action, target } => {
                render_modal(
                    f,
                    area,
                    &self.config,
                    *action,
                    target.as_deref(),
                    &self.rolls,
                    &self.current_branch,
                );
            }
            Mode::Detail { roll, ahead_behind } => {
                render_detail(f, area, roll, *ahead_behind, &self.rolls)
            }
            Mode::CreateInput { slug } => render_create_input(f, area, &self.config, slug),
            Mode::Delete { preview } => render_delete_modal(f, area, &self.config, preview),
            Mode::Bump { current } => render_bump_modal(f, area, *current, &self.current_branch),
            Mode::ForcePush { branch, remote } => {
                render_force_push_modal(f, area, branch, remote, self.tracking.get(branch))
            }
            Mode::Help { query, cursor } => render_help(f, area, query, *cursor),
            Mode::Browsing => {}
        }
    }

    fn render_header(&self, f: &mut Frame, area: Rect) {
        let spans = vec![
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
        ];
        let mut block = Block::bordered().title(" roll-flow ");
        // The version rides the top-right corner of the header border rather
        // than the end of the branch line: it belongs to the repo, not to the
        // branch, and the line it used to sit on grows with the branch name —
        // on a long `roll/N-MMDD-slug` it was the first thing to be truncated.
        // Only rendered when the repo has one; without that, `[b]` would be a
        // key whose whole effect is a line in a dismissable panel.
        if let Some(v) = self.version {
            block = block.title_top(
                Line::from(Span::styled(
                    format!(" v{v} "),
                    Style::default().fg(Color::Magenta),
                ))
                .right_aligned(),
            );
        }
        f.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
    }

    fn render_table(&mut self, f: &mut Frame, area: Rect) {
        let mut col_constraints = vec![
            // The current-branch chevron, narrow and always present so the
            // columns after it do not shift as HEAD moves.
            Constraint::Length(1),
            Constraint::Length(4),
            Constraint::Fill(1),
            Constraint::Length(3),
            // `↑12↓12` at its widest.
            Constraint::Length(7),
            Constraint::Length(13),
        ];
        if self.show_deps {
            col_constraints.push(Constraint::Length(8));
            // Exactly the header width: the values are short comma lists, and
            // `branch` is the Fill column that pays for anything wider.
            col_constraints.push(Constraint::Length(10));
        }

        let bold = Style::default().add_modifier(Modifier::BOLD);
        let mut header_cells = vec![
            Cell::from(""),
            Cell::from("#").style(bold),
            Cell::from("branch").style(bold),
            Cell::from("loc").style(bold),
            Cell::from("sync").style(bold),
            Cell::from("state").style(bold),
        ];
        if self.show_deps {
            header_cells
                .push(Cell::from("deps").style(Style::default().add_modifier(Modifier::BOLD)));
            header_cells.push(
                Cell::from("dependants").style(Style::default().add_modifier(Modifier::BOLD)),
            );
        }
        let table_header = Row::new(header_cells)
            .style(Style::default().add_modifier(Modifier::UNDERLINED))
            .height(1);

        let show_deps = self.show_deps;
        let tracking = &self.tracking;
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
                let (sync_text, sync_color) = sync_cell(track_of(tracking, &base.branch));
                let mut cells = vec![
                    Cell::from(current_marker(base.is_current)).style(
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Cell::from(""),
                    Cell::from(base.branch.clone()).style(base_style.fg(base.role.color())),
                    Cell::from(base.location.symbol()).style(base_style),
                    Cell::from(sync_text).style(Style::default().fg(sync_color)),
                    Cell::from(base.role.label()).style(Style::default().fg(base.role.color())),
                ];
                if show_deps {
                    cells.push(Cell::from(""));
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
            let (sync_text, sync_color) = sync_cell(track_of(tracking, &roll.branch));
            let mut cells = vec![
                Cell::from(current_marker(roll.is_current)).style(
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Cell::from(roll.number.to_string()).style(base_style),
                Cell::from(roll.branch.clone()).style(base_style),
                Cell::from(roll.location.symbol()).style(base_style),
                Cell::from(sync_text).style(Style::default().fg(sync_color)),
                Cell::from(roll.state.label()).style(Style::default().fg(row_state_color)),
            ];
            if show_deps {
                cells.push(Cell::from(branches::format_roll_numbers(&roll.deps)));
                cells.push(Cell::from(branches::format_roll_numbers(&roll.dependents)));
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
        // One line, built from the keymap rather than written out. Four hand-kept
        // lines listing twenty bindings crowded the bottom of the screen and had
        // to be re-measured against an 80-column terminal every time a key was
        // added; the rest now lives behind `?`, which can hold any number of
        // them and is searchable besides.
        f.render_widget(
            Paragraph::new(vec![msg_line, Line::from(basic_hints())]),
            area,
        );
    }
}

/// Perform the branch switch, returning printable status lines. A remote-only
/// branch is fetched first so `git switch` can DWIM-create a local tracking
/// branch from `origin/<branch>`.
fn run_switch(
    config: &Config,
    current_branch: &str,
    branch: &str,
    location: &BranchLocation,
) -> Result<Vec<String>> {
    let repo = &config.repo_root;
    if branch == current_branch {
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

/// Plan and apply the deletion of one branch, returning printable lines.
///
/// The plan is recomputed here rather than carried over from the modal: the
/// preview's commit counts are UI, and the authority to delete has to come from
/// the repo as it is *now*. If it changed underneath the modal, the core refuses
/// and says so.
fn run_delete(
    config: &Config,
    branch: &str,
    scope: DeleteScope,
    force: bool,
) -> Result<Vec<String>> {
    let plan = ops::delete_branch_plan(config, branch, &prune_scope_for(scope, force))?;
    if plan.is_empty() {
        let mut lines = vec![format!("nothing to delete for '{branch}'")];
        push_prune_skips(&mut lines, &plan.skipped);
        return Ok(lines);
    }
    let results = ops::prune_apply(config, &plan)?;
    Ok(render_prune_outcome(&plan, &results))
}

/// Run `ops::verify` and render its outcome, mirroring `main.rs`'s `cmd_verify`
/// line for line — the same checks in the same order, so a verdict in the panel
/// and a verdict in the terminal never disagree.
///
/// Two deliberate differences, both because this is the TUI:
///
/// - no version *bump*. `cmd_verify` offers one before the gates run; here the
///   gate failure points at `[b]`, which is the key that already does it and the
///   only place a bump commit is written from.
/// - nothing is forced and nothing is dry-run. `[v]` has no flags to carry.
///
/// A failed host or an unsatisfied version gate is an `Err`, not a line: the
/// panel marks a failed job, and a verdict that reads as "done" when it is
/// really "blocked" is the one outcome worth being loud about. Everything the
/// gates printed is already in the panel either way, streamed as they ran.
fn run_verify(config: &Config) -> Result<Vec<String>> {
    ops::ensure_clean_state(config)?;
    let outcome = ops::verify(config, false)?;

    let mut lines = Vec::new();
    if outcome.diverged_note {
        lines.push(format!(
            "note: '{}' has commits not in '{}'; graduation/promotion will create a --no-ff merge",
            outcome.target, outcome.source
        ));
    }
    push_version_check(
        &mut lines,
        &outcome.version,
        &outcome.source,
        &outcome.target,
    );
    push_gate_notices(&mut lines, &outcome.gate_notices);
    push_gate_notices(&mut lines, &outcome.host_notices);
    push_host_results(&mut lines, &outcome.host_results);

    if !outcome.failed_hosts.is_empty() {
        bail!(
            "host verification failed: {}",
            outcome.failed_hosts.join(", ")
        );
    }
    if !outcome.version.is_satisfied() {
        return Err(anyhow!(
            "{}\npress [b] to bump the version on '{}'",
            ops::version_gate_error(&outcome.version, &outcome.source, &outcome.target),
            outcome.source
        ));
    }
    lines.push(format!(
        "Verification passed: {} -> {}",
        outcome.source, outcome.target
    ));
    Ok(lines)
}

/// Drive a workflow operation through `core::ops`, rendering its structured
/// outcome into printable lines. Never runs dry and never forces.
fn run_op(config: &Config, action: Action, target: Option<&str>) -> Result<Vec<String>> {
    let force = ops::ForceOpts::new(false, None)?;
    let mut lines = Vec::new();
    match action {
        Action::Graduate => {
            let roll = target.ok_or_else(|| anyhow!("no roll selected"))?;
            ops::ensure_clean_state(config)?;
            let o = ops::graduate(config, roll, false, &force)?;
            push_gate_notices(&mut lines, &o.gate_notices);
            lines.push(format!("Graduated '{}' into '{}'", o.roll, o.rolling));
        }
        Action::Integrate => {
            let roll = target.ok_or_else(|| anyhow!("no roll selected"))?;
            // Deliberately no `ensure_clean_state`: this is the same operation as
            // `rf integrate`, and git already refuses a merge that would clobber
            // local changes while carrying harmless ones through. A conflict is a
            // legitimate outcome to go and resolve, not a reason to refuse up
            // front.
            let o = ops::integrate(config, roll).map_err(|err| integrate_error(config, err))?;
            lines.push(format!("Integrated '{}' into '{}'", o.branch, o.current));
        }
        Action::Promote => {
            ops::ensure_clean_state(config)?;
            let promote_target = match target {
                Some(roll) => ops::PromoteTarget::Rolls(vec![roll.to_string()]),
                None => ops::PromoteTarget::Rolling,
            };
            // Tagging is on; the version gate hard-fails here rather than
            // prompting, since the TUI has no place to offer a bump — the
            // error names the `rf promote --bump` fix.
            let o = ops::promote(config, &promote_target, false, &force, true)?;
            for step in &o.steps {
                push_gate_notices(&mut lines, &step.gate_notices);
                push_gate_notices(&mut lines, &step.host_notices);
                push_host_results(&mut lines, &step.host_results);
                let what = step.roll.as_deref().unwrap_or(&o.rolling);
                lines.push(format!("Promoted '{}' into '{}'", what, o.stable));
                if let Some(line) = step.tag.describe() {
                    lines.push(line);
                }
            }
            for skip in &o.skipped {
                lines.push(format!("skipped '{}': {}", skip.roll, skip.reason));
            }
        }
        Action::Update => match ops::update(config, false)? {
            ops::UpdateOutcome::NoActiveRolls => {
                lines.push("no active local rolls to update".to_string());
            }
            ops::UpdateOutcome::Ran { stable, items } => {
                for item in items {
                    match item {
                        ops::UpdateItem::AlreadyUpToDate { roll } => {
                            lines.push(format!("'{roll}' is already up to date with '{stable}'"));
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
            let plan = ops::prune_plan(config, &ops::PruneScope::both())?;
            if plan.is_empty() {
                lines.push("no promoted roll branches to prune".to_string());
                push_prune_skips(&mut lines, &plan.skipped);
            } else {
                let results = ops::prune_apply(config, &plan)?;
                lines.extend(render_prune_outcome(&plan, &results));
            }
        }
        Action::Tidy => {
            // Same shape as prune above, and the same refusal to force: a local
            // branch holding commits found nowhere else is reported as skipped,
            // and clearing it needs `rf tidy --force` from the CLI. `tidy(false)`
            // also fixes `remote: false`, which is what keeps the TUI out of the
            // three places allowed to delete a ref on origin.
            let plan = ops::tidy_plan(config, &ops::PruneScope::tidy(false), &TIDY_STATES)?;
            if plan.is_empty() {
                lines.push("no local roll branches to tidy".to_string());
                push_prune_skips(&mut lines, &plan.skipped);
            } else {
                let results = ops::prune_apply(config, &plan)?;
                lines.extend(render_prune_outcome(&plan, &results));
            }
        }
    }
    Ok(lines)
}

/// Add a recovery hint to a failed integrate when it left a merge in progress.
///
/// A conflicted merge is the common failure, and the raw
/// ``  `git merge --no-ff X` exited with exit status: 1`` says nothing about the
/// worktree now being mid-merge — which is the only fact the user needs next.
/// `MERGE_HEAD` exists exactly while a merge is unresolved, so `ref_exists`
/// answers this without a new git helper.
fn integrate_error(config: &Config, err: anyhow::Error) -> anyhow::Error {
    if git::ref_exists(&config.repo_root, "MERGE_HEAD") {
        return anyhow!(
            "{err}\nmerge left conflicts — resolve them and commit,\n\
             press gg for lazygit, or run `git merge --abort`"
        );
    }
    err
}

/// Read every local branch's upstream tracking state, keyed by branch name.
///
/// One `for-each-ref` for the whole repo, not one `ahead_behind` per row: the
/// sync column needs an answer for every visible branch on every reload. A
/// failure here degrades the column to `—` rather than failing the reload —
/// divergence is decoration, and losing it must not cost the user their table.
fn load_tracking(config: &Config) -> HashMap<String, git::LocalBranch> {
    git::local_branch_details(&config.repo_root)
        .map(|branches| branches.into_iter().map(|b| (b.name.clone(), b)).collect())
        .unwrap_or_default()
}

/// The tracking state to show for `branch`, or `None` when it has no local copy
/// in the batch — which the `sync` column renders as a dash rather than as
/// "in sync".
fn track_of(tracking: &HashMap<String, git::LocalBranch>, branch: &str) -> Option<TrackState> {
    tracking.get(branch).map(git::track_state)
}

/// Render the version-bump picker.
///
/// Every level shows the version it would produce, not just its name: "minor" is
/// abstract, `0.2.0 → 0.3.0` is not, and seeing all three at once is what makes
/// picking the right one obvious.
fn render_bump_modal(f: &mut Frame, area: Rect, current: Semver, branch: &str) {
    let mut lines = vec![
        Line::from(vec![
            Span::raw("Current: "),
            Span::styled(
                current.to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("  on {branch}")),
        ]),
        Line::from(""),
    ];
    for (i, (level, next)) in bump_previews(current).into_iter().enumerate() {
        lines.push(Line::from(vec![
            Span::styled(format!("[{}] ", i + 1), Style::default().fg(Color::Yellow)),
            // No padding needed for the arrows to line up: "patch", "minor" and
            // "major" are all five characters. (A width spec would be ignored
            // anyway — `BumpLevel`'s `Display` writes straight to the formatter
            // rather than going through `pad`.)
            Span::raw(format!("{level} → ")),
            Span::styled(
                next.to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "[n] cancel",
        Style::default().fg(Color::Yellow),
    )));

    let width = lines
        .iter()
        .map(|l| l.width())
        .max()
        .unwrap_or(32)
        .clamp(28, 70) as u16;
    let modal = centered_rect(area, width + 4, lines.len() as u16 + 2);
    f.render_widget(Clear, modal);
    f.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(Span::styled(
            " bump version ",
            Style::default().add_modifier(Modifier::BOLD),
        ))),
        modal,
    );
}

/// Render the force-push confirmation.
///
/// Red-bordered and stating the divergence in commits, because this is the one
/// key in the view that can destroy commits on the remote. The counts come from
/// the tracking batch, so the prompt says what would actually be overwritten
/// rather than asking in the abstract.
fn render_force_push_modal(
    f: &mut Frame,
    area: Rect,
    branch: &str,
    remote: &str,
    details: Option<&git::LocalBranch>,
) {
    let divergence = match details.map(git::track_state) {
        Some(TrackState::Diverged { ahead, behind }) => format!(
            "{remote}/{branch} has {behind} commit{} you don't; you have {ahead}.",
            plural(behind)
        ),
        Some(TrackState::Behind(behind)) => format!(
            "{remote}/{branch} has {behind} commit{} you don't.",
            plural(behind)
        ),
        // Reached when git refused a push we could not predict — say so plainly
        // rather than inventing a count.
        _ => format!("{remote}/{branch} has commits you don't."),
    };

    let lines = vec![
        Line::from(Span::styled(
            format!("Force-push {branch}?"),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(divergence),
        Line::from(Span::styled(
            "Uses --force-with-lease: refused if origin moved since your last fetch.",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "[y] force    [n] cancel",
            Style::default().fg(Color::Yellow),
        )),
    ];

    let width = lines
        .iter()
        .map(|l| l.width())
        .max()
        .unwrap_or(40)
        .clamp(32, 78) as u16;
    let modal = centered_rect(area, width + 4, lines.len() as u16 + 2);
    f.render_widget(Clear, modal);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .title(Span::styled(
                    " force push ",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ))
                .border_style(Style::default().fg(Color::Red)),
        ),
        modal,
    );
}

/// `"s"` unless `n` is 1 — used so the force prompt reads as prose.
fn plural(n: u32) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
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
    current_branch: &str,
) {
    let prompt = match action {
        Action::Graduate => format!(
            "Graduate {} into {}?",
            target.unwrap_or("(selected roll)"),
            config.rolling_branch
        ),
        Action::Integrate => format!(
            "Integrate {} into {}?",
            target.unwrap_or("(selected roll)"),
            current_branch
        ),
        // `target` is the selected roll when `[p]` was pressed on one, and
        // `None` for a whole-branch promotion — the prompt must say which,
        // because the two differ enormously in what they land on stable.
        Action::Promote => match target {
            Some(roll) => format!("Promote {} into {}?", roll, config.stable_branch),
            None => format!(
                "Promote all of {} into {}?",
                config.rolling_branch, config.stable_branch
            ),
        },
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
        // Says "local only" where prune says "local + origin": the two keys sit
        // next to each other and differ in exactly that, so the prompt is where
        // the difference has to be visible.
        Action::Tidy => {
            let n = tidyable_count(rolls);
            format!(
                "Delete {n} local graduated/promoted roll branch{} (local only)?",
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

/// Render the centered delete popup. Its shape follows `preview.prompt`, and
/// the border is red throughout so the destructive modal is never mistaken for
/// the ordinary `" confirm "` one.
///
/// Sized from the built lines rather than a fixed height, because the
/// force-confirm stage adds a warning line the other shapes do not have.
fn render_delete_modal(f: &mut Frame, area: Rect, config: &Config, preview: &DeletePreview) {
    let red = Style::default().fg(Color::Red);
    let dim = Style::default().fg(Color::DarkGray);

    let mut lines: Vec<Line> = Vec::new();
    let title = match preview.prompt {
        DeletePrompt::ForceConfirm(_) => " force delete ",
        _ => " delete ",
    };

    match preview.prompt {
        DeletePrompt::Single(DeleteScope::Local) => {
            lines.push(Line::from(format!(
                "Delete local branch {}?",
                preview.branch
            )));
        }
        DeletePrompt::Single(DeleteScope::Remote) | DeletePrompt::Single(DeleteScope::Both) => {
            lines.push(Line::from(format!("Delete origin/{}?", preview.branch)));
            if preview.local_is_checked_out {
                lines.push(Line::from(Span::styled(
                    "local copy is checked out — origin only",
                    dim,
                )));
            }
        }
        DeletePrompt::Choice => {
            lines.push(Line::from(format!(
                "Delete {} — it exists locally and on origin.",
                preview.branch
            )));
        }
        DeletePrompt::ForceConfirm(scope) => {
            lines.push(Line::from(format!(
                "Delete {} ({})",
                preview.branch,
                scope_label(scope)
            )));
            lines.push(Line::from(Span::styled(
                match unmerged_for_scope(scope, preview) {
                    Some(n) => format!(
                        "⚠ {n} commit{} not in {} will be lost — this cannot be undone",
                        if n == 1 { "" } else { "s" },
                        config.stable_branch
                    ),
                    None => format!(
                        "⚠ containment in {} is unknown — commits may be lost",
                        config.stable_branch
                    ),
                },
                red,
            )));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        match preview.prompt {
            DeletePrompt::Choice => "[l] local only   [r] origin only   [b] both   [n] neither",
            DeletePrompt::ForceConfirm(_) => "[y] delete anyway    [n] cancel",
            DeletePrompt::Single(_) => "[y] delete    [n] cancel",
        },
        dim,
    )));

    let width = lines.iter().map(|l| l.width()).max().unwrap_or(20) as u16 + 4;
    let height = lines.len() as u16 + 2;
    let modal = centered_rect(area, width.max(32), height);

    f.render_widget(Clear, modal);
    let body = Paragraph::new(lines)
        .alignment(Alignment::Center)
        .block(Block::bordered().border_style(red).title(title));
    f.render_widget(body, modal);
}

/// Human wording for which copies a scope covers, used in the force-confirm
/// line and nowhere else.
fn scope_label(scope: DeleteScope) -> &'static str {
    match scope {
        DeleteScope::Local => "local",
        DeleteScope::Remote => "origin",
        DeleteScope::Both => "local + origin",
    }
}

/// The status bar's one hint line, built from the `basic` bindings so it cannot
/// drift from what the keys actually are.
pub(crate) fn basic_hints() -> String {
    let mut line = String::new();
    for hint in BINDINGS.iter().filter_map(|b| b.hint) {
        // One leading space to clear the edge, three between fragments.
        line.push_str(if line.is_empty() { " " } else { "   " });
        line.push_str(hint);
    }
    line
}

/// Render the `?` keymap: a filter line, the matching bindings, and the keys
/// that drive it.
///
/// Sized to the terminal rather than to the list — twenty-odd bindings do not
/// fit an 80×24 screen, and the window scrolls to keep the cursor in view. The
/// selected row is marked *and* styled, so it stays visible on terminals that
/// drop the background colour.
fn render_help(f: &mut Frame, area: Rect, query: &str, cursor: usize) {
    let matches = filter_bindings(query);
    let dim = Style::default().fg(Color::DarkGray);

    // Both columns are sized from the whole table, not from what is showing, so
    // the box keeps its shape as the query narrows it. A modal that resizes on
    // every keystroke is unreadable to type into.
    let key_width = column_width(BINDINGS.iter().map(|b| b.keys));
    let label_width = column_width(BINDINGS.iter().map(|b| b.label));

    // Rows the list itself gets: the frame is two borders, the query line, a
    // blank, the overflow line, a blank and the hint. Budgeting for the
    // overflow line whether or not it appears keeps the box one height.
    let visible = (area.height.saturating_sub(8) as usize).max(1);
    let rows = matches.len().min(visible);
    // Scroll only once the cursor leaves the window, so a short list never jumps
    // around under the eye.
    let first = cursor.saturating_sub(rows.saturating_sub(1));

    let mut lines = vec![
        Line::from(vec![
            Span::styled("> ", Style::default().fg(Color::Cyan)),
            Span::raw(query.to_string()),
            Span::styled("_", Style::default().fg(Color::Cyan)),
        ]),
        Line::from(""),
    ];

    if matches.is_empty() {
        lines.push(Line::from(Span::styled("no key matches", dim)));
    }
    for (row, &index) in matches.iter().enumerate().skip(first).take(rows) {
        let binding = &BINDINGS[index];
        let selected = row == cursor;
        let label_style = if selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            // Marked as well as styled: the cursor has to survive a terminal
            // that drops the reverse attribute.
            Span::raw(if selected { "▶ " } else { "  " }),
            Span::styled(
                format!("{:<key_width$}", binding.keys),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(format!("{:<label_width$}", binding.label), label_style),
            Span::raw("  "),
            Span::styled(binding.group, dim),
        ]));
    }
    if matches.len() > rows {
        lines.push(Line::from(Span::styled(
            format!("… {} more, keep typing", matches.len() - rows),
            dim,
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "type to filter   [↑/↓] move   [enter] run   [esc] close",
        dim,
    )));

    let width = lines.iter().map(|l| l.width()).max().unwrap_or(40) as u16 + 4;
    let height = lines.len() as u16 + 2;
    let modal = centered_rect(area, width.max(52), height);

    f.render_widget(Clear, modal);
    f.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" keys ")),
        modal,
    );
}

/// The display width of the widest of `values`, for padding a column to it.
fn column_width<'a>(values: impl Iterator<Item = &'a str>) -> usize {
    values.map(|v| v.chars().count()).max().unwrap_or(0)
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

/// Render what a deletion actually did, as printable lines.
///
/// Shared by `[x] prune` and `[d]elete` so there is one result vocabulary for
/// branch deletion in the TUI, matching `main.rs`'s `render_prune_results`.
fn render_prune_outcome(plan: &ops::PrunePlan, results: &[ops::PruneResult]) -> Vec<String> {
    let mut lines = Vec::new();
    for result in results {
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
    push_prune_skips(&mut lines, &plan.skipped);
    lines
}

/// Append the copies a deletion declined to touch. Nothing is skipped silently.
fn push_prune_skips(lines: &mut Vec<String>, skipped: &[ops::PruneSkip]) {
    for skip in skipped {
        lines.push(format!("skipped '{}': {}", skip.branch, skip.reason));
    }
}

/// Append the per-host verification summary as readable lines (mirrors
/// `main.rs::render_host_results`). The TUI used to drop this on the floor, so a
/// host-gated repo learned less from `[p]` than from `rf promote`.
fn push_host_results(lines: &mut Vec<String>, results: &[ops::HostResult]) {
    if results.is_empty() {
        return;
    }
    lines.push("Host verification:".to_string());
    for result in results {
        let status = if result.passed() { "PASSED" } else { "FAILED" };
        lines.push(format!("  {}: {status}", result.host));
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
            dependents: Vec::new(),
            graduation_commit: None,
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
            version_gate: true,
            tag_on_promote: true,
            push_tag: true,
            roll_to_rolling_gates: Vec::new(),
            rolling_to_main_gates: Vec::new(),
            host_gates: Vec::new(),
            clean_protect: Vec::new(),
            pull_mode: Default::default(),
            lazygit_command: "lazygit".to_string(),
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
            dependents: Vec::new(),
            graduation_commit: None,
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
        assert!(validate_action(Action::Prune, None, &unpromoted, "main", "roll/").is_err());

        let mut with_promoted = unpromoted.clone();
        with_promoted.push(roll_n(5, RollState::Promoted));
        with_promoted.push(roll_n(6, RollState::Promoted));
        assert!(can_prune(&with_promoted));
        assert_eq!(prunable_count(&with_promoted), 2);
        // Prune is repo-wide, so it validates with no selection.
        assert!(validate_action(Action::Prune, None, &with_promoted, "main", "roll/").is_ok());
    }

    #[test]
    fn tidy_valid_only_for_local_graduated_or_promoted_rolls() {
        // States tidy does not clear, whatever their location.
        let wrong_state = vec![
            roll_n(1, RollState::Active),
            roll_n(2, RollState::Diverged),
            roll_n(3, RollState::Blocked),
        ];
        assert!(!can_tidy(&wrong_state));
        assert_eq!(tidyable_count(&wrong_state), 0);
        assert!(validate_action(Action::Tidy, None, &wrong_state, "main", "roll/").is_err());

        // Right state, but no local copy to delete — tidy never touches origin.
        let remote_only = vec![RollInfo {
            location: BranchLocation::Remote,
            ..roll_n(4, RollState::Graduated)
        }];
        assert!(!can_tidy(&remote_only));

        let mut tidyable = wrong_state.clone();
        tidyable.push(roll_n(5, RollState::Graduated));
        tidyable.push(RollInfo {
            location: BranchLocation::Both,
            ..roll_n(6, RollState::Promoted)
        });
        assert!(can_tidy(&tidyable));
        assert_eq!(tidyable_count(&tidyable), 2);
        // Repo-wide like prune, so it validates with no selection.
        assert!(validate_action(Action::Tidy, None, &tidyable, "main", "roll/").is_ok());
    }

    /// The keys of the bindings `query` matches, best first.
    fn matched_keys(query: &str) -> Vec<&'static str> {
        filter_bindings(query)
            .into_iter()
            .map(|i| BINDINGS[i].keys)
            .collect()
    }

    #[test]
    fn the_search_ranks_the_key_you_meant_first() {
        // The point of fuzzy over substring: a half-remembered word against a
        // label nobody read, with the right row at the top.
        assert_eq!(matched_keys("prune")[0], "x");
        assert_eq!(matched_keys("lazygit")[0], "gg");
        assert_eq!(matched_keys("bump")[0], "b");
        // By the key itself, which is the other way people search.
        assert_eq!(matched_keys("gg")[0], "gg");
        // A group name lists the whole group, and nothing outside it.
        let sync = matched_keys("sync");
        assert!(sync.len() >= 4, "{sync:?}");
        for keys in ["p", "P", "f", "gg"] {
            assert!(sync.contains(&keys), "{keys} missing from {sync:?}");
        }
        // An empty query is everything, in table order.
        assert_eq!(matched_keys("").len(), BINDINGS.len());
        assert_eq!(matched_keys("")[0], BINDINGS[0].keys);
        // And a query that matches nothing says so rather than falling back to
        // showing everything.
        assert!(matched_keys("zzzz").is_empty());
    }

    #[test]
    fn fuzzy_scoring_prefers_runs_and_word_starts() {
        // A contiguous run beats the same letters scattered.
        let run = fuzzy_score("prune branches", "prune").unwrap();
        let scattered = fuzzy_score("push remote under new entry", "prune").unwrap();
        assert!(run > scattered, "run {run} !> scattered {scattered}");

        // Matching at the start of a word beats matching mid-word.
        let word_start = fuzzy_score("push branch", "pb").unwrap();
        let mid_word = fuzzy_score("supbar", "pb").unwrap();
        assert!(word_start > mid_word, "{word_start} !> {mid_word}");

        // Order is required — it is a subsequence match, not a bag of letters.
        assert!(fuzzy_score("push", "hsup").is_none());
        assert_eq!(fuzzy_score("anything", ""), Some(0));
    }

    #[test]
    fn typing_in_the_list_filters_and_enter_runs_what_is_under_the_cursor() {
        let mut query = String::new();
        let mut cursor = 0;

        for c in "prune".chars() {
            assert_eq!(
                handle_help_key(&mut query, &mut cursor, KeyCode::Char(c)),
                HelpOutcome::Continue
            );
        }
        assert_eq!(query, "prune");

        let expected = filter_bindings("prune")[0];
        assert_eq!(
            handle_help_key(&mut query, &mut cursor, KeyCode::Enter),
            HelpOutcome::Run(expected)
        );
        assert_eq!(BINDINGS[expected].keys, "x");
    }

    #[test]
    fn editing_the_query_puts_the_cursor_back_on_the_top_row() {
        // Otherwise enter runs whatever happens to be at index 3 of a list the
        // user has just changed out from under it.
        let mut query = "s".to_string();
        let mut cursor = 0;
        handle_help_key(&mut query, &mut cursor, KeyCode::Down);
        handle_help_key(&mut query, &mut cursor, KeyCode::Down);
        assert_eq!(cursor, 2);

        handle_help_key(&mut query, &mut cursor, KeyCode::Char('y'));
        assert_eq!(cursor, 0);

        handle_help_key(&mut query, &mut cursor, KeyCode::Down);
        handle_help_key(&mut query, &mut cursor, KeyCode::Backspace);
        assert_eq!(cursor, 0);
        assert_eq!(query, "s");
    }

    #[test]
    fn the_cursor_cannot_leave_the_filtered_list() {
        let mut query = "lazygit".to_string();
        let mut cursor = 0;
        assert_eq!(filter_bindings(&query).len(), 1);

        // Past the end clamps to the last row rather than selecting nothing...
        for _ in 0..5 {
            handle_help_key(&mut query, &mut cursor, KeyCode::Down);
        }
        assert_eq!(cursor, 0);
        // ...and past the top clamps to the first.
        handle_help_key(&mut query, &mut cursor, KeyCode::Up);
        assert_eq!(cursor, 0);

        // Enter on a query that matches nothing closes rather than trapping.
        let mut empty = "zzzz".to_string();
        let mut at = 0;
        assert_eq!(
            handle_help_key(&mut empty, &mut at, KeyCode::Enter),
            HelpOutcome::Close
        );
    }

    #[test]
    fn esc_closes_the_list_and_stray_keys_leave_it_open() {
        let mut query = String::new();
        let mut cursor = 0;
        assert_eq!(
            handle_help_key(&mut query, &mut cursor, KeyCode::Esc),
            HelpOutcome::Close
        );
        // A key the list has no use for must not close it — the query survives.
        query.push_str("push");
        assert_eq!(
            handle_help_key(&mut query, &mut cursor, KeyCode::Tab),
            HelpOutcome::Continue
        );
        assert_eq!(query, "push");
    }

    #[test]
    fn the_list_shows_the_matches_and_the_keys_that_drive_it() {
        let out = draw(|f, area| render_help(f, area, "prune", 0));
        assert!(out.contains("> prune"), "{out}");
        assert!(out.contains("prune promoted roll branches"), "{out}");
        assert!(out.contains("[enter] run"), "{out}");
        assert!(out.contains("[esc] close"), "{out}");

        // A query with no match says so rather than rendering an empty box.
        let none = draw(|f, area| render_help(f, area, "zzzz", 0));
        assert!(none.contains("no key matches"), "{none}");
    }

    #[test]
    fn verify_reads_head_and_resolves_the_route_from_its_tier() {
        let cfg = config("main", "rolling");

        // A roll branch checks its graduation into rolling...
        assert_eq!(
            verify_route_for(&cfg, "roll/4-0918-x"),
            Some(("roll/4-0918-x".to_string(), "rolling".to_string()))
        );
        // ...and rolling checks its promotion to stable.
        assert_eq!(
            verify_route_for(&cfg, "rolling"),
            Some(("rolling".to_string(), "main".to_string()))
        );
        // Stable itself has nowhere to go, and neither does anything off the
        // tiers — `[v]` says so rather than guessing a route.
        assert_eq!(verify_route_for(&cfg, "main"), None);
        assert_eq!(verify_route_for(&cfg, "feature/whatever"), None);
    }

    #[test]
    fn the_version_line_matches_the_one_the_cli_prints() {
        let check = |status, head: Option<Semver>, base: Option<Semver>| {
            let mut lines = Vec::new();
            push_version_check(
                &mut lines,
                &VersionCheck { head, base, status },
                "rolling",
                "main",
            );
            lines
        };

        assert_eq!(
            check(VersionStatus::Unchanged, Some(v(0, 2, 3)), Some(v(0, 2, 3))),
            vec!["Version: 0.2.3 on 'rolling' (base 'main' 0.2.3) UNCHANGED"]
        );
        // A repo with no `Cargo.toml` says nothing at all, rather than reporting
        // a comparison it did not make.
        assert!(check(VersionStatus::NotApplicable, None, None).is_empty());
        // And an unreadable version still reports both sides, naming which one
        // could not be read.
        assert_eq!(
            check(VersionStatus::Unreadable, None, Some(v(1, 0, 0))),
            vec!["Version: <unreadable> on 'rolling' (base 'main' 1.0.0) UNREADABLE"]
        );
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
        assert!(validate_action(Action::Graduate, None, &active, "main", "roll/").is_err());
        assert!(
            validate_action(Action::Graduate, Some(&active[0]), &active, "main", "roll/").is_ok()
        );
        assert!(validate_action(
            Action::Graduate,
            Some(&graduated[0]),
            &graduated,
            "main",
            "roll/"
        )
        .is_err());

        // Promote needs a graduated roll on rolling.
        assert!(validate_action(Action::Promote, None, &active, "main", "roll/").is_err());
        assert!(validate_action(Action::Promote, None, &graduated, "main", "roll/").is_ok());

        // Update needs a local active roll.
        assert!(validate_action(Action::Update, None, &active, "main", "roll/").is_ok());
        assert!(validate_action(Action::Update, None, &graduated, "main", "roll/").is_err());
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
    fn promote_target_is_the_selected_graduated_roll() {
        let rolls = vec![
            roll_n(1, RollState::Graduated),
            roll_n(2, RollState::Diverged),
        ];
        assert_eq!(
            promote_target_for(Some(&rolls[0]), &rolls),
            Ok(Some("roll/1-0101-x".to_string()))
        );
        // Diverged still has a graduation commit to advance stable to.
        assert_eq!(
            promote_target_for(Some(&rolls[1]), &rolls),
            Ok(Some("roll/2-0101-x".to_string()))
        );
    }

    #[test]
    fn promote_target_is_the_whole_branch_without_a_roll_selected() {
        // A base-branch row resolves to `None` the same way an empty selection
        // does, which is how `[p]` on the rolling row promotes everything.
        let rolls = vec![roll_n(1, RollState::Graduated)];
        assert_eq!(promote_target_for(None, &rolls), Ok(None));
    }

    #[test]
    fn promote_target_refuses_rolls_with_nothing_to_promote() {
        for state in [RollState::Active, RollState::Blocked, RollState::Promoted] {
            let rolls = vec![roll_n(1, state.clone())];
            let err =
                promote_target_for(Some(&rolls[0]), &rolls).expect_err("should refuse {state:?}");
            assert!(
                err.contains("roll/1-0101-x"),
                "the message should name the roll: {err}"
            );
        }
    }

    #[test]
    fn promote_target_refusal_does_not_widen_to_the_whole_branch() {
        // The dangerous failure mode: pressing [p] on an active roll must not
        // fall back to promoting everything, which is far more than was asked.
        let rolls = vec![
            roll_n(1, RollState::Graduated),
            roll_n(2, RollState::Active),
        ];
        assert!(promote_target_for(Some(&rolls[1]), &rolls).is_err());
        assert!(
            validate_action(Action::Promote, Some(&rolls[1]), &rolls, "main", "roll/").is_err()
        );
    }

    #[test]
    fn promote_is_rejected_when_nothing_has_graduated() {
        let rolls = vec![roll_n(1, RollState::Active)];
        assert!(promote_target_for(None, &rolls).is_err());
    }

    #[test]
    fn dependent_rows_lists_every_roll_that_integrated_target() {
        // rolls 17 and 18 both integrated roll 14 → both are 14's dependents.
        let mut r17 = roll_n(17, RollState::Active);
        r17.deps = vec![14];
        let mut r18 = roll_n(18, RollState::Blocked);
        r18.deps = vec![14, 15];
        let mut target = roll_n(14, RollState::Active);
        target.dependents = vec![17, 18];
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
    fn dependent_rows_lists_graduated_dependents_too() {
        // The regression this whole change exists for: a dependant that has
        // already graduated must still show up. Before deps were computed for
        // non-active rolls, `dependents` was empty here and the link vanished.
        let mut r2 = roll_n(2, RollState::Graduated);
        r2.deps = vec![3];
        let mut target = roll_n(3, RollState::Graduated);
        target.dependents = vec![2];
        let all = vec![r2, target.clone()];

        let rows = dependent_rows(&target, &all);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].number, 2);
        assert_eq!(rows[0].state, RollState::Graduated);
        // A graduated target no longer gates anything.
        assert!(!rows[0].is_blocker);
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
    fn dependent_rows_skips_numbers_absent_from_the_list() {
        // A dependant that is not in `all` (filtered out, or a stale index)
        // must be skipped rather than rendered as a blank row.
        let mut target = roll_n(14, RollState::Active);
        target.dependents = vec![15, 99];
        let all = vec![target.clone(), roll_n(15, RollState::Active)];

        let rows = dependent_rows(&target, &all);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].number, 15);
    }

    #[test]
    fn dependent_rows_never_lists_target_itself() {
        // A self-referential entry must not turn the roll into its own
        // dependent.
        let mut target = roll_n(14, RollState::Active);
        target.dependents = vec![14];
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

    // ── delete ──────────────────────────────────────────────────────────

    /// A preview with the given per-copy unmerged counts, for the force-confirm
    /// and key-handling tests.
    fn preview(
        prompt: DeletePrompt,
        local_unmerged: Option<u32>,
        remote_unmerged: Option<u32>,
    ) -> DeletePreview {
        DeletePreview {
            branch: "roll/1-0101-x".to_string(),
            prompt,
            local_unmerged,
            remote_unmerged,
            local_is_checked_out: false,
        }
    }

    #[test]
    fn delete_prompt_shape_follows_location() {
        let single = |loc| delete_prompt(&roll(RollState::Active, loc));
        assert_eq!(
            single(BranchLocation::Local),
            Ok(DeletePrompt::Single(DeleteScope::Local))
        );
        assert_eq!(
            single(BranchLocation::Remote),
            Ok(DeletePrompt::Single(DeleteScope::Remote))
        );
        assert_eq!(single(BranchLocation::Both), Ok(DeletePrompt::Choice));
        assert!(
            single(BranchLocation::Neither).is_err(),
            "a row with no copies anywhere has nothing to delete"
        );
    }

    #[test]
    fn delete_offered_regardless_of_roll_state() {
        // The rule delete relaxes relative to prune: the user named this row.
        for state in [
            RollState::Active,
            RollState::Graduated,
            RollState::Diverged,
            RollState::Promoted,
            RollState::Blocked,
        ] {
            assert_eq!(
                delete_prompt(&roll(state.clone(), BranchLocation::Local)),
                Ok(DeletePrompt::Single(DeleteScope::Local)),
                "{state:?} should still be deletable"
            );
        }
    }

    #[test]
    fn delete_prompt_never_offers_the_checked_out_local_copy() {
        let mut local = roll(RollState::Active, BranchLocation::Local);
        local.is_current = true;
        let err = delete_prompt(&local).expect_err("checked-out local-only roll");
        assert!(err.contains("checked out"), "should explain itself: {err}");

        // The origin copy of a checked-out roll is still fair game.
        let mut both = roll(RollState::Active, BranchLocation::Both);
        both.is_current = true;
        assert_eq!(
            delete_prompt(&both),
            Ok(DeletePrompt::Single(DeleteScope::Remote)),
            "a checked-out both-location roll degrades to origin-only"
        );
    }

    #[test]
    fn delete_key_single_prompt_defaults_to_no() {
        let prompt = DeletePrompt::Single(DeleteScope::Local);
        for key in [KeyCode::Char('y'), KeyCode::Char('Y')] {
            assert_eq!(
                handle_delete_key(prompt, key),
                DeleteOutcome::Confirm(DeleteScope::Local)
            );
        }
        for key in [KeyCode::Char('n'), KeyCode::Char('N'), KeyCode::Esc] {
            assert_eq!(handle_delete_key(prompt, key), DeleteOutcome::Cancel);
        }
        // Nothing else acts — that is what "defaults to No" means here. Enter in
        // particular is not a shortcut for yes.
        for key in [
            KeyCode::Enter,
            KeyCode::Char(' '),
            KeyCode::Char('l'),
            KeyCode::Char('b'),
            KeyCode::Char('d'),
        ] {
            assert_eq!(
                handle_delete_key(prompt, key),
                DeleteOutcome::Ignore,
                "{key:?} must not delete anything"
            );
        }
    }

    #[test]
    fn delete_key_choice_prompt_maps_l_r_b_n() {
        let p = DeletePrompt::Choice;
        assert_eq!(
            handle_delete_key(p, KeyCode::Char('l')),
            DeleteOutcome::Confirm(DeleteScope::Local)
        );
        assert_eq!(
            handle_delete_key(p, KeyCode::Char('R')),
            DeleteOutcome::Confirm(DeleteScope::Remote)
        );
        assert_eq!(
            handle_delete_key(p, KeyCode::Char('b')),
            DeleteOutcome::Confirm(DeleteScope::Both)
        );
        for key in [KeyCode::Char('n'), KeyCode::Esc] {
            assert_eq!(handle_delete_key(p, key), DeleteOutcome::Cancel);
        }
        // `y` is deliberately unbound here: mapping it to "both" would let
        // muscle memory delete more than the user was looking at.
        assert_eq!(
            handle_delete_key(p, KeyCode::Char('y')),
            DeleteOutcome::Ignore
        );
    }

    #[test]
    fn delete_force_stage_requires_a_fresh_yes() {
        let p = DeletePrompt::ForceConfirm(DeleteScope::Both);
        assert_eq!(
            handle_delete_key(p, KeyCode::Char('y')),
            DeleteOutcome::Confirm(DeleteScope::Both)
        );
        for key in [KeyCode::Char('n'), KeyCode::Esc] {
            assert_eq!(handle_delete_key(p, key), DeleteOutcome::Cancel);
        }
        for key in [KeyCode::Char('l'), KeyCode::Char('r'), KeyCode::Char('b')] {
            assert_eq!(handle_delete_key(p, key), DeleteOutcome::Ignore);
        }
    }

    #[test]
    fn needs_force_confirm_only_when_a_selected_copy_is_uncontained() {
        let clean = preview(DeletePrompt::Choice, Some(0), Some(0));
        assert!(!needs_force_confirm(DeleteScope::Local, &clean));
        assert!(!needs_force_confirm(DeleteScope::Remote, &clean));
        assert!(!needs_force_confirm(DeleteScope::Both, &clean));

        let dirty_local = preview(DeletePrompt::Choice, Some(3), Some(0));
        assert!(needs_force_confirm(DeleteScope::Local, &dirty_local));
        assert!(!needs_force_confirm(DeleteScope::Remote, &dirty_local));
        assert!(needs_force_confirm(DeleteScope::Both, &dirty_local));

        // The asymmetric case: a clean local copy must not launder a dirty
        // origin one when both are selected.
        let dirty_remote = preview(DeletePrompt::Choice, Some(0), Some(2));
        assert!(!needs_force_confirm(DeleteScope::Local, &dirty_remote));
        assert!(needs_force_confirm(DeleteScope::Remote, &dirty_remote));
        assert!(needs_force_confirm(DeleteScope::Both, &dirty_remote));

        // An unknown count warns rather than staying quiet: not knowing what a
        // delete costs is no reason to skip the confirmation.
        let unknown = preview(DeletePrompt::Choice, None, Some(0));
        assert!(needs_force_confirm(DeleteScope::Local, &unknown));
        assert!(needs_force_confirm(DeleteScope::Both, &unknown));
    }

    #[test]
    fn unmerged_for_scope_reports_the_worst_selected_copy() {
        let p = preview(DeletePrompt::Choice, Some(1), Some(4));
        assert_eq!(unmerged_for_scope(DeleteScope::Local, &p), Some(1));
        assert_eq!(unmerged_for_scope(DeleteScope::Remote, &p), Some(4));
        assert_eq!(unmerged_for_scope(DeleteScope::Both, &p), Some(4));
        assert_eq!(
            unmerged_for_scope(
                DeleteScope::Both,
                &preview(DeletePrompt::Choice, None, None)
            ),
            None
        );
    }

    #[test]
    fn prune_scope_for_fetches_only_when_remote_is_in_scope() {
        let local = prune_scope_for(DeleteScope::Local, false);
        assert!(local.local && !local.remote);
        assert!(
            !local.fetch,
            "a local-only delete must not touch the network"
        );

        let remote = prune_scope_for(DeleteScope::Remote, false);
        assert!(!remote.local && remote.remote);
        assert!(remote.fetch, "origin containment needs fresh refs");

        let both = prune_scope_for(DeleteScope::Both, true);
        assert!(both.local && both.remote && both.fetch);
        assert!(both.force, "force passes through to the plan");
        assert!(!prune_scope_for(DeleteScope::Both, false).force);
    }

    // ── rendering ───────────────────────────────────────────────────────

    /// Draw `f` into an 80x24 test terminal and return its text, one row per
    /// line with trailing spaces trimmed. Lets the modals and the footer be
    /// asserted on without a real terminal.
    fn draw(render: impl FnOnce(&mut Frame, Rect)) -> String {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("test terminal");
        terminal
            .draw(|f| {
                let area = f.area();
                render(f, area)
            })
            .expect("draw");
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A minimal config for the render tests — only the branch names are read.
    fn test_config() -> Config {
        Config {
            config_version: 1,
            repo_root: std::path::PathBuf::from("/nonexistent"),
            rolling_branch: "rolling".to_string(),
            stable_branch: "main".to_string(),
            roll_prefix: "roll/".to_string(),
            mode: crate::core::config::Mode::default(),
            username: "test".to_string(),
            hosts: Vec::new(),
            host_active: Default::default(),
            version_gate: true,
            tag_on_promote: true,
            push_tag: true,
            roll_to_rolling_gates: Vec::new(),
            rolling_to_main_gates: Vec::new(),
            host_gates: Vec::new(),
            clean_protect: Vec::new(),
            pull_mode: Default::default(),
            lazygit_command: "lazygit".to_string(),
        }
    }

    #[test]
    fn delete_modal_offers_four_choices_for_a_both_location_roll() {
        let p = preview(DeletePrompt::Choice, Some(0), Some(0));
        let out = draw(|f, area| render_delete_modal(f, area, &test_config(), &p));
        assert!(out.contains("delete"), "{out}");
        assert!(out.contains("exists locally and on origin"), "{out}");
        for hint in [
            "[l] local only",
            "[r] origin only",
            "[b] both",
            "[n] neither",
        ] {
            assert!(out.contains(hint), "missing {hint}:\n{out}");
        }
        assert!(
            !out.contains("[y]"),
            "the choice modal must not offer a bare yes:\n{out}"
        );
    }

    #[test]
    fn delete_modal_single_copy_offers_y_n() {
        let p = preview(DeletePrompt::Single(DeleteScope::Local), Some(0), None);
        let out = draw(|f, area| render_delete_modal(f, area, &test_config(), &p));
        assert!(out.contains("Delete local branch"), "{out}");
        assert!(out.contains("[y] delete"), "{out}");
        assert!(out.contains("[n] cancel"), "{out}");
    }

    #[test]
    fn delete_modal_says_why_a_checked_out_roll_is_origin_only() {
        let mut p = preview(DeletePrompt::Single(DeleteScope::Remote), Some(0), Some(0));
        p.local_is_checked_out = true;
        let out = draw(|f, area| render_delete_modal(f, area, &test_config(), &p));
        assert!(out.contains("Delete origin/"), "{out}");
        assert!(out.contains("checked out"), "should explain itself:\n{out}");
    }

    #[test]
    fn delete_modal_force_stage_states_the_cost() {
        let p = preview(
            DeletePrompt::ForceConfirm(DeleteScope::Both),
            Some(1),
            Some(4),
        );
        let out = draw(|f, area| render_delete_modal(f, area, &test_config(), &p));
        assert!(out.contains("force delete"), "{out}");
        // The worst selected copy, not the first one.
        assert!(out.contains("4 commits not in main"), "{out}");
        assert!(out.contains("cannot be undone"), "{out}");
        assert!(out.contains("[y] delete anyway"), "{out}");

        let one = preview(
            DeletePrompt::ForceConfirm(DeleteScope::Local),
            Some(1),
            None,
        );
        let out = draw(|f, area| render_delete_modal(f, area, &test_config(), &one));
        assert!(out.contains("1 commit not in main"), "singular:\n{out}");
    }

    #[test]
    fn the_status_bar_carries_only_the_basics_and_points_at_the_rest() {
        // The bar this replaced spelled out twenty bindings across four lines;
        // its whole failure mode was silently truncating the last one at 80
        // columns. One line now, and the way to everything else has to be on it.
        let app = StatusApp::new(test_config(), "main".to_string(), Vec::new(), false);
        let out = draw(|f, area| app.render_status_bar(f, area));

        assert!(out.contains("[?] keys"), "no way to reach the rest:\n{out}");
        assert!(basic_hints().chars().count() < 80, "{}", basic_hints());
        // A hint per basic binding and not one more: the bar is built from the
        // keymap, so an over-eager `hint` on a new row shows up here.
        assert_eq!(BINDINGS.iter().filter(|b| b.hint.is_some()).count(), 5);
        for absent in ["[x] prune", "[G]raduate", "[gg] lazygit"] {
            assert!(!out.contains(absent), "{absent} still on the bar:\n{out}");
        }
    }

    #[test]
    fn the_status_bar_reserves_a_row_for_every_hint_line_it_writes() {
        // The layout hands `render_status_bar` a fixed height; one hint line
        // more than that and the last binding silently disappears.
        let app = StatusApp::new(test_config(), "main".to_string(), Vec::new(), false);
        let out = draw(|f, area| {
            let chunks = Layout::vertical([Constraint::Length(2)]).split(area);
            app.render_status_bar(f, chunks[0])
        });
        assert!(out.contains("[q] quit"), "hint line clipped:\n{out}");
    }

    #[test]
    fn every_binding_is_declared_once_and_can_be_run() {
        // The table is the only place keys are declared, so this is where a
        // doubled-up or unlabelled binding has to be caught.
        let mut seen = Vec::new();
        for binding in BINDINGS {
            assert!(!binding.keys.is_empty(), "a binding with no keys");
            assert!(!binding.label.is_empty(), "{} has no label", binding.keys);
            assert!(
                !seen.contains(&binding.keys),
                "{} declared twice",
                binding.keys
            );
            seen.push(binding.keys);
        }
        // `?` is the one entry with nothing to replay — enter on it would only
        // reopen the list it is in.
        let unrunnable: Vec<_> = BINDINGS
            .iter()
            .filter(|b| b.replay.is_empty())
            .map(|b| b.keys)
            .collect();
        assert_eq!(unrunnable, vec!["?"]);
    }

    #[test]
    fn the_table_shows_the_chevron_and_the_sync_glyph_on_the_right_rows() {
        let cfg = config("main", "rolling");
        let mut rolls = vec![roll_n(1, RollState::Active), roll_n(2, RollState::Active)];
        rolls[0].branch = "roll/1-0101-ahead".to_string();
        rolls[0].is_current = true;
        rolls[1].branch = "roll/2-0102-diverged".to_string();
        rolls[1].is_current = false;

        let mut tracking = HashMap::new();
        tracking.insert(
            "roll/1-0101-ahead".to_string(),
            git::LocalBranch {
                name: "roll/1-0101-ahead".to_string(),
                upstream: "origin/roll/1-0101-ahead".to_string(),
                remote_name: "origin".to_string(),
                track: "ahead 2".to_string(),
                worktree: String::new(),
            },
        );
        tracking.insert(
            "roll/2-0102-diverged".to_string(),
            git::LocalBranch {
                name: "roll/2-0102-diverged".to_string(),
                upstream: "origin/roll/2-0102-diverged".to_string(),
                remote_name: "origin".to_string(),
                track: "ahead 1, behind 3".to_string(),
                worktree: String::new(),
            },
        );

        let mut app = StatusApp {
            config: cfg,
            current_branch: "roll/1-0101-ahead".to_string(),
            bases: Vec::new(),
            rolls,
            show_deps: false,
            tracking,
            version: None,
            table: TableState::default(),
            mode: Mode::Browsing,
            message: None,
            job: None,
            panel: None,
            pending_g: false,
        };

        let out = draw(|f, area| app.render_table(f, area));
        let ahead = out
            .lines()
            .find(|l| l.contains("roll/1-0101-ahead"))
            .expect("roll 1 row");
        let diverged = out
            .lines()
            .find(|l| l.contains("roll/2-0102-diverged"))
            .expect("roll 2 row");

        assert!(ahead.contains('›'), "chevron missing on HEAD: {ahead}");
        assert!(ahead.contains("↑2"), "{ahead}");
        // The chevron marks HEAD only — a second row must not claim it.
        assert!(
            !diverged.contains('›'),
            "chevron on a non-HEAD row: {diverged}"
        );
        assert!(diverged.contains("↑1↓3"), "{diverged}");
        assert!(out.contains("sync"), "the sync header is missing:\n{out}");
    }

    #[test]
    fn a_branch_with_no_tracking_data_renders_a_dash_in_the_table() {
        // `load_tracking` degrades to an empty map when git fails, and the table
        // still has to draw. Every row shows a dash rather than a false ✓.
        let cfg = config("main", "rolling");
        let mut app = StatusApp {
            config: cfg,
            current_branch: "main".to_string(),
            bases: Vec::new(),
            rolls: vec![roll_n(1, RollState::Active)],
            show_deps: false,
            tracking: HashMap::new(),
            version: None,
            table: TableState::default(),
            mode: Mode::Browsing,
            message: None,
            job: None,
            panel: None,
            pending_g: false,
        };
        let out = draw(|f, area| app.render_table(f, area));
        let row = out
            .lines()
            .find(|l| l.contains("roll/1-0101-x"))
            .expect("roll row");
        assert!(row.contains('—'), "{row}");
        assert!(!row.contains('✓'), "{row}");
    }

    #[test]
    fn the_sync_column_maps_each_tracking_state_to_a_glyph() {
        assert_eq!(sync_cell(Some(TrackState::InSync)).0, "✓");
        assert_eq!(sync_cell(Some(TrackState::Ahead(2))).0, "↑2");
        assert_eq!(sync_cell(Some(TrackState::Behind(3))).0, "↓3");
        assert_eq!(
            sync_cell(Some(TrackState::Diverged {
                ahead: 2,
                behind: 1
            }))
            .0,
            "↑2↓1"
        );
        assert_eq!(sync_cell(Some(TrackState::Gone)).0, "gone");
    }

    #[test]
    fn a_branch_with_nothing_to_compare_shows_a_dash_not_a_tick() {
        // A remote-only roll has no local copy, and a local branch may have no
        // upstream at all. Neither is "in sync", and rendering ✓ would say the
        // opposite of the truth.
        assert_eq!(sync_cell(None).0, "—");
        assert_eq!(sync_cell(Some(TrackState::NoUpstream)).0, "—");
    }

    #[test]
    fn the_sync_column_is_green_in_sync_yellow_diverged_and_red_when_gone() {
        assert_eq!(sync_cell(Some(TrackState::InSync)).1, Color::Green);
        assert_eq!(sync_cell(Some(TrackState::Ahead(1))).1, Color::Yellow);
        assert_eq!(sync_cell(Some(TrackState::Behind(1))).1, Color::Yellow);
        assert_eq!(sync_cell(Some(TrackState::Gone)).1, Color::Red);
        assert_eq!(sync_cell(None).1, Color::DarkGray);
    }

    #[test]
    fn the_chevron_marks_only_the_checked_out_branch() {
        assert_eq!(current_marker(true), "›");
        assert_eq!(current_marker(false), "");
    }

    fn v(major: u64, minor: u64, patch: u64) -> Semver {
        Semver {
            major,
            minor,
            patch,
        }
    }

    #[test]
    fn the_bump_previews_cover_all_three_levels_in_ascending_order() {
        assert_eq!(
            bump_previews(v(0, 2, 0)),
            [
                (BumpLevel::Patch, v(0, 2, 1)),
                (BumpLevel::Minor, v(0, 3, 0)),
                (BumpLevel::Major, v(1, 0, 0)),
            ]
        );
    }

    #[test]
    fn minor_and_major_bumps_reset_the_fields_below_them() {
        let [(_, patch), (_, minor), (_, major)] = bump_previews(v(1, 4, 7));
        assert_eq!(patch, v(1, 4, 8));
        assert_eq!(minor, v(1, 5, 0), "minor must zero the patch");
        assert_eq!(major, v(2, 0, 0), "major must zero minor and patch");
    }

    #[test]
    fn the_bump_keys_are_digits_in_the_order_shown() {
        // The modal numbers its rows from `bump_previews`, so key N must select
        // preview N — a mismatch here would bump the wrong field silently.
        let previews = bump_previews(v(0, 2, 0));
        for (i, (level, _)) in previews.into_iter().enumerate() {
            let key = KeyCode::Char(char::from_digit(i as u32 + 1, 10).unwrap());
            assert_eq!(bump_key(key), BumpOutcome::Level(level), "row {}", i + 1);
        }
    }

    #[test]
    fn the_bump_modal_cancels_and_ignores_everything_else() {
        for key in [KeyCode::Char('n'), KeyCode::Char('N'), KeyCode::Esc] {
            assert_eq!(bump_key(key), BumpOutcome::Cancel, "{key:?}");
        }
        // No Enter and no initials: `m`/`M` for minor/major would differ only by
        // the shift key, for choices an order of magnitude apart.
        for key in [
            KeyCode::Enter,
            KeyCode::Char(' '),
            KeyCode::Char('m'),
            KeyCode::Char('M'),
            KeyCode::Char('p'),
            KeyCode::Char('0'),
            KeyCode::Char('4'),
        ] {
            assert_eq!(bump_key(key), BumpOutcome::Ignore, "{key:?}");
        }
    }

    #[test]
    fn bump_is_refused_in_a_repo_with_no_version() {
        let err = bump_gate(None).expect_err("a repo with no Cargo.toml has nothing to bump");
        assert!(err.contains("Cargo.toml"), "{err}");
        assert_eq!(bump_gate(Some(v(0, 1, 0))), Ok(v(0, 1, 0)));
    }

    #[test]
    fn the_bump_modal_shows_every_resulting_version() {
        // "minor" is abstract; `0.2.0 → 0.3.0` is not. All three have to be
        // visible at once for the choice to be obvious.
        let out = draw(|f, area| render_bump_modal(f, area, v(0, 2, 0), "roll/3-0918-x"));
        assert!(out.contains("bump version"), "{out}");
        assert!(out.contains("Current: 0.2.0"), "{out}");
        assert!(
            out.contains("roll/3-0918-x"),
            "names the branch it lands on: {out}"
        );
        assert!(out.contains("[1] patch → 0.2.1"), "{out}");
        assert!(out.contains("[2] minor → 0.3.0"), "{out}");
        assert!(out.contains("[3] major → 1.0.0"), "{out}");
        assert!(out.contains("[n] cancel"), "{out}");
    }

    #[test]
    fn the_bump_modal_stays_readable_for_wide_versions() {
        let out = draw(|f, area| render_bump_modal(f, area, v(12, 345, 6789), "roll/1-x"));
        assert!(out.contains("[3] major → 13.0.0"), "{out}");
        assert!(out.contains("[1] patch → 12.345.6790"), "{out}");
    }

    #[test]
    fn the_header_shows_the_version_only_when_the_repo_has_one() {
        let mut app = StatusApp::new(test_config(), "main".to_string(), Vec::new(), false);
        app.version = Some(v(1, 2, 3));
        let with = draw(|f, area| app.render_header(f, area));
        assert!(with.contains("v1.2.3"), "{with}");

        app.version = None;
        let without = draw(|f, area| app.render_header(f, area));
        assert!(!without.contains("v1.2.3"), "{without}");
        // The rest of the header is unaffected either way.
        assert!(without.contains("Branch: main"), "{without}");
    }

    #[test]
    fn the_version_sits_in_the_top_right_corner_clear_of_the_branch_name() {
        // The branch line grows with the branch name, so the version has to be
        // somewhere that length cannot push it out of. The corner is measured
        // here rather than assumed: the first rendered row, past the midpoint.
        let mut app = StatusApp::new(
            test_config(),
            "roll/12-0918-a-deliberately-long-slug".to_string(),
            Vec::new(),
            false,
        );
        app.version = Some(v(1, 2, 3));
        let out = draw(|f, area| app.render_header(f, area));

        let top = out.lines().next().expect("a top border row");
        let at = top
            .find("v1.2.3")
            .unwrap_or_else(|| panic!("no version:\n{out}"));
        assert!(at > top.chars().count() / 2, "not right-aligned:\n{out}");
        // And the branch it shares the header with is still intact below it.
        assert!(
            out.contains("roll/12-0918-a-deliberately-long-slug"),
            "{out}"
        );
    }

    #[test]
    fn only_an_explicit_y_forces_a_push() {
        assert_eq!(force_push_key(KeyCode::Char('y')), ForcePushOutcome::Force);
        assert_eq!(force_push_key(KeyCode::Char('Y')), ForcePushOutcome::Force);
        for key in [KeyCode::Char('n'), KeyCode::Char('N'), KeyCode::Esc] {
            assert_eq!(force_push_key(key), ForcePushOutcome::Cancel, "{key:?}");
        }
        // Enter is deliberately unbound: this modal can open the instant `[P]`
        // lands on a branch that is behind, so a stray Enter must not overwrite a
        // remote.
        for key in [
            KeyCode::Enter,
            KeyCode::Char(' '),
            KeyCode::Char('P'),
            KeyCode::Char('f'),
        ] {
            assert_eq!(force_push_key(key), ForcePushOutcome::Ignore, "{key:?}");
        }
    }

    #[test]
    fn the_force_push_modal_states_the_divergence_and_the_lease() {
        let details = git::LocalBranch {
            name: "roll/1-0101-x".to_string(),
            upstream: "origin/roll/1-0101-x".to_string(),
            remote_name: "origin".to_string(),
            track: "ahead 1, behind 2".to_string(),
            worktree: String::new(),
        };
        let out = draw(|f, area| {
            render_force_push_modal(f, area, "roll/1-0101-x", "origin", Some(&details))
        });
        assert!(out.contains("force push"), "{out}");
        assert!(out.contains("2 commits you don't"), "{out}");
        assert!(out.contains("--force-with-lease"), "{out}");
        assert!(out.contains("[y] force"), "{out}");
    }

    #[test]
    fn the_force_push_modal_says_something_sensible_without_counts() {
        // Reached when git refused a push we could not predict from the tracking
        // ref; inventing a number there would be worse than omitting one.
        let out = draw(|f, area| render_force_push_modal(f, area, "roll/1", "origin", None));
        assert!(out.contains("has commits you don't"), "{out}");
        assert!(!out.contains("0 commits"), "{out}");
    }

    #[test]
    fn a_single_commit_reads_as_singular_in_the_force_prompt() {
        let details = git::LocalBranch {
            name: "roll/1".to_string(),
            upstream: "origin/roll/1".to_string(),
            remote_name: "origin".to_string(),
            track: "behind 1".to_string(),
            worktree: String::new(),
        };
        let out =
            draw(|f, area| render_force_push_modal(f, area, "roll/1", "origin", Some(&details)));
        assert!(out.contains("1 commit you don't"), "{out}");
        assert!(!out.contains("1 commits"), "{out}");
    }

    #[test]
    fn integrate_merges_the_hovered_roll_into_the_checked_out_one() {
        let mut other = roll_n(1, RollState::Active);
        other.branch = "roll/1-0101-alpha".to_string();
        other.location = BranchLocation::Both;
        assert_eq!(
            integrate_target_for("roll/2-0102-beta", "roll/", Some(&other)),
            Ok("roll/1-0101-alpha".to_string())
        );
    }

    #[test]
    fn integrate_needs_a_roll_branch_checked_out() {
        // `ops::integrate` refuses this too; catching it here means the message
        // names the branch instead of surfacing a core error after the modal.
        let other = roll_n(1, RollState::Active);
        for head in ["main", "rolling", "feature/x"] {
            let err = integrate_target_for(head, "roll/", Some(&other))
                .expect_err("{head} should be refused");
            assert!(err.contains("not a roll branch"), "{head}: {err}");
        }
    }

    #[test]
    fn integrate_refuses_a_roll_into_itself() {
        let mut same = roll_n(1, RollState::Active);
        same.branch = "roll/1-0101-alpha".to_string();
        let err = integrate_target_for("roll/1-0101-alpha", "roll/", Some(&same))
            .expect_err("self-merge should be refused");
        assert!(err.contains("already the current branch"), "{err}");
    }

    #[test]
    fn integrate_refuses_a_remote_only_roll_with_advice() {
        // `ops::integrate` resolves the source with a bare `ref_exists`, so
        // without this the user would get "branch not found" for a roll plainly
        // listed on screen.
        let mut remote = roll_n(1, RollState::Active);
        remote.branch = "roll/1-0101-alpha".to_string();
        remote.location = BranchLocation::Remote;
        let err = integrate_target_for("roll/2-0102-beta", "roll/", Some(&remote))
            .expect_err("a remote-only roll should be refused");
        assert!(err.contains("only on origin"), "{err}");
    }

    #[test]
    fn integrate_needs_something_selected() {
        let err = integrate_target_for("roll/2-0102-beta", "roll/", None)
            .expect_err("nothing selected should be refused");
        assert!(err.contains("no roll selected"), "{err}");
    }

    #[test]
    fn integrate_accepts_an_already_graduated_roll() {
        // Merging a graduated roll into yours is legitimate — you want its code —
        // and the dependency it records is simply already satisfied.
        let mut graduated = roll_n(1, RollState::Graduated);
        graduated.branch = "roll/1-0101-alpha".to_string();
        graduated.location = BranchLocation::Local;
        assert!(integrate_target_for("roll/2-0102-beta", "roll/", Some(&graduated)).is_ok());
    }

    #[test]
    fn integrate_honours_a_non_default_roll_prefix() {
        let mut other = roll_n(1, RollState::Active);
        other.branch = "batch/1-0101-alpha".to_string();
        other.location = BranchLocation::Local;
        assert!(integrate_target_for("batch/2-0102-beta", "batch/", Some(&other)).is_ok());
        assert!(integrate_target_for("batch/2-0102-beta", "roll/", Some(&other)).is_err());
    }

    #[test]
    fn validate_action_gates_integrate_the_same_way() {
        // The single validation entry point must agree with the helper, or the
        // key and the modal could disagree about what is allowed.
        let mut rolls = vec![roll_n(1, RollState::Active)];
        rolls[0].branch = "roll/1-0101-alpha".to_string();
        rolls[0].location = BranchLocation::Both;
        assert!(validate_action(
            Action::Integrate,
            Some(&rolls[0]),
            &rolls,
            "roll/2-0102-beta",
            "roll/"
        )
        .is_ok());
        assert!(
            validate_action(Action::Integrate, Some(&rolls[0]), &rolls, "main", "roll/").is_err()
        );
    }

    #[test]
    fn the_integrate_modal_names_both_the_source_and_the_destination() {
        // The direction is the whole point and the easiest thing to get backwards,
        // so the prompt has to spell it out.
        let cfg = config("main", "rolling");
        let out = draw(|f, area| {
            render_modal(
                f,
                area,
                &cfg,
                Action::Integrate,
                Some("roll/1-0101-alpha"),
                &[],
                "roll/2-0102-beta",
            )
        });
        assert!(
            out.contains("Integrate roll/1-0101-alpha into roll/2-0102-beta?"),
            "{out}"
        );
    }

    #[test]
    fn the_tidy_modal_says_local_only_where_prune_says_local_plus_origin() {
        // `[x]` and `[t]` are adjacent keys whose only difference is whether
        // origin is touched, so each prompt has to say which it is.
        let cfg = config("main", "rolling");
        let rolls = vec![
            roll_n(1, RollState::Graduated),
            roll_n(2, RollState::Promoted),
        ];

        let tidy = draw(|f, area| render_modal(f, area, &cfg, Action::Tidy, None, &rolls, "main"));
        assert!(
            tidy.contains("Delete 2 local graduated/promoted roll branches (local only)?"),
            "{tidy}"
        );

        let prune =
            draw(|f, area| render_modal(f, area, &cfg, Action::Prune, None, &rolls, "main"));
        assert!(
            prune.contains("Delete 1 promoted roll branch (local + origin)?"),
            "{prune}"
        );
    }

    #[test]
    fn a_job_title_names_the_roll_it_targets() {
        assert_eq!(
            Action::Graduate.job_title(Some("roll/1-0101-x")),
            "rf graduate roll/1-0101-x"
        );
        assert_eq!(Action::Promote.job_title(None), "rf promote");
    }

    #[test]
    fn a_sync_failure_carries_gits_text_and_the_hint_together() {
        let failure = sync::SyncFailure::from(git::GitFailure {
            stderr: " ! [rejected] main -> main (stale info)".to_string(),
            message: "`git push` exited with 1".to_string(),
        });
        let rendered = sync_error(failure).to_string();
        assert!(rendered.contains("stale info"), "{rendered}");
        assert!(rendered.contains("press f to fetch"), "{rendered}");
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
            tracking: HashMap::new(),
            version: None,
            table,
            mode: Mode::Browsing,
            message: None,
            job: None,
            panel: None,
            pending_g: false,
        };

        // Tall enough for the header, three table rows and the four-line status
        // bar, and wide enough for the sync column.
        let mut term = Terminal::new(TestBackend::new(80, 14)).unwrap();
        term.draw(|f| app.render(f)).unwrap();
        let line = |row: u16| -> String {
            (0..80)
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
