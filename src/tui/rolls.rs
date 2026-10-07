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

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::{
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Cell, Clear, Paragraph, Row, Table, TableState},
    Frame,
};

use super::output::{self, Followup, JobDone, JobProgress};
use crate::core::{
    branches::{self, BranchLocation, HotfixInfo, HotfixState, RollInfo, RollState, VerifySet},
    config::Config,
    git::{self, TrackState},
    history, ops, proc,
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
    /// Merge stable into the selected roll, or every active local roll when
    /// none is selected.
    Update,
    /// Delete every promoted roll branch, locally and on origin.
    Prune,
    /// Delete the local copy of every graduated or promoted roll branch,
    /// leaving `origin` untouched.
    Tidy,
    /// Land the *checked-out* hotfix into stable, then reintegrate stable into
    /// rolling. Reads HEAD like `[v]` and `[b]` do: `ops::hotfix_land` merges
    /// from the branch that is checked out, so the cursor cannot pick another.
    LandHotfix,
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
            Action::LandHotfix => "hotfix --land",
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

/// What a keypress in the `[V]` picker resolves to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum VerifyManyOutcome {
    Ignore,
    Cancel,
    Set(VerifySet),
}

/// Decide a keystroke in the verify-many picker: digits pick a set in the
/// order [`VerifySet::ALL`] lists them, `n`/`esc` cancel. Digits for the same
/// reason the bump modal uses them — six choices share too many initials.
pub(crate) fn verify_many_key(code: KeyCode) -> VerifyManyOutcome {
    match code {
        KeyCode::Char(c @ '1'..='9') => {
            let index = (c as usize) - ('1' as usize);
            match VerifySet::ALL.get(index) {
                Some(set) => VerifyManyOutcome::Set(*set),
                None => VerifyManyOutcome::Ignore,
            }
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => VerifyManyOutcome::Cancel,
        _ => VerifyManyOutcome::Ignore,
    }
}

/// How many rolls each set would verify, in menu order. Computed once when the
/// modal opens so the counts it shows are the counts the pass will use.
pub(crate) fn verify_set_counts(rolls: &[RollInfo]) -> [(VerifySet, usize); 6] {
    VerifySet::ALL.map(|set| (set, set.select(rolls).len()))
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
        /// For `[m]` on a roll row: the rolls that graduated ahead of it and
        /// would land on stable with it. Read once when the modal opens — it
        /// costs git calls, and a draw must not — and empty for every other
        /// action. The modal states them because advancing stable to one roll's
        /// graduation commit is not what "promote this roll" sounds like.
        carried: Vec<String>,
    },
    /// Read-only drill-down for a single roll: its identity plus its dependency
    /// rows (issue #61). A snapshot of the selected roll is captured on open so
    /// the overlay stays stable regardless of later list reloads. For
    /// both-location rolls, `ahead_behind` holds the local-vs-`origin` divergence
    /// captured at open time (issue #99); `None` when not applicable/unknown.
    Detail {
        roll: RollInfo,
        ahead_behind: Option<(u32, u32)>,
        /// Which linked roll `[enter]` opens: an index into
        /// [`detail_targets`] — the chain rows, then the dependents.
        cursor: usize,
        /// The rolls drilled through to get here, innermost last, so
        /// `[backspace]` retraces the path rather than closing. Each carries
        /// the divergence it was opened with, so going back redraws exactly
        /// what was there.
        trail: Vec<(RollInfo, Option<(u32, u32)>)>,
    },
    /// Slug-input modal for creating a new roll (issue #79) or a hotfix. Holds
    /// the in-progress text buffer; on Enter it runs `ops::create` or
    /// `ops::hotfix_create` through the same job path as the other actions.
    CreateInput {
        slug: String,
        kind: CreateKind,
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
    /// `[V]`: pick which set of rolls to verify in one pass. Holds the counts
    /// read when the modal opened, so the numbers it shows and the set it
    /// runs agree.
    VerifyMany {
        counts: [(VerifySet, usize); 6],
    },
    /// A graduation or promotion conflicted and was unwound. Offers the ways
    /// forward; the diagnosis is in the panel underneath.
    Conflict {
        source: String,
        target: String,
        culprits: Vec<String>,
        can_integrate: bool,
    },
    /// `PP`: confirm pushing every branch that needs it.
    ///
    /// Its own variant rather than a `Confirm` action, for the same reason
    /// `Delete` is: the answer carries a list of resolved [`SyncTarget`]s that
    /// `Action` cannot hold, and a key that writes to several remote refs at
    /// once has to show which ones before it does.
    PushAll {
        plan: PushAllPlan,
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
    /// `:`: the lazygit-style command prompt.
    Command {
        /// What the user actually typed. Filters the history list, and
        /// nothing else — `Up`/`Down` never touch it, only character edits do
        /// (resetting `cursor` to 0, same as `Help`'s `query`). Keeping this
        /// apart from `input` is what lets Up/Down cycle through more than one
        /// suggestion: filtering against `input` instead would, after the
        /// first pick, filter history against a full command string rather
        /// than what was actually typed, collapsing the candidate list.
        query: String,
        /// What `enter` runs. Starts equal to `query` and is overwritten by
        /// `Up`/`Down` picking a suggestion (reverse-search-style); any
        /// further character edit resets it back to `query`.
        input: String,
        /// Indexes the fuzzy-filtered (by `query`) history list below the
        /// prompt.
        cursor: usize,
    },
}

/// What the slug-input modal creates. One modal for both, because the input
/// is identical — a slug — and only the op it feeds and the words on the box
/// differ.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CreateKind {
    Roll,
    Hotfix,
}

impl CreateKind {
    fn noun(self) -> &'static str {
        match self {
            CreateKind::Roll => "roll",
            CreateKind::Hotfix => "hotfix",
        }
    }
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
    /// A `hotfix/N-…` row, listed below the rolls.
    Hotfix(usize),
}

/// Map a flat table index onto the base-then-roll-then-hotfix row layout.
/// `None` when the index is past the last row.
pub(crate) fn row_at(
    index: usize,
    base_count: usize,
    roll_count: usize,
    hotfix_count: usize,
) -> Option<RowKind> {
    if index < base_count {
        Some(RowKind::Base(index))
    } else if index - base_count < roll_count {
        Some(RowKind::Roll(index - base_count))
    } else if index - base_count - roll_count < hotfix_count {
        Some(RowKind::Hotfix(index - base_count - roll_count))
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
pub(crate) fn initial_selection(
    bases: &[BaseBranch],
    rolls: &[RollInfo],
    hotfixes: &[HotfixInfo],
) -> Option<usize> {
    if bases.is_empty() && rolls.is_empty() && hotfixes.is_empty() {
        return None;
    }
    if let Some(i) = bases.iter().position(|b| b.is_current) {
        return Some(i);
    }
    if let Some(i) = rolls.iter().position(|r| r.is_current) {
        return Some(bases.len() + i);
    }
    if let Some(i) = hotfixes.iter().position(|h| h.is_current) {
        return Some(bases.len() + rolls.len() + i);
    }
    Some(0)
}

/// One dependency row rendered in the [`Mode::Detail`] view.
///
/// `is_blocker` and `needs_reintegration` answer different questions and are
/// **not** mutually exclusive. `is_blocker` mirrors [`RollState::Blocked`]'s
/// own rule: only an `Active`/`Blocked` dep actually gates graduation (the
/// ordering constraint). `needs_reintegration` comes from
/// [`RollInfo::stale_deps`] — a direct ancestry check against the dep's
/// *current* tip — so it fires whenever the dep has moved since it was
/// integrated, whatever its state: a still-`Active` dep that keeps gaining
/// commits is just as stale as a `Diverged` one that graduated and then moved.
/// A roll can therefore be both blocked on a dependency *and* behind its
/// latest commits at the same time.
///
/// `carried` marks the one exception to `is_blocker`: a fellow member of a
/// dependency cycle that the roll on the other end carries (see
/// [`branches::DepCycle`]). The carrier contains its tip, so graduating the
/// carrier lands it — it is ungraduated, but it gates nothing. Always false
/// when `is_blocker` is true.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DepRow {
    pub number: u32,
    pub branch: String,
    pub state: RollState,
    pub is_blocker: bool,
    pub needs_reintegration: bool,
    pub carried: bool,
}

/// True when `carrier` is the carrier of a dependency cycle `member` is also
/// in — the one relation under which an ungraduated dependency does not block.
fn carries(carrier: &RollInfo, member: u32) -> bool {
    carrier
        .cycle
        .as_ref()
        .is_some_and(|c| c.carrier() == Some(carrier.number) && c.contains(member))
}

/// Owns everything needed to render and to *reload* after an action.
struct StatusApp {
    config: Config,
    current_branch: String,
    /// Pinned stable/rolling rows shown above the rolls; recomputed on reload
    /// so `is_current` tracks the branch actually checked out.
    bases: Vec<BaseBranch>,
    rolls: Vec<RollInfo>,
    /// `hotfix/*` rows shown below the rolls; recomputed on reload like the
    /// rolls are. Kept apart from `rolls` because they carry no dependencies
    /// and number independently — see `branches::HotfixInfo`.
    hotfixes: Vec<HotfixInfo>,
    show_deps: bool,
    /// Upstream tracking state per local branch, refreshed on reload. Sourced in
    /// one `for-each-ref` rather than an `ahead_behind` call per row.
    tracking: HashMap<String, git::LocalBranch>,
    /// The `[package]` version at each visible branch's tip, refreshed on
    /// reload. Sourced in one `cat-file --batch` rather than a `git show` per
    /// row, the same reason `tracking` is one `for-each-ref`.
    versions: HashMap<String, Semver>,
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
    /// Whether the panel fills most of the frame instead of its usual
    /// bottom-right corner. Not folded into `Mode`: like `panel` itself, this
    /// is orthogonal state that must not steal input routing from `Browsing`.
    panel_maximized: bool,
    /// The panel's rect as of the last render, for hit-testing a mouse click
    /// against. `None` whenever no panel is shown.
    panel_rect: Option<Rect>,
    /// True after a bare `g`, waiting to see whether the next key makes it `gg`.
    /// `g` has no action of its own, so this needs no timeout.
    pending_g: bool,
    /// When a bare `P` was pressed, waiting to see whether it becomes `PP`.
    /// Unlike `pending_g` this *is* timed — see [`PUSH_CHORD_WINDOW`].
    pending_push: Option<Instant>,
    /// Commands run through `:`, most-recent-first. Loaded once at startup
    /// from [`history::load`] and saved back after every run.
    command_history: Vec<String>,
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

/// Whether `(x, y)` falls inside `rect`.
fn point_in_rect(x: u16, y: u16, rect: Rect) -> bool {
    x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
}

/// Whether a left-click at `(x, y)` against the panel's last-drawn `rect`
/// should leave it maximized.
///
/// No `maximized` input needed: "click on it to maximize, click off of it to
/// restore" is symmetric in both directions — a click inside the small panel
/// should maximize it, a click inside the already-maximized one should leave
/// it maximized, and a click outside either should not. All three collapse to
/// the same rule: maximized afterward iff the click landed inside whatever
/// rect was last drawn.
pub(crate) fn click_maximizes(x: u16, y: u16, rect: Rect) -> bool {
    point_in_rect(x, y, rect)
}

/// A roll can graduate while it is active, blocked (its ungraduated
/// dependencies graduate first, as a chain), or needs re-graduation (diverged,
/// or reverted on rolling). Graduated / promoted rolls cannot.
pub(crate) fn can_graduate(state: &RollState) -> bool {
    matches!(
        state,
        RollState::Active | RollState::Diverged | RollState::Reverted | RollState::Blocked
    )
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

/// How long a lone `P` is held before it is taken to be a single-branch push.
///
/// `PP` cannot be resolved the way `gg` is — `g` does nothing on its own, so a
/// pending `g` can wait forever for the next key, while a lone `P` has to fire
/// by itself. So this is a real timeout: long enough that a deliberate double
/// tap lands inside it, short enough that the ordinary `[P]` does not feel
/// stuck. The push it starts takes seconds, so the delay is lost in the noise.
pub(crate) const PUSH_CHORD_WINDOW: Duration = Duration::from_millis(400);

/// What the key following a bare `P` makes of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PushChord {
    /// A second `P`: push every branch that needs it.
    All,
    /// Anything else: the single-branch push the first `P` already meant.
    Single,
}

/// Decide a pending `P` from the key that followed it.
///
/// Only a second `P` completes the chord; every other key — including `Esc` —
/// falls back to the single push rather than cancelling. The first `P` is taken
/// as the user's decision to push the selected branch, so nothing here can turn
/// it into a no-op; the chord only ever widens what gets pushed.
pub(crate) fn push_chord_key(code: KeyCode) -> PushChord {
    match code {
        KeyCode::Char('P') => PushChord::All,
        _ => PushChord::Single,
    }
}

/// One branch `PP` will push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PushItem {
    pub target: SyncTarget,
    /// True when the branch has no upstream yet, so this push *creates* the ref
    /// on the remote rather than advancing one. Called out separately in the
    /// modal because they are different acts.
    pub creates: bool,
}

/// A branch `PP` deliberately leaves for a `[P]` of its own, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PushSkip {
    pub branch: String,
    pub reason: String,
}

/// What `PP` would do: the branches it pushes, and the ones it declines to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PushAllPlan {
    pub items: Vec<PushItem>,
    pub skipped: Vec<PushSkip>,
}

/// Plan a `PP` over every row in the table, in the order they are listed.
///
/// The selection rule is the point of this function, and it is deliberately
/// narrow: **every push `PP` performs is one `[P]` would have performed without
/// a prompt.** A branch ahead of its upstream fast-forwards it; a branch with no
/// upstream is created and tracked, which cannot lose anything. Everything else
/// is listed as skipped with the key to press instead:
///
/// - behind or diverged — pushing needs a force, and forcing is a per-branch
///   decision behind its own y/N. A bulk key must never be the thing that
///   overwrites a remote ref, so these are never in `items` even though they are
///   exactly the branches whose sync column looks unfinished.
/// - `gone` — the upstream was deleted, most likely by `rf prune` or
///   `rf clean --with-remote`. Pushing would *resurrect* a branch someone
///   retired on purpose, so re-creating it stays an explicit `[P]` on the row.
///
/// A branch with no local copy, or one in sync, is not mentioned at all: there
/// is nothing to push and nothing to explain. So is one absent from `tracking`,
/// which is the same state the sync column renders as `—`.
pub(crate) fn plan_push_all(
    rows: &[(String, BranchLocation)],
    current_branch: &str,
    tracking: &HashMap<String, git::LocalBranch>,
) -> PushAllPlan {
    let mut plan = PushAllPlan::default();
    for (branch, location) in rows {
        if !matches!(location, BranchLocation::Local | BranchLocation::Both) {
            continue;
        }
        let target = SyncTarget::resolve(
            branch,
            current_branch,
            location.clone(),
            tracking.get(branch),
        );
        let mut skip = |reason: String| {
            plan.skipped.push(PushSkip {
                branch: branch.clone(),
                reason,
            })
        };
        match target.track {
            None | Some(TrackState::InSync) => {}
            Some(TrackState::Ahead(_)) => plan.items.push(PushItem {
                target,
                creates: false,
            }),
            Some(TrackState::NoUpstream) => plan.items.push(PushItem {
                target,
                creates: true,
            }),
            Some(TrackState::Behind(n)) => {
                skip(format!("behind by {n} — [p] to pull, then [P] to force"))
            }
            Some(TrackState::Diverged { ahead, behind }) => skip(format!(
                "diverged ↑{ahead}↓{behind} — [P] on the row to force"
            )),
            Some(TrackState::Gone) => {
                skip("upstream deleted — [P] on the row to re-create it".to_string())
            }
        }
    }
    plan
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

/// Landed hotfixes prune would consider: landing is a hotfix's promotion, so
/// `ops::prune_plan` winds them up alongside promoted rolls.
pub(crate) fn prunable_hotfix_count(hotfixes: &[HotfixInfo]) -> usize {
    hotfixes
        .iter()
        .filter(|h| h.state == HotfixState::Landed)
        .count()
}

/// Hotfixes `[t]` would consider. `ops::tidy_plan` maps a landed hotfix to
/// `Promoted` and an open one to `Active`, and [`TIDY_STATES`] — tidy's
/// default — selects only the former; as with rolls, only a local copy counts.
pub(crate) fn tidyable_hotfix_count(hotfixes: &[HotfixInfo]) -> usize {
    hotfixes
        .iter()
        .filter(|h| {
            h.state == HotfixState::Landed
                && matches!(h.location, BranchLocation::Local | BranchLocation::Both)
        })
        .count()
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
        keys: "PP",
        // "all" earns its place: it is the word someone types looking for this,
        // and the search is a subsequence match — a label without it returns no
        // hits for "push all".
        label: "push all branches that need it",
        group: "sync",
        hint: None,
        // Two keystrokes, replayed back to back. The second `P` lands inside
        // `PUSH_CHORD_WINDOW`, so the chord completes exactly as typing it
        // would — the same trick `gg` relies on, and the reason `replay` is a
        // slice rather than a single key.
        replay: &[KeyCode::Char('P'), KeyCode::Char('P')],
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
        keys: "V",
        label: "verify all rolls, or a set of them",
        group: "roll",
        hint: None,
        replay: &[KeyCode::Char('V')],
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
        label: "update the selected roll (or all) from stable",
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
        keys: "h",
        label: "create a hotfix off stable",
        group: "hotfix",
        hint: None,
        replay: &[KeyCode::Char('h')],
    },
    Binding {
        keys: "H",
        label: "land the checked-out hotfix into stable and rolling",
        group: "hotfix",
        hint: None,
        replay: &[KeyCode::Char('H')],
    },
    Binding {
        keys: "d",
        label: "delete the selected roll or hotfix branch",
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
        label: "restore, then close, the output panel",
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
    Binding {
        keys: "z",
        label: "maximize or restore the output panel",
        group: "output",
        hint: None,
        // Also reachable by clicking the panel (to maximize) or clicking
        // outside it while maximized (to restore) — not something `?` can
        // replay, so there is nothing to add here for that.
        replay: &[KeyCode::Char('z')],
    },
    Binding {
        keys: ":",
        label: "run a shell command",
        group: "output",
        // Not a `hint`: the status bar is capped at five curated basics (see
        // `the_status_bar_carries_only_the_basics_and_points_at_the_rest`),
        // and `?` already makes every other binding, this one included,
        // discoverable by name.
        hint: None,
        replay: &[KeyCode::Char(':')],
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

/// The commands in `history` matching `query`, best first, as indices into
/// `history`. Exactly [`filter_bindings`]'s shape against a different list —
/// one fuzzy matcher, reused rather than written twice.
pub(crate) fn filter_history(history: &[String], query: &str) -> Vec<usize> {
    let mut scored: Vec<(usize, i32)> = history
        .iter()
        .enumerate()
        .filter_map(|(i, cmd)| fuzzy_score(cmd, query).map(|score| (i, score)))
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

/// What a keypress in the `:` command prompt does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandOutcome {
    /// Input or cursor changed (or the key meant nothing); keep the prompt open.
    Continue,
    Close,
    /// Run the current input in the panel.
    Run,
    /// Run the current input in the real shell instead (`alt+enter`, or
    /// `shift+enter` where the terminal reports it distinguishably).
    RunInShell,
}

/// Apply one keystroke to the `:` prompt, mutating `input` and `cursor` in
/// place. `alt` carries whether the modifier was held on `Enter` — the pure
/// function takes it as a plain bool rather than a `KeyEvent` so the whole
/// interaction is testable without constructing one.
///
/// Up/Down are reverse-search-style, unlike the `?` list's cursor: there is no
/// separate "list" to look at here, so moving the highlight also copies that
/// entry straight into `input`, the way a shell's history search does. Typing
/// resets the highlight to the top match, same as `?`.
pub(crate) fn handle_command_key(
    query: &mut String,
    input: &mut String,
    cursor: &mut usize,
    history: &[String],
    code: KeyCode,
    alt: bool,
) -> CommandOutcome {
    match code {
        KeyCode::Esc => CommandOutcome::Close,
        KeyCode::Enter if alt => CommandOutcome::RunInShell,
        KeyCode::Enter => {
            if input.trim().is_empty() {
                CommandOutcome::Continue
            } else {
                CommandOutcome::Run
            }
        }
        KeyCode::Down => {
            let matches = filter_history(history, query);
            if !matches.is_empty() {
                // `input == query` means nothing has been picked yet (every
                // character edit resets them to match), so the first press
                // lands on the top suggestion rather than skipping straight to
                // the second.
                *cursor = if *input == *query {
                    0
                } else {
                    (*cursor + 1).min(matches.len() - 1)
                };
                *input = history[matches[*cursor]].clone();
            }
            CommandOutcome::Continue
        }
        KeyCode::Up => {
            let matches = filter_history(history, query);
            if !matches.is_empty() {
                *cursor = if *input == *query {
                    0
                } else {
                    cursor.saturating_sub(1)
                };
                *input = history[matches[*cursor]].clone();
            }
            CommandOutcome::Continue
        }
        KeyCode::Backspace => {
            query.pop();
            *input = query.clone();
            *cursor = 0;
            CommandOutcome::Continue
        }
        KeyCode::Char(c) if !c.is_control() => {
            query.push(c);
            *input = query.clone();
            *cursor = 0;
            CommandOutcome::Continue
        }
        _ => CommandOutcome::Continue,
    }
}

/// Build a `<shell> -c <input>` command. The user's own shell (`$SHELL`,
/// falling back to `sh` only when it's unset) rather than a hardcoded `sh`:
/// the whole point of "the shell that launched rf" is that it may not be a
/// POSIX shell at all — this project's own developer runs Nushell.
fn shell_command(shell: &str, input: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new(shell);
    cmd.arg("-c").arg(input);
    cmd
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
        VersionStatus::DevVersion => "DEV",
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
/// dependency's branch and state. A dep is flagged as a *blocker* when it is
/// not currently graduated on rolling (state is `Active`/`Blocked`, or
/// `Reverted` — graduated once, but that merge was since undone there) —
/// those are what actually hold the roll back, matching the same states
/// `promote_target_for` refuses to promote. `needs_reintegration` is a separate, ancestry-based
/// question answered by `selected.stale_deps`: has the dependency's branch
/// moved since `selected` integrated it, whatever its state — so a dep can be
/// both a blocker *and* stale at once (still active, and already moved again).
/// The one exception to "ungraduated blocks" is a fellow dependency-cycle
/// member `selected` carries: it is `carried`, not a blocker, because
/// graduating `selected` lands it. Unknown dep numbers (not present in `all`)
/// are skipped. The empty result means "no dependencies / not blocked".
pub(crate) fn dep_rows(selected: &RollInfo, all: &[RollInfo]) -> Vec<DepRow> {
    selected
        .deps
        .iter()
        .filter_map(|num| all.iter().find(|r| r.number == *num))
        .map(|dep| {
            let carried = carries(selected, dep.number);
            DepRow {
                number: dep.number,
                branch: dep.branch.clone(),
                state: dep.state.clone(),
                is_blocker: !carried
                    && matches!(
                        dep.state,
                        RollState::Active | RollState::Blocked | RollState::Reverted
                    ),
                needs_reintegration: selected.stale_deps.contains(&dep.number),
                carried,
            }
        })
        .collect()
}

/// One link in the dependency chain the detail view draws.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChainRow {
    /// 0 for a direct dependency, 1 for a dependency of that one, and so on.
    pub depth: usize,
    pub number: u32,
    pub branch: String,
    pub state: RollState,
    /// Not yet graduated/promoted — holds its parent back.
    pub is_blocker: bool,
    /// The parent integrated this roll, and its branch has moved since —
    /// [`DepRow::needs_reintegration`], read from the parent's `stale_deps`.
    pub needs_reintegration: bool,
    /// [`DepRow::carried`], read the same way: the roll's parent at this link
    /// is the cycle's carrier, so this link gates nothing despite being
    /// ungraduated. Always false when `is_blocker` is true.
    pub carried: bool,
    /// Already listed higher up the chain (a diamond). Its own dependencies
    /// are not repeated under it.
    pub repeated: bool,
}

/// The whole dependency chain below `selected`, depth-first, as the detail
/// view shows it: roll 12 depends on 9, which depends on 8, which depends on 7.
///
/// [`dep_rows`] is the first level of this. The traversal keeps a set of rolls
/// already listed, and a roll reached a second time — a diamond, or a cycle if
/// the history is strange enough — is listed once more as `repeated` and not
/// descended into, so the output is finite and every roll's own deps appear
/// exactly once. Both markers on a link are the *parent's* judgement of it —
/// `needs_reintegration` reads the parent's `RollInfo::stale_deps`, since that
/// is who integrated it. Unknown numbers are skipped, as in [`dep_rows`].
pub(crate) fn dep_chain(selected: &RollInfo, all: &[RollInfo]) -> Vec<ChainRow> {
    fn walk(
        parent: &RollInfo,
        all: &[RollInfo],
        depth: usize,
        seen: &mut HashSet<u32>,
        rows: &mut Vec<ChainRow>,
    ) {
        // Each level is exactly `dep_rows` of its parent, so there is one
        // definition of a direct dependency row and the chain only adds depth.
        for row in dep_rows(parent, all) {
            let repeated = !seen.insert(row.number);
            rows.push(ChainRow {
                depth,
                number: row.number,
                branch: row.branch,
                state: row.state,
                is_blocker: row.is_blocker,
                needs_reintegration: row.needs_reintegration,
                carried: row.carried,
                repeated,
            });
            if !repeated {
                if let Some(dep) = all.iter().find(|r| r.number == row.number) {
                    walk(dep, all, depth + 1, seen, rows);
                }
            }
        }
    }
    let mut seen = HashSet::from([selected.number]);
    let mut rows = Vec::new();
    walk(selected, all, 0, &mut seen, &mut rows);
    rows
}

/// The rolls `[enter]` can open from a detail pane, in the order the pane
/// lists them: every chain row (repeated ones included — opening a diamond's
/// second mention is as valid as its first), then the dependents. The cursor
/// in [`Mode::Detail`] is an index into this.
pub(crate) fn detail_targets(roll: &RollInfo, all: &[RollInfo]) -> Vec<u32> {
    dep_chain(roll, all)
        .iter()
        .map(|r| r.number)
        .chain(dependent_rows(roll, all).iter().map(|r| r.number))
        .collect()
}

/// What a keypress in a detail pane does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DetailOutcome {
    Continue,
    /// Leave the overlay entirely, however deep the trail.
    Close,
    /// Return to the pane this one was opened from.
    Back,
    /// Open the linked roll at this index of [`detail_targets`].
    Open(usize),
}

/// Apply one keystroke to a detail pane.
///
/// The pane is a place to *dig*, not just read: `j`/`k` walk the linked rolls,
/// `enter` (or `l`/`→`) opens the one under the cursor as its own pane, and
/// `backspace` (or `h`/`←`) comes back up one level — falling through to
/// closing when there is nowhere further up, so the key never dead-ends.
/// `esc`/`q` always close outright, whatever the depth. Pure, so the whole
/// navigation is testable without a terminal.
pub(crate) fn detail_key(
    code: KeyCode,
    cursor: &mut usize,
    targets: usize,
    has_trail: bool,
) -> DetailOutcome {
    match code {
        KeyCode::Esc | KeyCode::Char('q') => DetailOutcome::Close,
        KeyCode::Backspace | KeyCode::Char('h') | KeyCode::Left => {
            if has_trail {
                DetailOutcome::Back
            } else {
                DetailOutcome::Close
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            *cursor = (*cursor + 1).min(targets.saturating_sub(1));
            DetailOutcome::Continue
        }
        KeyCode::Up | KeyCode::Char('k') => {
            *cursor = cursor.saturating_sub(1);
            DetailOutcome::Continue
        }
        KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right if targets > 0 => {
            DetailOutcome::Open((*cursor).min(targets - 1))
        }
        _ => DetailOutcome::Continue,
    }
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
/// gated by the target" — true while `target` is `Active`/`Blocked`/`Reverted`,
/// mirroring [`dep_rows`]'s rule, since until it graduates (or re-graduates)
/// the dependent cannot advance past it. `needs_reintegration` is the mirror
/// image of `dep_rows`' version:
/// it reads each dependent's *own* `stale_deps` (not `target`'s state), since
/// whether a given dependent's copy of `target` is stale depends on when that
/// dependent last integrated it, not on what `target` is doing now. The detail
/// view does not render a per-row marker for dependents, so both flags are
/// purely informational here, but they keep the fields meaningful and
/// testable. A dependent that carries `target` (a dependency-cycle carrier) is
/// `carried` rather than gated, mirroring [`dep_rows`]. A roll is never its own
/// dependent, even if a self-referential entry somehow appears.
pub(crate) fn dependent_rows(target: &RollInfo, all: &[RollInfo]) -> Vec<DepRow> {
    let target_gates = matches!(
        target.state,
        RollState::Active | RollState::Blocked | RollState::Reverted
    );
    target
        .dependents
        .iter()
        .filter(|num| **num != target.number)
        .filter_map(|num| all.iter().find(|r| r.number == *num))
        .map(|dependent| {
            let carried = carries(dependent, target.number);
            DepRow {
                number: dependent.number,
                branch: dependent.branch.clone(),
                state: dependent.state.clone(),
                is_blocker: target_gates && !carried,
                needs_reintegration: dependent.stale_deps.contains(&target.number),
                carried,
            }
        })
        .collect()
}

/// The detail view's dependency-cycle note: [`branches::DepCycle::advice`] —
/// the same sentence the plain tables print and `--json` carries — broken at
/// its dash so it fits a popup rather than stretching it across the screen.
pub(crate) fn cycle_lines(cycle: &branches::DepCycle, all: &[RollInfo]) -> Vec<String> {
    let advice = cycle.advice(all);
    match advice.split_once(" — ") {
        Some((what, todo)) => vec![what.to_string(), format!("  {todo}")],
        None => vec![advice],
    }
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
        RollState::Reverted => Color::LightRed,
        RollState::Promoted => Color::DarkGray,
        RollState::Demoted => Color::Cyan,
        RollState::Blocked => Color::Magenta,
    }
}

/// Hotfix rows in their own colour, distinct from every roll state, so a
/// glance separates the two tiers even when their labels are both a tick.
fn hotfix_color(state: HotfixState) -> Color {
    match state {
        HotfixState::Open => Color::LightRed,
        HotfixState::Landed => Color::DarkGray,
    }
}

/// The two-lane glyph pair (`main_lane`, `rolling_lane`) for a roll's graph
/// column cell, read left-to-right in the same order the pinned base rows are
/// listed (stable, then rolling).
///
/// Deliberately two *fixed* lanes rather than a dynamic multi-lane layout: a
/// dependency between two rolls already has a home (the `deps`/`dependants`
/// columns and their `⚠` stale markers), and the `branch` column has no width
/// to spare for a packer that would need unbounded space in pathological
/// cases. This column re-renders facts `RollState` already carries — it adds
/// no new ones — as the lane position a glance reads as "how settled is
/// this": further right (rolling) is further from landing, `●` in the main
/// lane means it is on stable. See docs/internals/algorithms.md.
pub(crate) fn graph_glyphs(state: &RollState) -> (char, char) {
    match state {
        RollState::Active => ('│', '○'),
        RollState::Blocked => ('│', '◌'),
        RollState::Diverged => ('│', '◐'),
        RollState::Reverted => ('│', '↺'),
        RollState::Graduated => ('│', '●'),
        RollState::Promoted => ('●', '●'),
        RollState::Demoted => ('◐', '●'),
    }
}

/// Same lane pair for a hotfix row: a hotfix lands directly into both stable
/// and rolling in one step (`[H]`), so it has no `Graduated`-only state of its
/// own — it is either still open (not yet on either lane) or landed (on both).
pub(crate) fn hotfix_graph_glyphs(state: HotfixState) -> (char, char) {
    match state {
        HotfixState::Open => ('│', '○'),
        HotfixState::Landed => ('●', '●'),
    }
}

/// The graph column's own two rows: stable sits on the main lane with nothing
/// yet in rolling's; rolling sits on its own lane, with the main lane passing
/// through underneath it since a later promotion still has to reach stable.
pub(crate) fn base_graph_glyphs(role: BaseRole) -> (char, char) {
    match role {
        BaseRole::Stable => ('●', ' '),
        BaseRole::Rolling => ('│', '●'),
    }
}

/// Render a lane pair as the graph column's cell text.
fn graph_cell(glyphs: (char, char)) -> String {
    format!("{}{}", glyphs.0, glyphs.1)
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
        RollState::Reverted => Err(format!(
            "{} was reverted on rolling — graduate it again before promoting",
            sel.branch
        )),
        RollState::Demoted => Err(format!(
            "{} was promoted, but that promotion was reverted on the stable branch — \
             re-promotion isn't automated yet; revert the revert manually",
            sel.branch
        )),
        RollState::Active | RollState::Blocked => Err(format!(
            "{} is {} — only graduated rolls can be promoted",
            sel.branch,
            sel.state.label()
        )),
    }
}

/// What `[u]` should update, given the current selection.
///
/// `Ok(None)` means every active local roll — the long-standing behaviour,
/// used when nothing narrower is selected. `Ok(Some(branch))` means just that
/// one roll, so updating a roll in progress no longer has to touch every other
/// active roll at the same time.
///
/// Mirrors `promote_target_for`: a base-branch row or empty selection widens to
/// the repo-wide shape, but a roll row that cannot be updated yields the reason
/// rather than quietly widening past what the keystroke asked for.
pub(crate) fn update_target_for(
    selected: Option<&RollInfo>,
    rolls: &[RollInfo],
) -> Result<Option<String>, String> {
    let Some(sel) = selected else {
        return if can_update(rolls) {
            Ok(None)
        } else {
            Err("no active local rolls to update".to_string())
        };
    };

    match sel.state {
        RollState::Active | RollState::Blocked => {
            if matches!(sel.location, BranchLocation::Local | BranchLocation::Both) {
                Ok(Some(sel.branch.clone()))
            } else {
                Err(format!(
                    "'{}' exists only on origin — press [space] or [p] to get it locally first",
                    sel.branch
                ))
            }
        }
        RollState::Graduated
        | RollState::Diverged
        | RollState::Reverted
        | RollState::Promoted
        | RollState::Demoted => Err(format!(
            "{} is {} — only active rolls can be updated",
            sel.branch,
            sel.state.label()
        )),
    }
}

/// The branch `[I]` would merge into the current one, or the reason it cannot.
///
/// Mirrors `integrate_target_for`, but the source is always the rolling branch
/// rather than the row under the cursor — rolling isn't a roll, so it never has
/// a row to select. This is the recovery path for a roll that fails to merge
/// into rolling at graduation time: bring rolling into the roll here instead,
/// resolve whatever conflicts graduation would have hit, then graduate
/// normally. Because rolling already contains every graduated roll, the same
/// merge picks all of them up as dependencies (method 2 in
/// `core::dependencies`) in one step.
pub(crate) fn integrate_rolling_target_for(
    current_branch: &str,
    roll_prefix: &str,
    rolling_branch: &str,
) -> Result<String, String> {
    if !current_branch.starts_with(roll_prefix) {
        return Err(format!(
            "'{current_branch}' is not a roll branch — integrate merges into a roll"
        ));
    }
    if current_branch == rolling_branch {
        return Err(format!("'{rolling_branch}' is already the current branch"));
    }
    Ok(rolling_branch.to_string())
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

/// [`validate_action`], plus the hotfix rows: prune and tidy are repo-wide and
/// also reach landed hotfixes, so either is worth offering when only hotfixes
/// qualify. Every other action is about rolls and is decided by
/// `validate_action` alone.
pub(crate) fn validate_with_hotfixes(
    action: Action,
    selected: Option<&RollInfo>,
    rolls: &[RollInfo],
    hotfixes: &[HotfixInfo],
    current_branch: &str,
    roll_prefix: &str,
) -> Result<(), String> {
    let hotfixes_qualify = match action {
        Action::Prune => prunable_hotfix_count(hotfixes) > 0,
        Action::Tidy => tidyable_hotfix_count(hotfixes) > 0,
        _ => false,
    };
    if hotfixes_qualify {
        return Ok(());
    }
    validate_action(action, selected, rolls, current_branch, roll_prefix)
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
            if !can_graduate(&sel.state) {
                return Err(format!(
                    "{} is {} — only active, blocked, diverged, or reverted rolls can graduate",
                    sel.branch,
                    sel.state.label()
                ));
            }
            // A blocked roll graduates after its dependencies; the planner's
            // refusals (a cycle, a dependency with no local copy) are the
            // reasons worth showing, so they are surfaced here rather than
            // discovered mid-run.
            ops::dependency_chain(rolls, &sel.branch, ops::ChainKind::Graduate)
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        Action::Integrate => {
            integrate_target_for(current_branch, roll_prefix, selected).map(|_| ())
        }
        Action::Promote => promote_target_for(selected, rolls).map(|_| ()),
        Action::Update => update_target_for(selected, rolls).map(|_| ()),
        Action::Prune => {
            if can_prune(rolls) {
                Ok(())
            } else {
                Err("nothing to prune — no promoted roll branches or landed hotfixes".to_string())
            }
        }
        Action::Tidy => {
            if can_tidy(rolls) {
                Ok(())
            } else {
                Err("nothing to tidy — no local graduated or promoted roll branches".to_string())
            }
        }
        Action::LandHotfix => {
            if current_branch.starts_with(branches::HOTFIX_PREFIX) {
                Ok(())
            } else {
                Err(format!(
                    "'{current_branch}' is not a hotfix — [space] onto one to land it"
                ))
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
pub(crate) fn delete_prompt(
    branch: &str,
    location: &BranchLocation,
    is_current: bool,
) -> Result<DeletePrompt, String> {
    match (location, is_current) {
        (BranchLocation::Neither, _) => Err(format!("{branch} no longer exists")),
        (BranchLocation::Local, true) => Err(format!(
            "{branch} is checked out — switch away before deleting it"
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
        // Loaded here rather than passed in: the CLI's plain table loads its
        // own, and a load failure degrades to "no hotfix rows" instead of
        // refusing to open the view.
        let hotfixes = branches::list_hotfixes(&config).unwrap_or_default();
        let mut table = TableState::default();
        table.select(initial_selection(&bases, &rolls, &hotfixes));
        let tracking = load_tracking(&config);
        let versions = load_versions(&config, &bases, &rolls, &hotfixes);
        let version = version::read_version(&config.repo_root).unwrap_or(None);
        Self {
            config,
            current_branch,
            bases,
            rolls,
            hotfixes,
            show_deps,
            tracking,
            versions,
            version,
            table,
            mode: Mode::Browsing,
            message: None,
            job: None,
            panel: None,
            panel_maximized: false,
            panel_rect: None,
            pending_g: false,
            pending_push: None,
            command_history: history::load(),
        }
    }

    fn run_loop(&mut self, terminal: &mut super::Tui) -> Result<()> {
        loop {
            terminal.draw(|f| self.render(f))?;
            // Before reading input, so a job that finished during the poll is
            // reflected in this frame rather than the next one.
            self.poll_job()?;
            self.resolve_lapsed_push();

            if event::poll(Duration::from_millis(50))? {
                let event = event::read()?;
                if let Event::Mouse(mouse) = event {
                    // Clicks only drive the panel, and only while nothing else
                    // is already claiming input — a modal's own keys take
                    // precedence, same as every mutating key already does.
                    if matches!(self.mode, Mode::Browsing) {
                        self.handle_mouse(mouse);
                    }
                    continue;
                }
                if let Event::Key(key) = event {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    if matches!(self.mode, Mode::Confirm { .. }) {
                        self.handle_confirm(key.code);
                    } else if matches!(self.mode, Mode::Conflict { .. }) {
                        self.handle_conflict(key.code);
                    } else if matches!(self.mode, Mode::PushAll { .. }) {
                        self.handle_push_all(key.code);
                    } else if matches!(self.mode, Mode::ForcePush { .. }) {
                        self.handle_force_push(key.code);
                    } else if matches!(self.mode, Mode::Bump { .. }) {
                        self.handle_bump(key.code);
                    } else if matches!(self.mode, Mode::VerifyMany { .. }) {
                        self.handle_verify_many(key.code);
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
                    } else if matches!(self.mode, Mode::Command { .. }) {
                        // Takes the whole `KeyEvent`, not just the code: telling
                        // `alt+enter` apart from a plain `enter` needs the
                        // modifiers, which every other mode's dispatch discards.
                        self.handle_command(terminal, key)?;
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
            Some(Followup::OfferConflictResolution {
                source,
                target,
                culprits,
                can_integrate,
            }) => {
                self.mode = Mode::Conflict {
                    source,
                    target,
                    culprits,
                    can_integrate,
                };
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
        self.panel_maximized = false;
        self.job = Some(output::Job::spawn(body));
    }

    /// Toggle the panel between its usual corner and filling most of the
    /// frame. A no-op with no panel up.
    fn toggle_maximize(&mut self) {
        if self.panel.is_some() {
            self.panel_maximized = !self.panel_maximized;
        }
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

        // Maximized, the table sits hidden behind the panel: `j`/`k` and the
        // page keys scroll it instead of moving a selection nobody can see,
        // and everything else beyond `z`/`esc`/`q` is swallowed rather than
        // falling through to a table-nav binding that would act blind.
        if self.panel_maximized {
            match code {
                KeyCode::Char('z') => self.toggle_maximize(),
                // Restores first; a second `esc` (now un-maximized) closes
                // the panel via the ordinary arm below.
                KeyCode::Esc => self.panel_maximized = false,
                KeyCode::Char('j') | KeyCode::Down => self.scroll_panel(PanelScroll::Down),
                KeyCode::Char('k') | KeyCode::Up => self.scroll_panel(PanelScroll::Up),
                KeyCode::PageUp => self.scroll_panel(PanelScroll::Up),
                KeyCode::PageDown => self.scroll_panel(PanelScroll::Down),
                KeyCode::End => self.scroll_panel(PanelScroll::End),
                KeyCode::Char('q') => return Ok(true),
                _ => {}
            }
            return Ok(false);
        }

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

        // `PP` pushes every branch that needs it. The first key after a bare `P`
        // always resolves the chord and does nothing else: a second `P` makes it
        // the bulk push, anything else falls back to the single-branch `[P]` the
        // user has already committed to. That key is consumed rather than also
        // acted on, because resolving may open the force-push modal or start a
        // job — applying a browsing binding to a screen that just moved is worse
        // than a keystroke the user can simply repeat.
        if self.pending_push.take().is_some() {
            match push_chord_key(code) {
                PushChord::All => self.start_push_all(),
                PushChord::Single => self.start_push(),
            }
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
            KeyCode::Char('z') => self.toggle_maximize(),
            KeyCode::Char(':') => {
                if self.busy() {
                    return Ok(false);
                }
                self.mode = Mode::Command {
                    query: String::new(),
                    input: String::new(),
                    cursor: 0,
                };
            }
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
                    kind: CreateKind::Roll,
                }
            }
            KeyCode::Char('h') => {
                if self.busy() {
                    return Ok(false);
                }
                self.mode = Mode::CreateInput {
                    slug: String::new(),
                    kind: CreateKind::Hotfix,
                }
            }
            KeyCode::Char('H') => self.request_land_hotfix(),
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
                    Some(RowKind::Hotfix(i)) => {
                        let hotfix = self.hotfixes[i].clone();
                        self.execute_switch(hotfix.branch, hotfix.location);
                    }
                    None => self.message = Some("no branch selected".to_string()),
                }
            }
            KeyCode::Char('p') => self.start_pull(),
            KeyCode::Char('P') => self.arm_push(),
            KeyCode::Char('f') => self.start_fetch(),
            KeyCode::Char('v') => self.start_verify(),
            KeyCode::Char('V') => self.request_verify_many(),
            KeyCode::Char('G') => self.request(Action::Graduate),
            KeyCode::Char('i') => self.request_integrate(),
            KeyCode::Char('I') => self.request_integrate_rolling(),
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
                } else if let Some(RowKind::Hotfix(i)) = self.selected_row() {
                    // Nor do hotfixes: no dependencies, nothing to break out.
                    self.message = Some(format!(
                        "'{}' is a hotfix branch — {}",
                        self.hotfixes[i].branch,
                        self.hotfixes[i].state.label()
                    ));
                } else if let Some(roll) = self.selected_roll() {
                    let roll = roll.clone();
                    // Capture the branch's divergence from origin for the overlay
                    // (issue #99), only meaningful when it exists on both sides.
                    let ahead_behind = if matches!(roll.location, BranchLocation::Both) {
                        git::ahead_behind(&self.config.repo_root, &roll.branch).ok()
                    } else {
                        None
                    };
                    self.mode = Mode::Detail {
                        roll,
                        ahead_behind,
                        cursor: 0,
                        trail: Vec::new(),
                    };
                }
            }
            _ => {}
        }
        Ok(false)
    }

    /// Handle a keypress while the read-only detail overlay is open: walk,
    /// open and retrace linked rolls (see [`detail_key`]). Action keys are
    /// ignored, so nothing can fire from here.
    fn handle_detail(&mut self, code: KeyCode) {
        let Mode::Detail {
            roll,
            ahead_behind,
            cursor,
            trail,
        } = &mut self.mode
        else {
            return;
        };
        let targets = detail_targets(roll, &self.rolls);
        match detail_key(code, cursor, targets.len(), !trail.is_empty()) {
            DetailOutcome::Continue => {}
            DetailOutcome::Close => self.mode = Mode::Browsing,
            DetailOutcome::Back => {
                if let Some((previous, divergence)) = trail.pop() {
                    *roll = previous;
                    *ahead_behind = divergence;
                    *cursor = 0;
                }
            }
            DetailOutcome::Open(index) => {
                // Drill into the linked roll: the one under the cursor becomes
                // the pane, and the current one joins the trail so backspace
                // can return to it. Divergence is read the way `[enter]` from
                // the table reads it, at open time, for a both-location roll.
                let Some(next) = targets
                    .get(index)
                    .and_then(|n| self.rolls.iter().find(|r| r.number == *n))
                    .cloned()
                else {
                    return;
                };
                let divergence = if matches!(next.location, BranchLocation::Both) {
                    git::ahead_behind(&self.config.repo_root, &next.branch).ok()
                } else {
                    None
                };
                trail.push((std::mem::replace(roll, next), *ahead_behind));
                *ahead_behind = divergence;
                *cursor = 0;
            }
        }
    }

    /// Handle a keypress while the create-input modal is open: edit the buffer,
    /// cancel back to browsing, or submit. Submitting an empty buffer surfaces a
    /// message instead of invoking `ops::create`.
    fn handle_create_input(&mut self, code: KeyCode) {
        let outcome = if let Mode::CreateInput { slug, .. } = &mut self.mode {
            handle_create_key(slug, code)
        } else {
            return;
        };
        match outcome {
            InputOutcome::Continue => {}
            InputOutcome::Cancel => self.mode = Mode::Browsing,
            InputOutcome::Submit => {
                let (slug, kind) = match std::mem::replace(&mut self.mode, Mode::Browsing) {
                    Mode::CreateInput { slug, kind } => (slug, kind),
                    _ => (String::new(), CreateKind::Roll),
                };
                if is_submittable_slug(&slug) {
                    self.execute_create(slug, kind);
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
                if let Mode::Confirm { action, target, .. } =
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

    /// Answer the `PP` modal. Only an explicit `y` pushes.
    fn handle_push_all(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if let Mode::PushAll { plan } = std::mem::replace(&mut self.mode, Mode::Browsing) {
                    self.push_all_job(plan);
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

    /// Dispatch one keystroke to the `:` prompt. Unlike every other mode's
    /// handler this takes the whole `KeyEvent` rather than just its `code` —
    /// `alt+enter` has to be told apart from a plain `enter`, which needs the
    /// modifiers.
    fn handle_command(&mut self, terminal: &mut super::Tui, key: KeyEvent) -> Result<()> {
        // Cloned rather than borrowed: `handle_command_key` needs it alongside
        // a mutable borrow of `self.mode` for `input`/`cursor`, and the history
        // list is short enough that cloning it once per keystroke is cheaper
        // than fighting the borrow checker over two fields of `self`.
        let history = self.command_history.clone();
        let alt = key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT);
        let outcome = {
            let Mode::Command {
                query,
                input,
                cursor,
            } = &mut self.mode
            else {
                return Ok(());
            };
            handle_command_key(query, input, cursor, &history, key.code, alt)
        };
        match outcome {
            CommandOutcome::Continue => Ok(()),
            CommandOutcome::Close => {
                self.mode = Mode::Browsing;
                Ok(())
            }
            CommandOutcome::Run => {
                let input = self.take_command_input();
                self.execute_command(input);
                Ok(())
            }
            CommandOutcome::RunInShell => {
                let input = self.take_command_input();
                self.run_command_in_shell(terminal, input)
            }
        }
    }

    /// Close the `:` prompt and hand back the text it held, or an empty string
    /// if the mode has already moved on (defensive; the two callers only ever
    /// reach this from inside `Mode::Command`).
    fn take_command_input(&mut self) -> String {
        match std::mem::replace(&mut self.mode, Mode::Browsing) {
            Mode::Command { input, .. } => input,
            other => {
                self.mode = other;
                String::new()
            }
        }
    }

    /// `enter` in the `:` prompt: run `input` as a background job, the same
    /// path every other mutating action uses — its output streams into the
    /// floating panel while the table stays navigable.
    fn execute_command(&mut self, input: String) {
        history::record(&mut self.command_history, &input);
        // Best-effort: a command the user just ran is worth more than a
        // history write that failed because, say, `$HOME` is unset.
        let _ = history::save(&self.command_history);
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
        let title = input.clone();
        self.start_job(title, move || {
            let status = proc::run(&mut shell_command(&shell, &input))?;
            Ok(JobDone::lines(if status.success() {
                Vec::new()
            } else {
                vec![format!("exited with {status}")]
            }))
        });
    }

    /// `alt+enter` (or `shift+enter`, where the terminal reports it
    /// distinguishably) in the `:` prompt: suspend the terminal and run
    /// `input` with inherited stdio, the same shape [`Self::launch_lazygit`]
    /// uses — a shell command may be interactive (an editor, a pager, a
    /// prompt of its own) in a way a piped job never could be.
    fn run_command_in_shell(&mut self, terminal: &mut super::Tui, input: String) -> Result<()> {
        if self.busy() {
            return Ok(());
        }
        history::record(&mut self.command_history, &input);
        let _ = history::save(&self.command_history);
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());

        super::suspend(terminal)?;
        let spawned = shell_command(&shell, &input).status();
        if spawned.is_ok() {
            // Unlike lazygit's own full-screen takeover, a one-shot shell
            // command returns the instant it exits — without this pause,
            // `resume`'s `terminal.clear()` would wipe output nobody had a
            // chance to read.
            println!("\npress enter to return to rf...");
            let mut discard = String::new();
            let _ = std::io::stdin().read_line(&mut discard);
        }
        super::resume(terminal)?;

        match spawned {
            Ok(status) if status.success() => self.message = None,
            Ok(status) => self.message = Some(format!("'{input}' exited with {status}")),
            Err(err) => {
                let mut panel = output::Panel::new(input.clone());
                panel.fail(vec![format!("could not run '{input}': {err}")]);
                self.panel = Some(panel);
            }
        }
        self.reload()
    }

    /// Total number of table rows: the pinned base branches plus the rolls.
    fn row_count(&self) -> usize {
        self.bases.len() + self.rolls.len() + self.hotfixes.len()
    }

    /// Which row the cursor is on, or `None` when the table is empty.
    fn selected_row(&self) -> Option<RowKind> {
        let index = self.table.selected()?;
        row_at(
            index,
            self.bases.len(),
            self.rolls.len(),
            self.hotfixes.len(),
        )
    }

    /// The selected roll, or `None` when the cursor is on a base-branch row —
    /// roll-only actions treat that the same as no selection.
    fn selected_roll(&self) -> Option<&RollInfo> {
        match self.selected_row()? {
            RowKind::Roll(i) => self.rolls.get(i),
            RowKind::Base(_) | RowKind::Hotfix(_) => None,
        }
    }

    /// Validate an action and either open the confirm modal or set a message.
    fn request(&mut self, action: Action) {
        let selected = self.selected_roll();
        let validation = validate_with_hotfixes(
            action,
            selected,
            &self.rolls,
            &self.hotfixes,
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
            // `None` here means "every active local roll", not "no target".
            Action::Update => update_target_for(selected, &self.rolls).unwrap_or(None),
            _ => None,
        };
        let carried = match (action, &target) {
            (Action::Promote, Some(roll)) => carried_by_promoting(&self.config, &self.rolls, roll),
            _ => Vec::new(),
        };
        match validation {
            Ok(()) => {
                self.mode = Mode::Confirm {
                    action,
                    target,
                    carried,
                }
            }
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
        if let Some(RowKind::Hotfix(i)) = self.selected_row() {
            self.message = Some(format!(
                "'{}' is a hotfix — it lands on stable, not into a roll",
                self.hotfixes[i].branch
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
                    carried: Vec::new(),
                }
            }
            Err(msg) => self.message = Some(msg),
        }
    }

    /// `[I]` — merge the rolling branch into the checked-out roll.
    ///
    /// Needs no selection, unlike `[i]`: the source is always rolling, which
    /// isn't a roll row to point the cursor at.
    fn request_integrate_rolling(&mut self) {
        if self.busy() {
            return;
        }
        match integrate_rolling_target_for(
            &self.current_branch,
            &self.config.roll_prefix,
            &self.config.rolling_branch,
        ) {
            Ok(branch) => {
                self.mode = Mode::Confirm {
                    action: Action::Integrate,
                    target: Some(branch),
                    carried: Vec::new(),
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

    /// `[V]` — open the verify-many picker, or say why it cannot run.
    ///
    /// The clean-tree check happens here as well as inside `ops::verify_many`,
    /// so the refusal is a status-bar message before the modal opens rather
    /// than a failed job after the user has already chosen a set.
    fn request_verify_many(&mut self) {
        if self.busy() {
            return;
        }
        if let Err(err) = ops::ensure_clean_state(&self.config) {
            self.message = Some(format!("{err} — verifying many rolls switches branches"));
            return;
        }
        self.mode = Mode::VerifyMany {
            counts: verify_set_counts(&self.rolls),
        };
    }

    /// Handle a keypress while the verify-many picker is open.
    fn handle_verify_many(&mut self, code: KeyCode) {
        match verify_many_key(code) {
            VerifyManyOutcome::Ignore => {}
            VerifyManyOutcome::Cancel => self.mode = Mode::Browsing,
            VerifyManyOutcome::Set(set) => {
                self.mode = Mode::Browsing;
                self.execute_verify_many(set);
            }
        }
    }

    /// Verify every roll in `set`, as one job. Sequential and in one panel: a
    /// pass that switches branches under the table must not race another job,
    /// and the roll-by-roll log reads best as a single scroll.
    fn execute_verify_many(&mut self, set: VerifySet) {
        let selected = set.select(&self.rolls);
        if selected.is_empty() {
            self.message = Some(format!("no rolls to verify ({})", set.label()));
            return;
        }
        let config = self.config.clone();
        let n = selected.len();
        self.start_job(
            format!("rf verify --all ({}, {n})", set.label()),
            move || {
                let results = ops::verify_many(&config, &selected)?;
                let (lines, failed) = render_verify_many(&results);
                if failed.is_empty() {
                    Ok(JobDone::lines(lines))
                } else {
                    // Folded into the error so a failed panel still carries the
                    // per-roll report rather than only the names.
                    Err(anyhow!(
                        "{}\nverification failed for: {}",
                        lines.join("\n"),
                        failed.join(", ")
                    ))
                }
            },
        );
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
        // Rolls and hotfixes alike: `delete_branch_plan` takes any branch that
        // is not stable or rolling, and the safety rules downstream are the
        // same. Only a base row is refused, and `selected_roll` already treats
        // it as no selection.
        let (branch, location, is_current) = match self.selected_row() {
            Some(RowKind::Roll(i)) => {
                let r = &self.rolls[i];
                (r.branch.clone(), r.location.clone(), r.is_current)
            }
            Some(RowKind::Hotfix(i)) => {
                let h = &self.hotfixes[i];
                (h.branch.clone(), h.location.clone(), h.is_current)
            }
            _ => {
                self.message = Some("no roll or hotfix selected".to_string());
                return Ok(());
            }
        };

        let prompt = match delete_prompt(&branch, &location, is_current) {
            Ok(prompt) => prompt,
            Err(msg) => {
                self.message = Some(msg);
                return Ok(());
            }
        };

        let (local_unmerged, remote_unmerged) = ops::unmerged_commit_counts(&self.config, &branch);

        self.mode = Mode::Delete {
            preview: DeletePreview {
                branch,
                prompt,
                local_unmerged,
                remote_unmerged,
                local_is_checked_out: is_current && matches!(location, BranchLocation::Both),
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
        let current = self.current_branch.clone();
        self.start_job(title, move || {
            match run_op(&config, action, target.as_deref()) {
                Ok(lines) => Ok(JobDone::lines(lines)),
                // A conflict is an expected answer to act on, not an error to
                // stop at — the same treatment a rejected push gets. The repo is
                // already clean again; the panel carries the diagnosis and the
                // follow-up modal carries the choices.
                Err(err) => match err.downcast::<ops::MergeConflict>() {
                    Ok(conflict) => Ok(conflict_job_done(&conflict, &current)),
                    Err(err) => Err(err),
                },
            }
        });
    }

    /// Create a roll from `slug`, selecting it once the reload turns it up. An
    /// `ops::create` error (e.g. an invalid slug) lands in the panel like any
    /// other failure and never aborts the TUI.
    fn execute_create(&mut self, slug: String, kind: CreateKind) {
        let config = self.config.clone();
        let title = match kind {
            CreateKind::Roll => "rf create",
            CreateKind::Hotfix => "rf hotfix",
        };
        self.start_job(title, move || {
            let outcome = match kind {
                CreateKind::Roll => ops::create(&config, &slug, None, false)?,
                CreateKind::Hotfix => ops::hotfix_create(&config, &slug, None, false)?,
            };
            Ok(JobDone::with_next(
                vec![format!("Created {}", outcome.branch)],
                Followup::SelectBranch(outcome.branch),
            ))
        });
    }

    /// `[H]` — land the checked-out hotfix. The cursor is consulted only to
    /// explain a refusal: a hotfix row that is not checked out gets told how to
    /// become so, since `ops::hotfix_land` can only merge from HEAD.
    fn request_land_hotfix(&mut self) {
        if let Some(RowKind::Hotfix(i)) = self.selected_row() {
            let hotfix = &self.hotfixes[i];
            if !hotfix.is_current {
                self.message = Some(format!(
                    "[space] onto '{}' first — landing merges from the checked-out branch",
                    hotfix.branch
                ));
                return;
            }
            if hotfix.state == HotfixState::Landed {
                self.message = Some(format!("'{}' is already landed", hotfix.branch));
                return;
            }
        }
        self.request(Action::LandHotfix);
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
            RowKind::Hotfix(i) => (
                self.hotfixes[i].branch.clone(),
                self.hotfixes[i].location.clone(),
            ),
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

    /// `[P]` — arm the push chord. The push itself does not start until the
    /// chord resolves, in [`Self::handle_browsing`] or
    /// [`Self::resolve_lapsed_push`].
    ///
    /// The busy check happens here rather than only at the far end so that a `P`
    /// during a running job is refused the instant it is pressed, as every other
    /// mutating key is, instead of after the window.
    fn arm_push(&mut self) {
        if self.busy() {
            return;
        }
        self.pending_push = Some(Instant::now());
        self.message = Some("P again to push every branch that needs it".to_string());
    }

    /// Fire a pending `P` that nothing followed: the chord window lapsed, so it
    /// was a single-branch push after all. Called once per loop iteration,
    /// before the frame is drawn.
    fn resolve_lapsed_push(&mut self) {
        let lapsed = self
            .pending_push
            .is_some_and(|armed| armed.elapsed() >= PUSH_CHORD_WINDOW);
        if lapsed {
            self.pending_push = None;
            self.message = None;
            self.start_push();
        }
    }

    /// `PP` — plan a push of every branch that needs one and open its modal.
    ///
    /// Nothing is pushed here. The plan is resolved from the tracking data the
    /// view already loaded, so the modal states exactly what the job will do.
    fn start_push_all(&mut self) {
        if self.busy() {
            return;
        }
        let rows: Vec<(String, BranchLocation)> = self
            .bases
            .iter()
            .map(|b| (b.branch.clone(), b.location.clone()))
            .chain(
                self.rolls
                    .iter()
                    .map(|r| (r.branch.clone(), r.location.clone())),
            )
            // Hotfix rows are rows too: `[P]` pushes one unchanged, so `PP`
            // covers them under the same ahead-or-unpublished rule.
            .chain(
                self.hotfixes
                    .iter()
                    .map(|h| (h.branch.clone(), h.location.clone())),
            )
            .collect();
        let plan = plan_push_all(&rows, &self.current_branch, &self.tracking);
        if plan.items.is_empty() {
            self.message = Some(match plan.skipped.len() {
                0 => "nothing to push — every branch is in sync".to_string(),
                n => format!(
                    "nothing to push without a force — {n} branch{} need{} a [P] of its own",
                    if n == 1 { "" } else { "es" },
                    if n == 1 { "s" } else { "" }
                ),
            });
            return;
        }
        self.mode = Mode::PushAll { plan };
    }

    /// Push every branch in the plan, in table order, reporting each one.
    ///
    /// One job rather than one per branch: they share a panel and a reload, and
    /// a half-finished sweep is easier to read as a single log. A branch that
    /// fails does not stop the sweep — the remaining branches are independent
    /// refs, and stopping would leave the user guessing which were reached.
    /// `force` is not a parameter and never will be: see [`plan_push_all`].
    fn push_all_job(&mut self, plan: PushAllPlan) {
        let repo = self.config.repo_root.clone();
        let title = format!("git push ×{}", plan.items.len());
        self.start_job(title, move || {
            let mut lines = Vec::new();
            let mut failed = 0;
            for item in &plan.items {
                let target = &item.target;
                match sync::run_push(&repo, target, false) {
                    Ok(PushOutcome::Pushed) if item.creates => lines.push(format!(
                        "Pushed '{}' to {} (new, now tracking it)",
                        target.branch, target.remote
                    )),
                    Ok(PushOutcome::Pushed) => {
                        lines.push(format!("Pushed '{}' to {}", target.branch, target.remote))
                    }
                    // The plan said this was a fast-forward, so a refusal means
                    // the remote moved since the table was loaded. Reported, not
                    // retried with a force: that decision belongs to `[P]` on
                    // the row, behind its own y/N.
                    Ok(PushOutcome::Rejected { .. }) => {
                        failed += 1;
                        lines.push(format!(
                            "'{}' was rejected by {} — press [P] on the row to force it",
                            target.branch, target.remote
                        ));
                    }
                    Err(err) => {
                        failed += 1;
                        lines.push(format!("'{}' failed: {}", target.branch, sync_error(err)));
                    }
                }
            }
            push_push_skips(&mut lines, &plan.skipped);
            if failed > 0 {
                lines.push(format!(
                    "{failed} of {} branches were not pushed",
                    plan.items.len()
                ));
            }
            Ok(JobDone::lines(lines))
        });
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

    /// Route a mouse event to the output panel. A no-op with no panel up —
    /// there is nothing else on this view a click or scroll drives.
    fn handle_mouse(&mut self, mouse: MouseEvent) {
        let Some(rect) = self.panel_rect else {
            return;
        };
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.panel_maximized = click_maximizes(mouse.column, mouse.row, rect);
            }
            MouseEventKind::ScrollUp => self.scroll_panel(PanelScroll::Up),
            MouseEventKind::ScrollDown => self.scroll_panel(PanelScroll::Down),
            _ => {}
        }
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

    /// Answer the conflict modal.
    fn handle_conflict(&mut self, code: KeyCode) {
        let Mode::Conflict { can_integrate, .. } = &self.mode else {
            return;
        };
        match conflict_key(code, *can_integrate) {
            ConflictOutcome::Ignore => {}
            ConflictOutcome::Close => self.mode = Mode::Browsing,
            ConflictOutcome::Integrate => {
                let Mode::Conflict {
                    source, culprits, ..
                } = std::mem::replace(&mut self.mode, Mode::Browsing)
                else {
                    return;
                };
                self.integrate_culprits_job(source, culprits);
            }
            ConflictOutcome::Stage => {
                let Mode::Conflict { source, target, .. } =
                    std::mem::replace(&mut self.mode, Mode::Browsing)
                else {
                    return;
                };
                self.stage_conflict_job(source, target);
            }
        }
    }

    /// Integrate each culprit into the checked-out roll in turn, stopping at
    /// the first one that conflicts — that conflict is now on the roll branch,
    /// which is the point. When every culprit merges cleanly, the graduation
    /// is retried on the spot, since nothing stands in its way any more.
    fn integrate_culprits_job(&mut self, source: String, culprits: Vec<String>) {
        let config = self.config.clone();
        self.start_job(format!("rf integrate → {source}"), move || {
            let mut lines = Vec::new();
            for culprit in &culprits {
                match ops::integrate(&config, culprit) {
                    Ok(o) => lines.push(format!("Integrated '{}' into '{}'", o.branch, o.current)),
                    Err(err) => {
                        if git::ref_exists(&config.repo_root, "MERGE_HEAD") {
                            lines.push(format!(
                                "'{source}' is now mid-merge with '{culprit}': resolve the \
                                 conflicts and commit (gg for lazygit, or git merge --abort), \
                                 then [G]raduate again"
                            ));
                            return Ok(JobDone::lines(lines));
                        }
                        return Err(err);
                    }
                }
            }
            lines.push("every culprit merged cleanly; retrying the graduation".to_string());
            let force = ops::ForceOpts::new(false, None)?;
            let o = ops::graduate(&config, &source, false, &force, true)?;
            push_gate_notices(&mut lines, &o.gate_notices);
            lines.push(format!("Graduated '{}' into '{}'", o.roll, o.rolling));
            if let Some(line) = o.tag.describe() {
                lines.push(line);
            }
            Ok(JobDone::lines(lines))
        });
    }

    /// Re-run the merge on the target and leave the conflict there. The one
    /// job that leaves `MERGE_HEAD` behind on purpose — and only because the
    /// user pressed the key that asks for exactly that.
    fn stage_conflict_job(&mut self, source: String, target: String) {
        let config = self.config.clone();
        self.start_job(format!("git merge {source} (left for you)"), move || {
            let lines = if ops::stage_conflict(&config, &source, &target)? {
                vec![format!(
                    "'{target}' is checked out mid-merge with '{source}': resolve the conflicts \
                     and commit (gg for lazygit), or git merge --abort to back out"
                )]
            } else {
                vec![format!(
                    "the merge of '{source}' into '{target}' went through cleanly this time and \
                     is committed"
                )]
            };
            Ok(JobDone::lines(lines))
        });
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
        self.hotfixes = branches::list_hotfixes(&self.config).unwrap_or_default();
        self.tracking = load_tracking(&self.config);
        self.versions = load_versions(&self.config, &self.bases, &self.rolls, &self.hotfixes);
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
        // area so the keymap stays readable while a job runs. Maximized, it
        // still only grows into this area, never over the header/status bar.
        self.panel_rect = self
            .panel
            .as_ref()
            .map(|panel| output::render(f, chunks[1], panel, self.panel_maximized));

        match &self.mode {
            Mode::Confirm {
                action,
                target,
                carried,
            } => {
                render_modal(
                    f,
                    area,
                    &self.config,
                    &ConfirmModal {
                        action: *action,
                        target: target.as_deref(),
                        carried,
                        rolls: &self.rolls,
                        hotfixes: &self.hotfixes,
                        current_branch: &self.current_branch,
                    },
                );
            }
            Mode::Detail {
                roll,
                ahead_behind,
                cursor,
                trail,
            } => {
                let path: Vec<u32> = trail.iter().map(|(r, _)| r.number).collect();
                render_detail(f, area, roll, *ahead_behind, &self.rolls, *cursor, &path)
            }
            Mode::CreateInput { slug, kind } => {
                render_create_input(f, area, &self.config, slug, *kind)
            }
            Mode::Delete { preview } => render_delete_modal(f, area, &self.config, preview),
            Mode::Bump { current } => render_bump_modal(f, area, *current, &self.current_branch),
            Mode::VerifyMany { counts } => render_verify_many_modal(f, area, counts),
            Mode::ForcePush { branch, remote } => {
                render_force_push_modal(f, area, branch, remote, self.tracking.get(branch))
            }
            Mode::Conflict {
                source,
                target,
                culprits,
                can_integrate,
            } => render_conflict_modal(f, area, source, target, culprits, *can_integrate),
            Mode::PushAll { plan } => render_push_all_modal(f, area, plan),
            Mode::Help { query, cursor } => render_help(f, area, query, *cursor),
            Mode::Command {
                query,
                input,
                cursor,
            } => render_command(f, area, query, input, *cursor, &self.command_history),
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
        // The corner names the binary that is running, not the checked-out
        // branch's manifest. The two used to be conflated, and the corner would
        // change on every `[space]` — in a repo that *is* roll-flow it read as
        // the roll's dev version, in any other repo as whatever that repo ships.
        // Neither is what "which rf is this" asks. The per-branch versions have
        // their own column; the bump modal shows the manifest it will raise.
        let block = Block::bordered().title(" roll-flow ").title_top(
            Line::from(Span::styled(
                format!(" rf v{} ", env!("CARGO_PKG_VERSION")),
                Style::default().fg(Color::Magenta),
            ))
            .right_aligned(),
        );
        f.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
    }

    fn render_table(&mut self, f: &mut Frame, area: Rect) {
        let mut col_constraints = vec![
            // The current-branch chevron, narrow and always present so the
            // columns after it do not shift as HEAD moves.
            Constraint::Length(1),
            // The graph column: two lane glyphs (main, rolling) plus a cell of
            // breathing room. TUI-only — see graph_glyphs' doc comment.
            Constraint::Length(3),
            Constraint::Length(4),
            Constraint::Fill(1),
            Constraint::Length(3),
            // `↑12↓12` at its widest.
            Constraint::Length(7),
            Constraint::Length(13),
        ];
        // Present only in repos that have a `Cargo.toml`, the same rule the
        // header version follows — so a dotfiles repo pays nothing for it.
        // Sized to the longest cell on screen (a dev marker like `-roll10`
        // makes this wider than a bare `12.34.5`), the same way `main.rs`'s
        // `version_column` sizes the plain tables; `branch` is the Fill
        // column that pays for it.
        let show_versions = !self.versions.is_empty();
        if show_versions {
            let width = self
                .versions
                .values()
                .map(|v| v.to_string().chars().count())
                .max()
                .unwrap_or(0)
                .max("version".len());
            col_constraints.push(Constraint::Length(width as u16));
        }
        if self.show_deps {
            col_constraints.push(Constraint::Length(8));
            // Exactly the header width: the values are short comma lists, and
            // `branch` is the Fill column that pays for anything wider.
            col_constraints.push(Constraint::Length(10));
        }

        let bold = Style::default().add_modifier(Modifier::BOLD);
        let mut header_cells = vec![
            Cell::from(""),
            Cell::from(""),
            Cell::from("#").style(bold),
            Cell::from("branch").style(bold),
            Cell::from("loc").style(bold),
            Cell::from("sync").style(bold),
            Cell::from("state").style(bold),
        ];
        if show_versions {
            header_cells.push(Cell::from("version").style(bold));
        }
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
        let versions = &self.versions;
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
                    Cell::from(graph_cell(base_graph_glyphs(base.role))),
                    Cell::from(""),
                    Cell::from(base.branch.clone()).style(base_style.fg(base.role.color())),
                    Cell::from(base.location.symbol()).style(base_style),
                    Cell::from(sync_text).style(Style::default().fg(sync_color)),
                    Cell::from(base.role.label()).style(Style::default().fg(base.role.color())),
                ];
                if show_versions {
                    cells.push(
                        Cell::from(version_cell(versions.get(&base.branch).copied()))
                            .style(base_style),
                    );
                }
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
                Cell::from(graph_cell(graph_glyphs(&roll.state))),
                Cell::from(roll.number.to_string()).style(base_style),
                Cell::from(roll.branch.clone()).style(base_style),
                Cell::from(roll.location.symbol()).style(base_style),
                Cell::from(sync_text).style(Style::default().fg(sync_color)),
                Cell::from(roll.state.label()).style(Style::default().fg(row_state_color)),
            ];
            if show_versions {
                cells.push(
                    Cell::from(version_cell(versions.get(&roll.branch).copied())).style(base_style),
                );
            }
            if show_deps {
                cells.push(Cell::from(branches::format_deps(roll)));
                cells.push(Cell::from(branches::format_roll_numbers(&roll.dependents)));
            }
            Row::new(cells)
        }));
        // Hotfixes last, numbered `h<N>` so their independent numbering is never
        // read as a roll's. Same columns, so the sync keys work unchanged.
        let first_hotfix = rows.len();
        rows.extend(self.hotfixes.iter().enumerate().map(|(i, hotfix)| {
            let base_style = if hotfix.is_current {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let (sync_text, sync_color) = sync_cell(track_of(tracking, &hotfix.branch));
            let color = hotfix_color(hotfix.state);
            let mut cells = vec![
                Cell::from(current_marker(hotfix.is_current)).style(
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Cell::from(graph_cell(hotfix_graph_glyphs(hotfix.state))),
                Cell::from(format!("h{}", hotfix.number)).style(base_style.fg(color)),
                Cell::from(hotfix.branch.clone()).style(base_style.fg(color)),
                Cell::from(hotfix.location.symbol()).style(base_style),
                Cell::from(sync_text).style(Style::default().fg(sync_color)),
                Cell::from(hotfix.state.label()).style(Style::default().fg(color)),
            ];
            if show_versions {
                cells.push(
                    Cell::from(version_cell(versions.get(&hotfix.branch).copied()))
                        .style(base_style),
                );
            }
            if show_deps {
                cells.push(Cell::from(""));
                cells.push(Cell::from(""));
            }
            // The first hotfix row keeps a blank line above it, which the rule
            // below is drawn into: the tiers number independently, so they
            // should not read as one list.
            Row::new(cells).top_margin(u16::from(i == 0))
        }));

        let table = Table::new(rows, col_constraints)
            .header(table_header)
            .block(Block::bordered().title(" branches "))
            .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .highlight_symbol("▶ ");

        f.render_stateful_widget(table, area, &mut self.table);

        // Drawn over that margin rather than as a row of its own, so it is never
        // selectable and every index `row_at` maps stays as it was. The table
        // has scrolled by `offset` rows (all one line tall above the hotfixes);
        // when the first hotfix is the top visible row there is nothing above
        // it to separate from, and the margin is not drawn over.
        if !self.hotfixes.is_empty() && first_hotfix > self.table.offset() {
            // Top border and header, then one line per visible row above.
            let y = area.y + 2 + (first_hotfix - self.table.offset()) as u16;
            if y + 1 < area.bottom() {
                let rule = Rect {
                    x: area.x + 1,
                    y,
                    width: area.width.saturating_sub(2),
                    height: 1,
                };
                f.render_widget(
                    Paragraph::new("┄".repeat(rule.width as usize))
                        .style(Style::default().fg(Color::DarkGray)),
                    rule,
                );
            }
        }
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
/// The dev-marker *application* is not on that list and is not optional: it
/// uses `ops::apply_dev_version_for_branch`, the same function `cmd_verify`
/// calls, specifically so the two cannot drift apart on it again — this
/// function used to claim the "line for line" mirror while actually omitting
/// the marker step, since it was inlined separately in each caller instead of
/// living in one shared place.
///
/// A failed host or an unsatisfied version gate is an `Err`, not a line: the
/// panel marks a failed job, and a verdict that reads as "done" when it is
/// really "blocked" is the one outcome worth being loud about. Everything the
/// gates printed is already in the panel either way, streamed as they ran.
fn run_verify(config: &Config) -> Result<Vec<String>> {
    ops::ensure_clean_state(config)?;

    let mut lines = Vec::new();
    let current = git::current_branch(&config.repo_root)?;
    if let Some(dev) = ops::apply_dev_version_for_branch(config, &current)? {
        lines.push(format!("version marked {dev}"));
    }

    let outcome = ops::verify(config, false)?;
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
            // The modal listed the chain, so this is the confirmation. Each
            // step is the ordinary graduation with its own gate run; a failure
            // partway names what landed and what did not.
            let rolls = branches::list_rolls(config)?;
            let chain = ops::dependency_chain(&rolls, roll, ops::ChainKind::Graduate)?;
            let outcomes = ops::graduate_chain(config, &chain, false, &force, true, ops::graduate)?;
            for (step, o) in chain.iter().zip(&outcomes) {
                push_gate_notices(&mut lines, &o.gate_notices);
                if o.restored {
                    lines.push(format!(
                        "Restored '{}' on '{}' (reverted the revert)",
                        o.roll, o.rolling
                    ));
                } else {
                    lines.push(format!("Graduated '{}' into '{}'", o.roll, o.rolling));
                }
                if !step.carries.is_empty() {
                    lines.push(step.carried_line(&rolls, false));
                }
                if let Some(line) = o.tag.describe() {
                    lines.push(line);
                }
            }
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
                // Graduated-but-unpromoted dependencies first, each its own
                // step — the same expansion `rf promote --roll` performs.
                Some(roll) => {
                    let rolls = branches::list_rolls(config)?;
                    let chain = ops::dependency_chain(&rolls, roll, ops::ChainKind::Promote)?;
                    ops::PromoteTarget::Rolls(chain.into_iter().map(|s| s.branch).collect())
                }
                None => ops::PromoteTarget::Rolling,
            };
            // Tagging is on; the version gate hard-fails here rather than
            // prompting, since the TUI has no place to offer a bump — the
            // error names the `rf promote --bump` fix.
            let o = ops::promote(config, &promote_target, false, &force, true, None)?;
            for step in &o.steps {
                push_gate_notices(&mut lines, &step.gate_notices);
                push_gate_notices(&mut lines, &step.host_notices);
                push_host_results(&mut lines, &step.host_results);
                let what = step.roll.as_deref().unwrap_or(&o.rolling);
                lines.push(format!("Promoted '{}' into '{}'", what, o.stable));
                for carried in &step.carried {
                    lines.push(format!("  also landed: {carried}"));
                }
                if let Some(line) = step.tag.describe() {
                    lines.push(line);
                }
            }
            for skip in &o.skipped {
                lines.push(format!("skipped '{}': {}", skip.roll, skip.reason));
            }
        }
        Action::Update => {
            let update_target = match target {
                Some(roll) => ops::UpdateTarget::Rolls(vec![roll.to_string()]),
                None => ops::UpdateTarget::AllActive,
            };
            match ops::update(config, &update_target, false)? {
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
            }
        }
        Action::Prune => {
            // The modal was the confirmation, so plan and apply run back to
            // back here. `PruneScope::both` never forces: a branch holding
            // commits stable lacks is reported as skipped, and clearing it
            // needs `rf prune --force` from the CLI, deliberately.
            let plan = ops::prune_plan(config, &ops::PruneScope::both())?;
            if plan.is_empty() {
                lines.push("no promoted roll branches or landed hotfixes to prune".to_string());
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
                lines.push("no local roll branches or hotfixes to tidy".to_string());
                push_prune_skips(&mut lines, &plan.skipped);
            } else {
                let results = ops::prune_apply(config, &plan)?;
                lines.extend(render_prune_outcome(&plan, &results));
            }
        }
        Action::LandHotfix => {
            // The same op `rf hotfix --land` runs, gates and all; never dry,
            // never forced — a hotfix that fails its gates is fixed on the
            // branch, not pushed past them from a keypress.
            ops::ensure_clean_state(config)?;
            let o = ops::hotfix_land(config, false)?;
            push_gate_notices(&mut lines, &o.gate_notices);
            lines.push(format!("Landed '{}' into '{}'", o.current, o.stable));
            lines.push(format!("Reintegrated '{}' into '{}'", o.stable, o.rolling));
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

/// Read the crate version at every branch on screen, keyed by branch name.
///
/// The refspec per row follows the same local-first order as
/// `git::resolve_branch`: a branch with a local copy is read at its own tip, a
/// remote-only one at `origin/<branch>`. One `cat-file --batch` covers the lot.
///
/// Degrades to an empty map rather than failing the reload — a missing version
/// renders as a dash, and no repo should lose its table over a decoration. That
/// also covers the ordinary case of a repo with no `Cargo.toml` at all, where
/// the column simply never appears.
fn load_versions(
    config: &Config,
    bases: &[BaseBranch],
    rolls: &[RollInfo],
    hotfixes: &[HotfixInfo],
) -> HashMap<String, Semver> {
    let rows: Vec<(&String, &BranchLocation)> = bases
        .iter()
        .map(|b| (&b.branch, &b.location))
        .chain(rolls.iter().map(|r| (&r.branch, &r.location)))
        .chain(hotfixes.iter().map(|h| (&h.branch, &h.location)))
        .collect();
    let refs: Vec<String> = rows
        .iter()
        .map(|(branch, location)| branches::content_ref(branch, location))
        .collect();

    let by_ref = version::versions_at(&config.repo_root, &refs);
    rows.iter()
        .map(|(branch, _)| *branch)
        .zip(refs.iter())
        .filter_map(|(branch, refspec)| Some((branch.clone(), *by_ref.get(refspec)?)))
        .collect()
}

/// The version cell for `branch`: the full version through `Semver`'s
/// `Display` — numbers plus a `-rollN` dev marker when the branch carries one
/// — or a dash.
///
/// The `#` column also names the roll, but not whether its dev marker has
/// actually been applied yet: a roll created before the marker existed, or
/// with `--no-dev-version`, reads as a plain release version until its first
/// `rf verify` (see `ops::apply_dev_version`). The version cell is the only
/// place that distinction is visible.
pub(crate) fn version_cell(version: Option<Semver>) -> String {
    match version {
        Some(v) => v.to_string(),
        None => "—".to_string(),
    }
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
/// Render a verify-many pass as printable lines, plus the branches that failed.
///
/// Mirrors `run_verify`'s vocabulary roll by roll, then a summary line, so a
/// six-roll pass reads like six `[v]` results stacked — and never offers a
/// bump, which is a commit on one branch where this pass walks many.
fn render_verify_many(results: &[ops::VerifyManyResult]) -> (Vec<String>, Vec<String>) {
    let mut lines = Vec::new();
    let mut failed = Vec::new();
    let mut passed = 0;
    let mut skipped = 0;
    for result in results {
        lines.push(format!("── {} ──", result.branch));
        if let Some(dev) = result.marked {
            lines.push(format!("version marked {dev}"));
        }
        if let Some(o) = &result.outcome {
            if o.diverged_note {
                lines.push(format!(
                    "note: '{}' has commits not in '{}'; graduation/promotion will create a --no-ff merge",
                    o.target, o.source
                ));
            }
            push_version_check(&mut lines, &o.version, &o.source, &o.target);
            push_gate_notices(&mut lines, &o.gate_notices);
            push_gate_notices(&mut lines, &o.host_notices);
            push_host_results(&mut lines, &o.host_results);
        }
        match &result.verdict {
            ops::VerifyVerdict::Passed => {
                passed += 1;
                lines.push("PASSED".to_string());
            }
            ops::VerifyVerdict::Failed(why) => {
                failed.push(result.branch.clone());
                lines.push(format!("FAILED: {why}"));
            }
            ops::VerifyVerdict::Skipped(why) => {
                skipped += 1;
                lines.push(format!("skipped: {why}"));
            }
        }
    }
    lines.push(format!(
        "{passed} passed, {} failed, {skipped} skipped",
        failed.len()
    ));
    (lines, failed)
}

/// Render the `[V]` picker: each set with how many rolls it would cover.
fn render_verify_many_modal(f: &mut Frame, area: Rect, counts: &[(VerifySet, usize); 6]) {
    let mut lines = vec![
        Line::from(Span::styled(
            "Verify which rolls? (switches to each, then back)",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    for (i, (set, n)) in counts.iter().enumerate() {
        lines.push(Line::from(vec![
            Span::styled(format!("[{}] ", i + 1), Style::default().fg(Color::Yellow)),
            Span::raw(format!("{:<24}", set.label())),
            Span::styled(
                format!("{n} roll{}", if *n == 1 { "" } else { "s" }),
                Style::default().fg(Color::DarkGray),
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
        Paragraph::new(lines).block(Block::bordered().title(" verify all ")),
        modal,
    );
}

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
/// What a keypress in the conflict modal does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConflictOutcome {
    Integrate,
    Stage,
    Close,
    Ignore,
}

/// Decide the conflict modal's key. `[i]` is only live when the source is the
/// checked-out roll — `ops::integrate` merges into HEAD, so offering it for
/// any other row would integrate into the wrong branch. Enter is unbound on
/// purpose: every choice here leaves something to resolve by hand.
pub(crate) fn conflict_key(code: KeyCode, can_integrate: bool) -> ConflictOutcome {
    match code {
        KeyCode::Char('i') | KeyCode::Char('I') if can_integrate => ConflictOutcome::Integrate,
        KeyCode::Char('m') | KeyCode::Char('M') => ConflictOutcome::Stage,
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => ConflictOutcome::Close,
        _ => ConflictOutcome::Ignore,
    }
}

/// Turn a diagnosed conflict into a finished job: the report as panel lines,
/// plus the follow-up that opens the modal.
fn conflict_job_done(conflict: &ops::MergeConflict, current_branch: &str) -> JobDone {
    let report = &conflict.report;
    let culprits = report.culprit_rolls();
    let mut lines = vec![conflict.to_string(), String::new()];
    lines.extend(report.render());
    lines.push(String::new());
    if culprits.is_empty() {
        lines.push(format!(
            "the conflicting change was made on '{}' directly, not by a roll",
            report.target
        ));
    } else {
        lines.push(format!(
            "the conflicting change is already on '{}' — it came in with {}",
            report.target,
            culprits.join(", ")
        ));
    }
    lines.push(format!(
        "NOT merged: '{}' is unchanged and '{}' is clean",
        report.target, conflict.original
    ));
    JobDone::with_next(
        lines,
        Followup::OfferConflictResolution {
            source: report.source.clone(),
            target: report.target.clone(),
            can_integrate: report.source == current_branch && !culprits.is_empty(),
            culprits,
        },
    )
}

/// Render the conflict modal: which rolls collided, and the keys that decide
/// what happens next. Red border, like the delete modal — every choice here
/// leaves a merge for the user to finish.
fn render_conflict_modal(
    f: &mut Frame,
    area: Rect,
    source: &str,
    target: &str,
    culprits: &[String],
    can_integrate: bool,
) {
    let red = Style::default().fg(Color::Red);
    let dim = Style::default().fg(Color::DarkGray);
    let mut lines = vec![
        Line::from(Span::styled(
            format!("'{source}' conflicts with '{target}'"),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    if culprits.is_empty() {
        lines.push(Line::from(
            "The other side was changed directly, not by a roll.",
        ));
    } else {
        lines.push(Line::from(format!(
            "Already on {target} via: {}",
            culprits.join(", ")
        )));
    }
    lines.push(Line::from(""));
    if can_integrate {
        lines.push(Line::from(Span::styled(
            "[i] integrate them into this roll and resolve here   (recommended)",
            Style::default().fg(Color::Yellow),
        )));
    } else if !culprits.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("[space] switch to {source} first to integrate them here"),
            dim,
        )));
    }
    lines.push(Line::from(Span::styled(
        format!("[m] redo the merge on {target} and leave it for lazygit"),
        Style::default().fg(Color::Yellow),
    )));
    lines.push(Line::from(Span::styled(
        "[n] nothing — the panel has the details",
        Style::default().fg(Color::Yellow),
    )));

    let width = lines
        .iter()
        .map(|l| l.width())
        .max()
        .unwrap_or(40)
        .clamp(32, 78) as u16;
    let modal = centered_rect(area, width + 4, lines.len() as u16 + 2);
    f.render_widget(Clear, modal);
    let body = Paragraph::new(lines)
        .alignment(Alignment::Left)
        .block(Block::bordered().border_style(red).title(" conflict "));
    f.render_widget(body, modal);
}

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

/// Everything the confirm modal reads, gathered so the renderer takes one
/// parameter per *thing* rather than one per field.
struct ConfirmModal<'a> {
    action: Action,
    /// The roll a roll-scoped action targets; `None` for the repo-wide shapes.
    target: Option<&'a str>,
    /// See [`Mode::Confirm`]'s field of the same name.
    carried: &'a [String],
    rolls: &'a [RollInfo],
    /// Counted alongside `rolls` by `[x]` and `[t]`, which reach landed hotfixes.
    hotfixes: &'a [HotfixInfo],
    current_branch: &'a str,
}

/// Render the centered confirmation popup for a pending action.
///
/// `carried` is non-empty only for `[m]` on a roll row, and the modal then grows
/// to list those rolls: the merge lands them on stable too, and the confirmation
/// is the last place the user can see that before it happens.
/// What a prune or tidy confirmation is about to delete: `n` roll branches
/// described by `rolls`, and `hotfixes` landed hotfixes. The hotfix half only
/// appears when there is one, so a roll-only prompt reads exactly as before.
fn deletion_count(n: usize, rolls: &str, hotfixes: usize) -> String {
    let plural = |n: usize, one: &str, many: &str| {
        if n == 1 {
            one.to_string()
        } else {
            many.to_string()
        }
    };
    let roll_part = format!("{n} {rolls} {}", plural(n, "branch", "branches"));
    match (n, hotfixes) {
        (_, 0) => roll_part,
        (0, h) => format!("{h} landed {}", plural(h, "hotfix", "hotfixes")),
        (_, h) => format!(
            "{roll_part} and {h} landed {}",
            plural(h, "hotfix", "hotfixes")
        ),
    }
}

fn render_modal(f: &mut Frame, area: Rect, config: &Config, modal: &ConfirmModal) {
    let ConfirmModal {
        action,
        target,
        carried,
        rolls,
        hotfixes,
        current_branch,
    } = *modal;
    // Lines under the prompt: the ordered steps when an action lands more than
    // the one roll the cursor is on. Empty for everything else, so the modal
    // keeps its two-line shape for the ordinary case.
    let mut detail: Vec<String> = Vec::new();
    let prompt = match action {
        Action::Graduate => {
            match target.and_then(|t| chain_lines(rolls, t, ops::ChainKind::Graduate)) {
                Some(lines) => {
                    let n = lines.len();
                    detail = lines;
                    format!(
                        "Graduate {n} rolls into {}, in this order?",
                        config.rolling_branch
                    )
                }
                None => format!(
                    "Graduate {} into {}?",
                    target.unwrap_or("(selected roll)"),
                    config.rolling_branch
                ),
            }
        }
        Action::Integrate => format!(
            "Integrate {} into {}?",
            target.unwrap_or("(selected roll)"),
            current_branch
        ),
        // `target` is the selected roll when `[p]` was pressed on one, and
        // `None` for a whole-branch promotion — the prompt must say which,
        // because the two differ enormously in what they land on stable.
        Action::Promote => match target {
            Some(roll) => match chain_lines(rolls, roll, ops::ChainKind::Promote) {
                Some(lines) => {
                    let n = lines.len();
                    detail = lines;
                    format!(
                        "Promote {n} rolls into {}, in this order?",
                        config.stable_branch
                    )
                }
                None => format!("Promote {} into {}?", roll, config.stable_branch),
            },
            None => format!(
                "Promote all of {} into {}?",
                config.rolling_branch, config.stable_branch
            ),
        },
        // `target` is the selected roll when `[u]` was pressed on one, and
        // `None` for every active local roll.
        Action::Update => match target {
            Some(roll) => format!("Update {} from {}?", roll, config.stable_branch),
            None => format!(
                "Update all active local rolls from {}?",
                config.stable_branch
            ),
        },
        Action::Prune => {
            let what = deletion_count(
                prunable_count(rolls),
                "promoted roll",
                prunable_hotfix_count(hotfixes),
            );
            format!("Delete {what} (local + origin)?")
        }
        // Says "local only" where prune says "local + origin": the two keys sit
        // next to each other and differ in exactly that, so the prompt is where
        // the difference has to be visible.
        Action::Tidy => {
            let what = deletion_count(
                tidyable_count(rolls),
                "local graduated/promoted roll",
                tidyable_hotfix_count(hotfixes),
            );
            format!("Delete {what} (local only)?")
        }
        // Both merges named, because the second is the one people forget:
        // landing writes to stable *and* to rolling.
        Action::LandHotfix => format!(
            "Land {} into {}, then reintegrate into {}?",
            current_branch, config.stable_branch, config.rolling_branch
        ),
    };
    let hint = "[y] confirm    [n] cancel";

    let mut lines = vec![Line::from(prompt)];
    lines.extend(detail.into_iter().map(Line::from));
    if !carried.is_empty() {
        let yellow = Style::default().fg(Color::Yellow);
        lines.push(Line::from(Span::styled(
            "also lands, in graduation order:",
            yellow,
        )));
        for roll in carried {
            lines.push(Line::from(Span::styled(roll.clone(), yellow)));
        }
    }
    lines.push(Line::from(hint));

    let width = lines.iter().map(|l| l.width()).max().unwrap_or(20) as u16 + 4;
    let modal = centered_rect(area, width, lines.len() as u16 + 2);

    f.render_widget(Clear, modal);
    let body = Paragraph::new(lines)
        .alignment(Alignment::Center)
        .block(Block::bordered().title(" confirm "));
    f.render_widget(body, modal);
}

/// The numbered steps of a chain that lands more than `target` alone, or `None`
/// when it is just the one roll (or cannot be planned — validation has already
/// said why, and the modal is not the place to repeat it).
fn chain_lines(rolls: &[RollInfo], target: &str, kind: ops::ChainKind) -> Option<Vec<String>> {
    let chain = ops::dependency_chain(rolls, target, kind).ok()?;
    // A lone step is still worth listing when it is not `target` (a carried
    // cycle member graduates by way of its carrier) or carries other rolls:
    // either way more lands than the row under the cursor.
    if ops::chain_is_lone(&chain, target) && chain.iter().all(|s| s.carries.is_empty()) {
        return None;
    }
    Some(
        chain
            .iter()
            .enumerate()
            .map(|(i, step)| format!("{}. {}", i + 1, step.describe()))
            .collect(),
    )
}

/// The rolls that `[m]` on `roll` would land on stable besides `roll` itself
/// and the graduated-but-unpromoted dependencies its chain already names (those
/// are listed as steps of their own, see [`chain_lines`]).
///
/// Best-effort: a planning failure yields an empty list rather than an error.
/// The keypress opens a confirmation, and `ops::promote` reports the real
/// problem a moment later if there is one — refusing to draw the modal because
/// the disclosure could not be computed would be the worse trade.
fn carried_by_promoting(config: &Config, rolls: &[RollInfo], roll: &str) -> Vec<String> {
    // The same expansion `run_op` promotes, so the disclosure matches the merge.
    let named: Vec<String> = ops::dependency_chain(rolls, roll, ops::ChainKind::Promote)
        .map(|chain| chain.into_iter().map(|s| s.branch).collect())
        .unwrap_or_else(|_| vec![roll.to_string()]);
    ops::preview_roll_promotion(config, &named)
        .map(|preview| {
            preview
                .steps
                .into_iter()
                .flat_map(|step| step.carried)
                .collect()
        })
        .unwrap_or_default()
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

/// How many branches the `PP` modal lists before it summarises the rest. Chosen
/// so the modal still fits an 80×24 terminal once the skip lines and the hint
/// are added.
const PUSH_ALL_LIST_LIMIT: usize = 8;

/// Render the `PP` confirmation: what will be pushed, what will not, and why.
///
/// A bulk write to the remote has to be legible before it happens, so the
/// branches are listed rather than counted — the count alone would not show that
/// the one branch the user cared about is in the skipped half.
fn render_push_all_modal(f: &mut Frame, area: Rect, plan: &PushAllPlan) {
    let dim = Style::default().fg(Color::DarkGray);
    let n = plan.items.len();
    let mut lines = vec![Line::from(Span::styled(
        format!(
            "Push {n} branch{} to their remotes?",
            if n == 1 { "" } else { "es" }
        ),
        Style::default().add_modifier(Modifier::BOLD),
    ))];

    for item in plan.items.iter().take(PUSH_ALL_LIST_LIMIT) {
        let note = if item.creates {
            "new on the remote".to_string()
        } else {
            match item.target.track {
                Some(TrackState::Ahead(n)) => format!("↑{n}"),
                // Unreachable via `plan_push_all`, which only ever queues the two
                // states above; rendered rather than asserted so a future state
                // shows up in the modal instead of panicking in a draw.
                _ => String::new(),
            }
        };
        lines.push(Line::from(vec![
            Span::styled(item.target.branch.clone(), Style::default().fg(Color::Cyan)),
            Span::raw("  "),
            Span::styled(note, Style::default().fg(Color::Green)),
        ]));
    }
    if n > PUSH_ALL_LIST_LIMIT {
        lines.push(Line::from(Span::styled(
            format!("… and {} more", n - PUSH_ALL_LIST_LIMIT),
            dim,
        )));
    }

    for skip in plan.skipped.iter().take(PUSH_ALL_LIST_LIMIT) {
        lines.push(Line::from(Span::styled(
            format!("skipped {}: {}", skip.branch, skip.reason),
            Style::default().fg(Color::Yellow),
        )));
    }
    if plan.skipped.len() > PUSH_ALL_LIST_LIMIT {
        lines.push(Line::from(Span::styled(
            format!(
                "… and {} more skipped",
                plan.skipped.len() - PUSH_ALL_LIST_LIMIT
            ),
            dim,
        )));
    }

    lines.push(Line::from(Span::styled("[y] confirm    [n] cancel", dim)));

    let width = lines.iter().map(|l| l.width()).max().unwrap_or(32) as u16 + 4;
    let height = lines.len() as u16 + 2;
    let modal = centered_rect(area, width.max(36), height);

    f.render_widget(Clear, modal);
    let body = Paragraph::new(lines)
        .alignment(Alignment::Center)
        .block(Block::bordered().title(" push all "));
    f.render_widget(body, modal);
}

/// Append the skipped branches to a `PP` job's output, so the log says the same
/// thing the modal did rather than silently omitting them.
fn push_push_skips(lines: &mut Vec<String>, skipped: &[PushSkip]) {
    for skip in skipped {
        lines.push(format!("skipped '{}': {}", skip.branch, skip.reason));
    }
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

/// Render the `:` command prompt: the input line on top, the fuzzy-matched
/// history below it. Exactly [`render_help`]'s shape against a plain list of
/// strings instead of [`BINDINGS`].
fn render_command(
    f: &mut Frame,
    area: Rect,
    query: &str,
    input: &str,
    cursor: usize,
    history: &[String],
) {
    // The list is filtered by `query` (what was typed), not `input` (what
    // will run) — after an Up/Down pick they differ, and filtering by the
    // picked command instead would collapse the candidate list to just itself.
    let matches = filter_history(history, query);
    let dim = Style::default().fg(Color::DarkGray);

    let visible = (area.height.saturating_sub(8) as usize).max(1);
    let rows = matches.len().min(visible);
    let first = cursor.saturating_sub(rows.saturating_sub(1));

    let mut lines = vec![
        Line::from(vec![
            Span::styled(": ", Style::default().fg(Color::Cyan)),
            Span::raw(input.to_string()),
            Span::styled("_", Style::default().fg(Color::Cyan)),
        ]),
        Line::from(""),
    ];

    if matches.is_empty() {
        lines.push(Line::from(Span::styled("no history yet", dim)));
    }
    for (row, &index) in matches.iter().enumerate().skip(first).take(rows) {
        let selected = row == cursor;
        let style = if selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            Span::raw(if selected { "▶ " } else { "  " }),
            Span::styled(history[index].clone(), style),
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
        "[↑/↓] history   [enter] run here   [alt+enter] run in shell   [esc] cancel",
        dim,
    )));

    let width = lines.iter().map(|l| l.width()).max().unwrap_or(40) as u16 + 4;
    let height = lines.len() as u16 + 2;
    let modal = centered_rect(area, width.max(60), height);

    f.render_widget(Clear, modal);
    f.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" : ")),
        modal,
    );
}

/// Render the centered slug-input popup for creating a new roll. Shows the
/// prompt, the current buffer with a trailing caret, and the key hints.
fn render_create_input(f: &mut Frame, area: Rect, config: &Config, buffer: &str, kind: CreateKind) {
    let prompt = format!(
        "New {} slug (branched from {}):",
        kind.noun(),
        config.stable_branch
    );
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
    .block(Block::bordered().title(format!(" create {} ", kind.noun())));
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
    cursor: usize,
    trail: &[u32],
) {
    let chain = dep_chain(roll, all);
    let dependents = dependent_rows(roll, all);
    // The cursor marker: chain rows first, then dependents, matching
    // `detail_targets` exactly so what is highlighted is what enter opens.
    let cursor_mark = |index: usize| if index == cursor { "▶ " } else { "  " };

    let mut lines = Vec::new();
    // Where this pane was reached from, so a deep dig still reads as a path
    // rather than as a pane that appeared from nowhere.
    if !trail.is_empty() {
        let mut crumbs = String::new();
        for n in trail {
            crumbs.push_str(&format!("#{n} → "));
        }
        crumbs.push_str(&format!("#{}", roll.number));
        lines.push(Line::from(Span::styled(
            crumbs,
            Style::default().fg(Color::DarkGray),
        )));
    }
    lines.extend([
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
    ]);

    // Local-vs-origin divergence, only for both-location rolls (issue #99).
    if let Some(text) = format_ahead_behind(ahead_behind) {
        lines.push(Line::from(Span::styled(
            text,
            Style::default().fg(Color::Yellow),
        )));
    }

    lines.push(Line::from(""));

    // A cycle explains what the dependency rows alone cannot: why two rolls
    // are each waiting on the other, and which one to graduate to end it.
    if let Some(cycle) = &roll.cycle {
        lines.extend(
            cycle_lines(cycle, all)
                .into_iter()
                .map(|text| Line::from(Span::styled(text, Style::default().fg(Color::Cyan)))),
        );
        lines.push(Line::from(""));
    }

    if chain.is_empty() {
        lines.push(Line::from(Span::styled(
            "no dependencies / not blocked",
            Style::default().fg(Color::Green),
        )));
    } else {
        // Blockers anywhere in the chain, each counted once: a dependency's
        // own ungraduated dependency holds this roll back just as surely.
        // Stale links are counted per link, since each is a separate
        // integration — the same roll reached twice may be stale in one
        // parent and current in the other.
        let blockers = chain.iter().filter(|r| r.is_blocker && !r.repeated).count();
        let stale = chain.iter().filter(|r| r.needs_reintegration).count();
        let header = match (blockers, stale) {
            (0, 0) => "dependency chain (all graduated):".to_string(),
            (0, s) => format!("dependency chain ({s} stale — reintegrate):"),
            (b, 0) => format!("dependency chain ({b} blocking):"),
            (b, s) => format!("dependency chain ({b} blocking, {s} stale):"),
        };
        lines.push(Line::from(Span::styled(
            header,
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for (i, r) in chain.iter().enumerate() {
            let indent = "  ".repeat(r.depth);
            let elbow = if r.depth > 0 { "└ " } else { "" };
            // `repeated` (shown higher in the chain already) takes priority
            // over everything else, same as a plain blocker/stale link would.
            // `carried` is checked next, ahead of blocker/stale: a cycle's
            // carrier contains this row's tip, so it gates nothing despite
            // being ungraduated — see [`ChainRow::carried`].
            let (marker, marker_style) =
                match (r.repeated, r.carried, r.is_blocker, r.needs_reintegration) {
                    (true, _, _, true) => {
                        ("↑ shown above, ⚠ stale", Style::default().fg(Color::Yellow))
                    }
                    (true, _, _, false) => ("↑ shown above", Style::default().fg(Color::DarkGray)),
                    (false, true, _, _) => ("↻ carried", Style::default().fg(Color::Cyan)),
                    (false, false, true, true) => {
                        ("⛔ blocker, ⚠ stale", Style::default().fg(Color::Red))
                    }
                    (false, false, true, false) => ("⛔ blocker", Style::default().fg(Color::Red)),
                    (false, false, false, true) => {
                        ("⚠ reintegrate", Style::default().fg(Color::Yellow))
                    }
                    (false, false, false, false) => ("✓ ok", Style::default().fg(Color::Green)),
                };
            lines.push(Line::from(vec![
                Span::raw(format!("{}{indent}{elbow}#{}  ", cursor_mark(i), r.number)),
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
        for (i, r) in dependents.iter().enumerate() {
            lines.push(Line::from(vec![
                Span::raw(format!("{}#{}  ", cursor_mark(chain.len() + i), r.number)),
                Span::styled(r.branch.clone(), Style::default().fg(Color::Cyan)),
                Span::raw("  ["),
                Span::styled(r.state.label(), Style::default().fg(state_color(&r.state))),
                Span::raw("]"),
            ]));
        }
    }

    lines.push(Line::from(""));
    let hint = match (chain.is_empty() && dependents.is_empty(), trail.is_empty()) {
        (true, true) => "[q/esc] close",
        (true, false) => "[backspace] back   [esc] close",
        (false, true) => "[j/k] move   [enter] open   [esc] close",
        (false, false) => "[j/k] move   [enter] open   [backspace] back   [esc] close",
    };
    lines.push(Line::from(Span::styled(
        hint,
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
            stale_deps: Vec::new(),
            graduation_commit: None,
            cycle: None,
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
            tag_on_graduate: true,
            push_tag: true,
            dev_versions: true,
            roll_to_rolling_gates: Vec::new(),
            rolling_to_main_gates: Vec::new(),
            host_gates: Vec::new(),
            clean_protect: Vec::new(),
            pull_mode: Default::default(),
            lazygit_command: "lazygit".to_string(),
        }
    }

    /// A throwaway git repo on disk, with a Cargo.toml and a `rolling` branch
    /// split off from `main`, so `run_verify`'s real git calls (merge-state
    /// classification, the dev-marker commit) have something to act on. The
    /// `tests/` integration suite can't reach `run_verify` directly — it only
    /// drives the compiled binary — so this is the one way to exercise it.
    fn sandbox_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(repo)
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "t@t.com"]);
        git(&["config", "user.name", "tester"]);
        std::fs::write(
            repo.join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.0.1\"\nedition = \"2021\"\n",
        )
        .expect("write Cargo.toml");
        git(&["add", "Cargo.toml"]);
        git(&["commit", "-q", "-m", "add manifest"]);
        git(&["branch", "rolling"]);
        git(&["checkout", "-q", "-b", "roll/1-0101-late"]);
        std::fs::write(repo.join("work.txt"), "w\n").expect("write work.txt");
        git(&["add", "work.txt"]);
        git(&["commit", "-q", "-m", "roll work"]);
        dir
    }

    #[test]
    fn a_click_inside_the_small_panel_maximizes_it() {
        let rect = Rect {
            x: 50,
            y: 20,
            width: 30,
            height: 10,
        };
        assert!(click_maximizes(55, 22, rect));
        assert!(click_maximizes(50, 20, rect), "top-left corner is inside");
        assert!(
            click_maximizes(79, 29, rect),
            "bottom-right-most cell is inside"
        );
    }

    #[test]
    fn a_click_outside_the_panel_rect_does_not_maximize() {
        let rect = Rect {
            x: 50,
            y: 20,
            width: 30,
            height: 10,
        };
        assert!(!click_maximizes(0, 0, rect));
        assert!(!click_maximizes(80, 20, rect), "one past the right edge");
        assert!(!click_maximizes(50, 30, rect), "one past the bottom edge");
    }

    #[test]
    fn a_click_outside_the_maximized_rect_restores_it() {
        // Same rule, different rect: `render` passes the *expanded* rect once
        // maximized, so a click outside that larger area is what restores it
        // — `click_maximizes` does not need to know it was ever maximized.
        let expanded = Rect {
            x: 2,
            y: 2,
            width: 96,
            height: 36,
        };
        assert!(click_maximizes(50, 20, expanded));
        assert!(!click_maximizes(0, 0, expanded));
    }

    #[test]
    fn clicking_the_panel_maximizes_it_and_clicking_off_restores_it() {
        let cfg = config("main", "develop");
        let mut app = StatusApp {
            config: cfg,
            current_branch: "main".to_string(),
            bases: Vec::new(),
            rolls: Vec::new(),
            hotfixes: Vec::new(),
            show_deps: false,
            tracking: HashMap::new(),
            versions: HashMap::new(),
            version: None,
            table: TableState::default(),
            mode: Mode::Browsing,
            message: None,
            job: None,
            panel: Some(output::Panel::new("test")),
            panel_maximized: false,
            panel_rect: Some(Rect {
                x: 50,
                y: 20,
                width: 30,
                height: 10,
            }),
            pending_g: false,
            pending_push: None,
        };

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 55,
            row: 22,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert!(
            app.panel_maximized,
            "click inside the panel should maximize it"
        );

        // The rect `handle_mouse` tests against is whatever was last drawn —
        // standing in for `render` having drawn the expanded rect once
        // maximized.
        app.panel_rect = Some(Rect {
            x: 2,
            y: 2,
            width: 96,
            height: 36,
        });
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert!(!app.panel_maximized, "click outside should restore it");
    }

    #[test]
    fn mouse_clicks_are_a_no_op_with_no_panel_up() {
        let cfg = config("main", "develop");
        let mut app = StatusApp {
            config: cfg,
            current_branch: "main".to_string(),
            bases: Vec::new(),
            rolls: Vec::new(),
            hotfixes: Vec::new(),
            show_deps: false,
            tracking: HashMap::new(),
            versions: HashMap::new(),
            version: None,
            table: TableState::default(),
            mode: Mode::Browsing,
            message: None,
            job: None,
            panel: None,
            panel_maximized: false,
            panel_rect: None,
            pending_g: false,
            pending_push: None,
        };

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 5,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert!(!app.panel_maximized);
    }

    #[test]
    fn toggle_maximize_is_a_no_op_with_no_panel() {
        let cfg = config("main", "develop");
        let mut app = StatusApp {
            config: cfg,
            current_branch: "main".to_string(),
            bases: Vec::new(),
            rolls: Vec::new(),
            hotfixes: Vec::new(),
            show_deps: false,
            tracking: HashMap::new(),
            versions: HashMap::new(),
            version: None,
            table: TableState::default(),
            mode: Mode::Browsing,
            message: None,
            job: None,
            panel: None,
            panel_maximized: false,
            panel_rect: None,
            pending_g: false,
            pending_push: None,
        };
        app.toggle_maximize();
        assert!(!app.panel_maximized, "nothing to maximize");

        app.panel = Some(output::Panel::new("test"));
        app.toggle_maximize();
        assert!(app.panel_maximized);
        app.toggle_maximize();
        assert!(!app.panel_maximized);
    }

    #[test]
    fn run_verify_applies_the_dev_marker_like_cmd_verify_does() {
        // Regression test: `run_verify`'s doc comment claims a "line for line"
        // mirror of `cmd_verify`, but the dev-marker step was missing — a roll
        // started without one (predating the feature, or `--no-dev-version`)
        // never got marked from the TUI's `[v]`, only from `rf verify`.
        let dir = sandbox_repo();
        let mut cfg = config("main", "rolling");
        cfg.repo_root = dir.path().to_path_buf();

        let lines = run_verify(&cfg).expect("verify should pass with no gates configured");
        assert!(
            lines
                .iter()
                .any(|l| l.contains("version marked 0.0.1-roll1")),
            "{lines:?}"
        );

        let cargo_toml =
            std::fs::read_to_string(dir.path().join("Cargo.toml")).expect("read Cargo.toml");
        assert!(
            cargo_toml.contains("0.0.1-roll1"),
            "marker not written: {cargo_toml}"
        );
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
        assert_eq!(row_at(0, 2, 3, 0), Some(RowKind::Base(0)));
        assert_eq!(row_at(1, 2, 3, 0), Some(RowKind::Base(1)));
        assert_eq!(row_at(2, 2, 3, 0), Some(RowKind::Roll(0)));
        assert_eq!(row_at(4, 2, 3, 0), Some(RowKind::Roll(2)));
        // Past the last row.
        assert_eq!(row_at(5, 2, 3, 0), None);
        // No bases → rolls start at 0; no rolls → only bases.
        assert_eq!(row_at(0, 0, 1, 0), Some(RowKind::Roll(0)));
        assert_eq!(row_at(1, 1, 0, 0), None);
        assert_eq!(row_at(0, 0, 0, 0), None);
    }

    #[test]
    fn initial_selection_prefers_the_current_branch() {
        let cfg = config("main", "develop");
        let bases = base_branches(&cfg, "develop", |_| true);
        let mut rolls = vec![roll_n(1, RollState::Active), roll_n(2, RollState::Active)];

        // Current branch is the rolling base → its own row.
        assert_eq!(initial_selection(&bases, &rolls, &[]), Some(1));

        // Current branch is a roll → offset past the bases.
        let off_bases = base_branches(&cfg, "roll/2-0101-x", |_| true);
        rolls[1].is_current = true;
        assert_eq!(initial_selection(&off_bases, &rolls, &[]), Some(3));

        // Nothing current → first row.
        rolls[1].is_current = false;
        assert_eq!(initial_selection(&off_bases, &rolls, &[]), Some(0));

        // Rolls but no bases still selects the first roll.
        assert_eq!(initial_selection(&[], &rolls, &[]), Some(0));

        // Nothing at all → no selection.
        assert_eq!(initial_selection(&[], &[], &[]), None);
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
            stale_deps: Vec::new(),
            graduation_commit: None,
            cycle: None,
        }
    }

    #[test]
    fn graduate_valid_only_for_active_or_diverged() {
        assert!(can_graduate(&RollState::Active));
        assert!(can_graduate(&RollState::Diverged));
        assert!(!can_graduate(&RollState::Graduated));
        assert!(!can_graduate(&RollState::Promoted));
        // Blocked is allowed: the chain graduates what it waits on first.
        assert!(can_graduate(&RollState::Blocked));
    }

    #[test]
    fn landed_hotfixes_make_prune_and_tidy_worth_offering() {
        let hotfix = |number: u32, state: HotfixState, location: BranchLocation| HotfixInfo {
            branch: format!("hotfix/{number}-0720-x"),
            number,
            state,
            location,
            is_current: false,
        };
        let hotfixes = vec![
            hotfix(1, HotfixState::Landed, BranchLocation::Both),
            hotfix(2, HotfixState::Landed, BranchLocation::Remote),
            hotfix(3, HotfixState::Open, BranchLocation::Local),
        ];
        // Prune takes both copies of every landed hotfix; tidy only local
        // copies, and an open hotfix is no more tidy's default than an active
        // roll is.
        assert_eq!(prunable_hotfix_count(&hotfixes), 2);
        assert_eq!(tidyable_hotfix_count(&hotfixes), 1);

        // No roll qualifies, yet the hotfixes alone make both keys valid.
        let rolls = vec![roll_n(1, RollState::Active)];
        for action in [Action::Prune, Action::Tidy] {
            assert!(validate_action(action, None, &rolls, "main", "roll/").is_err());
            assert!(
                validate_with_hotfixes(action, None, &rolls, &hotfixes, "main", "roll/").is_ok(),
                "{action:?}"
            );
            assert!(
                validate_with_hotfixes(action, None, &rolls, &[], "main", "roll/").is_err(),
                "{action:?}"
            );
        }
    }

    #[test]
    fn deletion_prompts_count_hotfixes_only_when_there_are_some() {
        assert_eq!(
            deletion_count(2, "promoted roll", 0),
            "2 promoted roll branches"
        );
        assert_eq!(
            deletion_count(1, "promoted roll", 2),
            "1 promoted roll branch and 2 landed hotfixes"
        );
        assert_eq!(deletion_count(0, "promoted roll", 1), "1 landed hotfix");
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

    /// A `git::LocalBranch` with the upstream fields `track_state` reads.
    fn tracked(name: &str, track: &str) -> git::LocalBranch {
        git::LocalBranch {
            name: name.to_string(),
            upstream: format!("origin/{name}"),
            remote_name: "origin".to_string(),
            track: track.to_string(),
            worktree: String::new(),
        }
    }

    /// A local branch that was never pushed: no upstream at all.
    fn untracked(name: &str) -> git::LocalBranch {
        git::LocalBranch {
            name: name.to_string(),
            upstream: String::new(),
            remote_name: String::new(),
            track: String::new(),
            worktree: String::new(),
        }
    }

    fn push_all_rows(names: &[&str]) -> Vec<(String, BranchLocation)> {
        names
            .iter()
            .map(|n| (n.to_string(), BranchLocation::Both))
            .collect()
    }

    #[test]
    fn push_all_takes_only_the_branches_a_plain_push_would_carry() {
        let mut tracking = HashMap::new();
        tracking.insert("main".to_string(), tracked("main", ""));
        tracking.insert("rolling".to_string(), tracked("rolling", "ahead 2"));
        tracking.insert("roll/1-x".to_string(), untracked("roll/1-x"));
        let rows = push_all_rows(&["main", "rolling", "roll/1-x"]);

        let plan = plan_push_all(&rows, "rolling", &tracking);

        // Ahead and never-pushed are both fast-forwards or creations; in-sync is
        // not mentioned at all, because there is nothing to say about it.
        let pushed: Vec<_> = plan
            .items
            .iter()
            .map(|i| (i.target.branch.as_str(), i.creates))
            .collect();
        assert_eq!(pushed, vec![("rolling", false), ("roll/1-x", true)]);
        assert!(plan.skipped.is_empty(), "{:?}", plan.skipped);
    }

    #[test]
    fn push_all_never_queues_a_branch_that_would_need_a_force() {
        let mut tracking = HashMap::new();
        tracking.insert("behind".to_string(), tracked("behind", "behind 3"));
        tracking.insert(
            "diverged".to_string(),
            tracked("diverged", "ahead 1, behind 2"),
        );
        let rows = push_all_rows(&["behind", "diverged"]);

        let plan = plan_push_all(&rows, "main", &tracking);

        // The whole safety argument for a bulk push key: it can only ever
        // fast-forward. Forcing stays a per-branch decision behind its own y/N.
        assert!(plan.items.is_empty(), "{:?}", plan.items);
        let reasons: Vec<_> = plan
            .skipped
            .iter()
            .map(|s| (s.branch.as_str(), s.reason.as_str()))
            .collect();
        assert_eq!(
            reasons,
            vec![
                ("behind", "behind by 3 — [p] to pull, then [P] to force"),
                ("diverged", "diverged ↑1↓2 — [P] on the row to force"),
            ]
        );
    }

    #[test]
    fn push_all_does_not_resurrect_a_deleted_upstream() {
        // `rf prune` and `rf clean --with-remote` delete branches on origin on
        // purpose. A bulk push that re-created them would quietly undo that, so
        // `gone` is reported and left for an explicit `[P]`.
        let mut tracking = HashMap::new();
        tracking.insert("roll/1-x".to_string(), tracked("roll/1-x", "gone"));
        let plan = plan_push_all(&push_all_rows(&["roll/1-x"]), "main", &tracking);

        assert!(plan.items.is_empty());
        assert_eq!(plan.skipped.len(), 1);
        assert!(
            plan.skipped[0].reason.starts_with("upstream deleted"),
            "{:?}",
            plan.skipped[0]
        );
    }

    #[test]
    fn push_all_ignores_branches_with_nothing_local_to_push() {
        let mut tracking = HashMap::new();
        tracking.insert("roll/1-x".to_string(), tracked("roll/1-x", "ahead 1"));
        // Remote-only: no local copy, so nothing to push even though the
        // tracking map happens to carry an entry for the name.
        let rows = vec![
            ("roll/1-x".to_string(), BranchLocation::Remote),
            // Local but absent from the tracking batch — the state the sync
            // column renders as `—`. Unknown is not a reason to push.
            ("roll/2-y".to_string(), BranchLocation::Local),
        ];

        let plan = plan_push_all(&rows, "main", &tracking);

        assert!(plan.items.is_empty(), "{:?}", plan.items);
        assert!(plan.skipped.is_empty(), "{:?}", plan.skipped);
    }

    #[test]
    fn only_a_second_p_widens_a_push_and_nothing_cancels_one() {
        assert_eq!(push_chord_key(KeyCode::Char('P')), PushChord::All);
        // Lowercase `p` is pull, not push — it must not complete the chord.
        // `Esc` resolves to the single push rather than cancelling: the first
        // `P` was already a decision to push the selected branch.
        for key in [
            KeyCode::Char('p'),
            KeyCode::Esc,
            KeyCode::Char('q'),
            KeyCode::Enter,
            KeyCode::Down,
        ] {
            assert_eq!(push_chord_key(key), PushChord::Single, "{key:?}");
        }
    }

    #[test]
    fn the_push_all_modal_lists_both_halves_of_the_plan() {
        let mut tracking = HashMap::new();
        tracking.insert("rolling".to_string(), tracked("rolling", "ahead 2"));
        tracking.insert("roll/1-x".to_string(), untracked("roll/1-x"));
        tracking.insert("roll/2-y".to_string(), tracked("roll/2-y", "gone"));
        let plan = plan_push_all(
            &push_all_rows(&["rolling", "roll/1-x", "roll/2-y"]),
            "main",
            &tracking,
        );

        let out = draw(|f, area| render_push_all_modal(f, area, &plan));

        assert!(out.contains("Push 2 branches to their remotes?"), "{out}");
        assert!(out.contains("rolling"), "{out}");
        assert!(out.contains("↑2"), "{out}");
        assert!(out.contains("new on the remote"), "{out}");
        // The skipped half is the reason this is a list and not a count: the
        // branch the user cared about may be in it.
        assert!(out.contains("skipped roll/2-y"), "{out}");
        assert!(out.contains("[y] confirm"), "{out}");
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
    fn filter_history_ranks_the_best_match_first_and_respects_order_on_ties() {
        let history = vec![
            "git status".to_string(),
            "git push".to_string(),
            "ls -la".to_string(),
        ];
        let matches = filter_history(&history, "git");
        assert_eq!(matches, vec![0, 1]);
        // An empty query is everything, in history order (most-recent-first,
        // since that is the order the caller stores it in).
        assert_eq!(filter_history(&history, ""), vec![0, 1, 2]);
        assert!(filter_history(&history, "zzzz").is_empty());
    }

    #[test]
    fn typing_in_the_command_prompt_fills_the_buffer() {
        let mut query = String::new();
        let mut input = String::new();
        let mut cursor = 0;
        let history: Vec<String> = Vec::new();
        for c in "git st".chars() {
            assert_eq!(
                handle_command_key(
                    &mut query,
                    &mut input,
                    &mut cursor,
                    &history,
                    KeyCode::Char(c),
                    false
                ),
                CommandOutcome::Continue
            );
        }
        assert_eq!(query, "git st");
        // Nothing has been picked from history, so `input` (what `enter` would
        // run) tracks the typed text exactly.
        assert_eq!(input, "git st");
    }

    #[test]
    fn enter_on_a_blank_prompt_does_nothing() {
        let mut query = String::new();
        let mut input = String::new();
        let mut cursor = 0;
        let history: Vec<String> = Vec::new();
        assert_eq!(
            handle_command_key(
                &mut query,
                &mut input,
                &mut cursor,
                &history,
                KeyCode::Enter,
                false
            ),
            CommandOutcome::Continue
        );
    }

    #[test]
    fn enter_on_a_typed_command_runs_it() {
        let mut query = "echo hi".to_string();
        let mut input = "echo hi".to_string();
        let mut cursor = 0;
        let history: Vec<String> = Vec::new();
        assert_eq!(
            handle_command_key(
                &mut query,
                &mut input,
                &mut cursor,
                &history,
                KeyCode::Enter,
                false
            ),
            CommandOutcome::Run
        );
    }

    #[test]
    fn alt_enter_runs_in_the_shell_instead() {
        let mut query = "vim".to_string();
        let mut input = "vim".to_string();
        let mut cursor = 0;
        let history: Vec<String> = Vec::new();
        assert_eq!(
            handle_command_key(
                &mut query,
                &mut input,
                &mut cursor,
                &history,
                KeyCode::Enter,
                true
            ),
            CommandOutcome::RunInShell
        );
    }

    #[test]
    fn esc_closes_the_prompt() {
        let mut query = "anything".to_string();
        let mut input = "anything".to_string();
        let mut cursor = 0;
        let history: Vec<String> = Vec::new();
        assert_eq!(
            handle_command_key(
                &mut query,
                &mut input,
                &mut cursor,
                &history,
                KeyCode::Esc,
                false
            ),
            CommandOutcome::Close
        );
    }

    #[test]
    fn up_and_down_fill_the_input_from_the_highlighted_history_entry() {
        let history = vec!["git push".to_string(), "git status".to_string()];
        let mut query = "git".to_string();
        let mut input = "git".to_string();
        let mut cursor = 0;

        // The first press lands on the top suggestion.
        handle_command_key(
            &mut query,
            &mut input,
            &mut cursor,
            &history,
            KeyCode::Down,
            false,
        );
        assert_eq!(cursor, 0);
        assert_eq!(input, "git push");
        // The query itself — what's filtered against — is untouched by the pick.
        assert_eq!(query, "git");

        handle_command_key(
            &mut query,
            &mut input,
            &mut cursor,
            &history,
            KeyCode::Down,
            false,
        );
        assert_eq!(cursor, 1);
        assert_eq!(input, "git status");

        handle_command_key(
            &mut query,
            &mut input,
            &mut cursor,
            &history,
            KeyCode::Up,
            false,
        );
        assert_eq!(cursor, 0);
        assert_eq!(input, "git push");

        // Cannot walk off either end.
        handle_command_key(
            &mut query,
            &mut input,
            &mut cursor,
            &history,
            KeyCode::Up,
            false,
        );
        assert_eq!(cursor, 0);
        assert_eq!(input, "git push");
    }

    #[test]
    fn cycling_through_more_than_two_history_entries_does_not_collapse_the_list() {
        // The bug this guards against: filtering by `input` (the just-picked
        // command) instead of `query` (what was actually typed) would shrink
        // the candidate list to one entry after the first pick, so a second
        // `Down` could never reach a third match.
        let history = vec![
            "git push".to_string(),
            "git status".to_string(),
            "git log".to_string(),
        ];
        let mut query = "git".to_string();
        let mut input = "git".to_string();
        let mut cursor = 0;

        for expected in ["git push", "git status", "git log"] {
            handle_command_key(
                &mut query,
                &mut input,
                &mut cursor,
                &history,
                KeyCode::Down,
                false,
            );
            assert_eq!(input, expected);
        }
        assert_eq!(query, "git", "query must stay exactly what was typed");
    }

    #[test]
    fn typing_after_a_history_pick_resets_the_highlight_and_the_input() {
        let history = vec!["git push".to_string(), "git status".to_string()];
        let mut query = "git".to_string();
        let mut input = "git".to_string();
        let mut cursor = 0;
        handle_command_key(
            &mut query,
            &mut input,
            &mut cursor,
            &history,
            KeyCode::Down,
            false,
        );
        assert_eq!(cursor, 0);
        assert_eq!(input, "git push");

        handle_command_key(
            &mut query,
            &mut input,
            &mut cursor,
            &history,
            KeyCode::Char('x'),
            false,
        );
        assert_eq!(cursor, 0);
        assert_eq!(query, "gitx");
        // Typing overrides whatever was picked — it no longer reflects the
        // history entry that was highlighted a moment ago.
        assert_eq!(input, "gitx");
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
    fn update_target_is_the_selected_active_roll() {
        let rolls = vec![roll_n(1, RollState::Active), roll_n(2, RollState::Blocked)];
        assert_eq!(
            update_target_for(Some(&rolls[0]), &rolls),
            Ok(Some("roll/1-0101-x".to_string()))
        );
        assert_eq!(
            update_target_for(Some(&rolls[1]), &rolls),
            Ok(Some("roll/2-0101-x".to_string()))
        );
    }

    #[test]
    fn update_target_is_every_active_roll_without_one_selected() {
        // A base-branch row resolves to `None` the same way an empty selection
        // does, which is how `[u]` with nothing selected updates every active
        // local roll.
        let rolls = vec![roll_n(1, RollState::Active)];
        assert_eq!(update_target_for(None, &rolls), Ok(None));
    }

    #[test]
    fn update_target_refuses_a_roll_that_cannot_be_updated() {
        let graduated = vec![roll(RollState::Graduated, BranchLocation::Both)];
        let err = update_target_for(Some(&graduated[0]), &graduated)
            .expect_err("graduated rolls should be refused");
        assert!(
            err.contains("roll/1-0101-x"),
            "the message should name the roll: {err}"
        );

        let remote_only = vec![roll(RollState::Active, BranchLocation::Remote)];
        let err = update_target_for(Some(&remote_only[0]), &remote_only)
            .expect_err("remote-only rolls should be refused");
        assert!(
            err.contains("roll/1-0101-x"),
            "the message should name the roll: {err}"
        );
    }

    #[test]
    fn update_target_refusal_does_not_widen_to_every_active_roll() {
        // The dangerous failure mode: pressing [u] on a graduated roll must not
        // fall back to updating every active roll, which is far more than was
        // asked.
        let rolls = vec![
            roll_n(1, RollState::Graduated),
            roll_n(2, RollState::Active),
        ];
        assert!(update_target_for(Some(&rolls[0]), &rolls).is_err());
        assert!(validate_action(Action::Update, Some(&rolls[0]), &rolls, "main", "roll/").is_err());
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
    fn dep_rows_flag_blockers_and_staleness_independently() {
        let all = vec![
            roll_n(1, RollState::Graduated),
            roll_n(2, RollState::Active),
            roll_n(3, RollState::Diverged),
            roll_n(4, RollState::Promoted),
        ];
        let mut selected = roll_n(5, RollState::Blocked);
        selected.deps = vec![1, 2, 3, 4];
        // Ancestry-based, independent of state: 2 (still active) and 3
        // (diverged) have both moved since `selected` integrated them; 1 and 4
        // have not.
        selected.stale_deps = vec![2, 3];

        let rows = dep_rows(&selected, &all);
        assert_eq!(rows.len(), 4);
        // Graduated / promoted deps are satisfied — not blockers, not stale.
        let r1 = rows.iter().find(|r| r.number == 1).unwrap();
        assert!(!r1.is_blocker && !r1.needs_reintegration);
        let r4 = rows.iter().find(|r| r.number == 4).unwrap();
        assert!(!r4.is_blocker && !r4.needs_reintegration);
        // An active dep that has ALSO moved since integration is both a
        // blocker (it hasn't graduated yet) and stale (reintegrating would
        // pick up more) — exactly the case that matters before merging a
        // batch of dependents against a still-moving dependency.
        let r2 = rows.iter().find(|r| r.number == 2).unwrap();
        assert!(r2.is_blocker && r2.needs_reintegration);
        // A diverged dep already graduated — it is not a blocker, but it has
        // moved on since `selected` integrated it and needs reintegrating.
        let r3 = rows.iter().find(|r| r.number == 3).unwrap();
        assert!(!r3.is_blocker && r3.needs_reintegration);

        let blockers: Vec<u32> = rows
            .iter()
            .filter(|r| r.is_blocker)
            .map(|r| r.number)
            .collect();
        assert_eq!(blockers, vec![2]);

        let stale: Vec<u32> = rows
            .iter()
            .filter(|r| r.needs_reintegration)
            .map(|r| r.number)
            .collect();
        assert_eq!(stale, vec![2, 3]);
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
        assert!(!rows[0].needs_reintegration);
    }

    #[test]
    fn dependent_rows_flag_reintegration_from_the_dependents_own_staleness() {
        // `needs_reintegration` here reads the *dependent's* `stale_deps`, not
        // `target`'s state — a dependent is stale as soon as the target moves
        // past what it integrated, even while the target is still `Active` and
        // therefore still gating it. This is roll/27's actual situation with
        // its roll/26 dependency: roll/26 kept gaining commits without ever
        // graduating, so roll/27 is both blocked on it and behind it.
        let mut r27 = roll_n(27, RollState::Active);
        r27.deps = vec![26];
        r27.stale_deps = vec![26];
        let mut target = roll_n(26, RollState::Active);
        target.dependents = vec![27];
        let all = vec![r27, target.clone()];

        let rows = dependent_rows(&target, &all);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].number, 27);
        // Still active, so it still gates roll/27's graduation...
        assert!(rows[0].is_blocker);
        // ...but roll/27's copy is also stale, so it needs reintegrating too.
        assert!(rows[0].needs_reintegration);
    }

    #[test]
    fn dependent_rows_do_not_flag_reintegration_when_dependent_is_current() {
        // A dependent that already has the target's latest tip (stale_deps
        // does not name it) is not told to reintegrate, even if the target is
        // still active and gating it.
        let mut r27 = roll_n(27, RollState::Active);
        r27.deps = vec![26];
        let mut target = roll_n(26, RollState::Active);
        target.dependents = vec![27];
        let all = vec![r27, target.clone()];

        let rows = dependent_rows(&target, &all);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].is_blocker);
        assert!(!rows[0].needs_reintegration);
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

    /// `12 → 9 → 8 → 7`: the chain the user asked to see, built the way
    /// `list_rolls` would record it (each roll's `deps` are its direct
    /// integrations only).
    fn linear_chain() -> Vec<RollInfo> {
        let mut r7 = roll_n(7, RollState::Graduated);
        let mut r8 = roll_n(8, RollState::Graduated);
        let mut r9 = roll_n(9, RollState::Active);
        let mut r12 = roll_n(12, RollState::Blocked);
        r8.deps = vec![7];
        r9.deps = vec![8];
        r12.deps = vec![9];
        r7.dependents = vec![8];
        r8.dependents = vec![9];
        r9.dependents = vec![12];
        vec![r7, r8, r9, r12]
    }

    #[test]
    fn dep_chain_follows_every_link_to_the_bottom() {
        let all = linear_chain();
        let rows = dep_chain(&all[3], &all);
        let shape: Vec<(usize, u32, bool)> = rows
            .iter()
            .map(|r| (r.depth, r.number, r.is_blocker))
            .collect();
        // Depth grows one per link; only 9 is still ungraduated.
        assert_eq!(shape, vec![(0, 9, true), (1, 8, false), (2, 7, false)]);
        assert!(rows.iter().all(|r| !r.repeated));
        // The first level is exactly `dep_rows`, by construction.
        let direct: Vec<u32> = dep_rows(&all[3], &all).iter().map(|r| r.number).collect();
        assert_eq!(direct, vec![9]);
    }

    #[test]
    fn dep_chain_lists_a_diamond_once_and_never_loops() {
        // 4 depends on 2 and 3; both depend on 1 (a diamond). 1 also claims to
        // depend on 4 (a cycle, which real history cannot produce but a scan
        // of hand-written merges might).
        let mut r1 = roll_n(1, RollState::Active);
        let mut r2 = roll_n(2, RollState::Active);
        let mut r3 = roll_n(3, RollState::Active);
        let mut r4 = roll_n(4, RollState::Blocked);
        r1.deps = vec![4];
        r2.deps = vec![1];
        r3.deps = vec![1];
        r4.deps = vec![2, 3];
        let all = vec![r1, r2, r3, r4];

        let rows = dep_chain(&all[3], &all);
        let shape: Vec<(usize, u32, bool)> = rows
            .iter()
            .map(|r| (r.depth, r.number, r.repeated))
            .collect();
        assert_eq!(
            shape,
            vec![
                (0, 2, false),
                (1, 1, false),
                // 1's dep on 4 is the roll we started from: repeated, not
                // descended — so the walk terminates.
                (2, 4, true),
                (0, 3, false),
                // Second arm of the diamond: 1 is shown again but marked.
                (1, 1, true),
            ]
        );
        // Blockers are counted once, however many arms reach them.
        assert_eq!(
            rows.iter().filter(|r| r.is_blocker && !r.repeated).count(),
            3
        );
    }

    #[test]
    fn dep_chain_marks_the_link_the_parent_finds_stale() {
        let mut all = linear_chain();
        // 9 integrated 8 and 8 has since moved; 12's own view of 9 is fine.
        all[2].stale_deps = vec![8];
        let rows = dep_chain(&all[3], &all);
        let flags: Vec<(u32, bool)> = rows
            .iter()
            .map(|r| (r.number, r.needs_reintegration))
            .collect();
        assert_eq!(flags, vec![(9, false), (8, true), (7, false)]);
    }

    /// 12 → 9 → 8 → 7, with 3 depending on 12: the shape this feature exists for.
    fn drill_fixture() -> Vec<RollInfo> {
        let mut rolls = vec![
            roll_n(7, RollState::Graduated),
            roll_n(8, RollState::Active),
            roll_n(9, RollState::Blocked),
            roll_n(12, RollState::Blocked),
            roll_n(3, RollState::Blocked),
        ];
        rolls[0].dependents = vec![8];
        rolls[1].deps = vec![7];
        rolls[1].dependents = vec![9];
        rolls[2].deps = vec![8];
        rolls[2].dependents = vec![12];
        rolls[3].deps = vec![9];
        rolls[3].dependents = vec![3];
        rolls[4].deps = vec![12];
        rolls
    }

    #[test]
    fn the_detail_targets_are_the_chain_then_the_dependents_in_pane_order() {
        let all = drill_fixture();
        let twelve = all.iter().find(|r| r.number == 12).unwrap();
        // What the cursor walks is exactly what the pane lists, in order — so
        // the highlighted row and the opened row can never disagree.
        assert_eq!(detail_targets(twelve, &all), vec![9, 8, 7, 3]);
        let seven = all.iter().find(|r| r.number == 7).unwrap();
        assert_eq!(detail_targets(seven, &all), vec![8]);
    }

    #[test]
    fn detail_keys_walk_open_and_retrace() {
        let mut cursor = 0;
        // Movement clamps to the list.
        assert_eq!(
            detail_key(KeyCode::Char('j'), &mut cursor, 3, false),
            DetailOutcome::Continue
        );
        assert_eq!(
            detail_key(KeyCode::Down, &mut cursor, 3, false),
            DetailOutcome::Continue
        );
        assert_eq!(
            detail_key(KeyCode::Char('j'), &mut cursor, 3, false),
            DetailOutcome::Continue
        );
        assert_eq!(cursor, 2, "walked past the end");
        assert_eq!(
            detail_key(KeyCode::Char('k'), &mut cursor, 3, false),
            DetailOutcome::Continue
        );
        assert_eq!(cursor, 1);

        // Enter opens the row under the cursor; with nothing to open it does nothing.
        assert_eq!(
            detail_key(KeyCode::Enter, &mut cursor, 3, false),
            DetailOutcome::Open(1)
        );
        assert_eq!(
            detail_key(KeyCode::Char('l'), &mut cursor, 3, false),
            DetailOutcome::Open(1)
        );
        assert_eq!(
            detail_key(KeyCode::Enter, &mut cursor, 0, false),
            DetailOutcome::Continue
        );

        // Backspace retraces when there is a trail and closes when there is not,
        // so the key never dead-ends. Esc always closes outright.
        assert_eq!(
            detail_key(KeyCode::Backspace, &mut cursor, 3, true),
            DetailOutcome::Back
        );
        assert_eq!(
            detail_key(KeyCode::Char('h'), &mut cursor, 3, true),
            DetailOutcome::Back
        );
        assert_eq!(
            detail_key(KeyCode::Backspace, &mut cursor, 3, false),
            DetailOutcome::Close
        );
        assert_eq!(
            detail_key(KeyCode::Esc, &mut cursor, 3, true),
            DetailOutcome::Close
        );
        assert_eq!(
            detail_key(KeyCode::Char('q'), &mut cursor, 3, true),
            DetailOutcome::Close
        );
    }

    #[test]
    fn a_drilled_pane_shows_where_it_came_from_and_marks_the_cursor() {
        let all = drill_fixture();
        let nine = all.iter().find(|r| r.number == 9).unwrap();
        let out = draw(|f, area| render_detail(f, area, nine, None, &all, 1, &[12]));
        assert!(out.contains("#12 → #9"), "no breadcrumb:\n{out}");
        // Cursor on the second target (#7), not the first.
        assert!(out.contains("▶ "), "{out}");
        let marked = out.lines().find(|l| l.contains("▶ ")).unwrap();
        assert!(marked.contains("#7"), "cursor on the wrong row: {marked}");
        assert!(out.contains("[backspace] back"), "{out}");

        // A top-level pane has no breadcrumb and no back hint.
        let top = draw(|f, area| render_detail(f, area, nine, None, &all, 0, &[]));
        assert!(!top.contains("→ #9"), "{top}");
        assert!(!top.contains("[backspace]"), "{top}");
        assert!(top.contains("[enter] open"), "{top}");
    }

    #[test]
    fn the_detail_view_draws_the_chain_with_its_markers() {
        let mut all = linear_chain();
        all[3].stale_deps = vec![9];
        let out = draw(|f, area| render_detail(f, area, &all[3], None, &all, 0, &[]));
        assert!(
            out.contains("dependency chain (1 blocking, 1 stale)"),
            "{out}"
        );
        assert!(out.contains("#9"), "{out}");
        // The blocker and the staleness are both on the one link.
        assert!(out.contains("blocker, ⚠ stale"), "{out}");
        assert!(out.contains("└ #8"), "{out}");
        assert!(out.contains("└ #7"), "{out}");
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

    fn delete_prompt_of(roll: &RollInfo) -> Result<DeletePrompt, String> {
        delete_prompt(&roll.branch, &roll.location, roll.is_current)
    }

    #[test]
    fn delete_prompt_shape_follows_location() {
        let single = |loc| delete_prompt_of(&roll(RollState::Active, loc));
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
                delete_prompt_of(&roll(state.clone(), BranchLocation::Local)),
                Ok(DeletePrompt::Single(DeleteScope::Local)),
                "{state:?} should still be deletable"
            );
        }
    }

    #[test]
    fn delete_prompt_never_offers_the_checked_out_local_copy() {
        let mut local = roll(RollState::Active, BranchLocation::Local);
        local.is_current = true;
        let err = delete_prompt_of(&local).expect_err("checked-out local-only roll");
        assert!(err.contains("checked out"), "should explain itself: {err}");

        // The origin copy of a checked-out roll is still fair game.
        let mut both = roll(RollState::Active, BranchLocation::Both);
        both.is_current = true;
        assert_eq!(
            delete_prompt_of(&both),
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

    // ── graph column ────────────────────────────────────────────────────

    #[test]
    fn graph_glyphs_place_an_open_marker_on_the_rolling_lane_while_active() {
        assert_eq!(graph_glyphs(&RollState::Active), ('│', '○'));
        assert_eq!(graph_glyphs(&RollState::Blocked), ('│', '◌'));
    }

    #[test]
    fn graph_glyphs_fill_the_rolling_lane_once_graduated() {
        assert_eq!(graph_glyphs(&RollState::Graduated), ('│', '●'));
    }

    #[test]
    fn graph_glyphs_mark_divergence_and_reversion_on_the_rolling_lane() {
        assert_eq!(graph_glyphs(&RollState::Diverged), ('│', '◐'));
        assert_eq!(graph_glyphs(&RollState::Reverted), ('│', '↺'));
    }

    #[test]
    fn graph_glyphs_fill_both_lanes_once_promoted() {
        assert_eq!(graph_glyphs(&RollState::Promoted), ('●', '●'));
    }

    #[test]
    fn graph_glyphs_mark_a_demoted_main_lane_without_losing_the_rolling_fill() {
        // Demoted means stable's copy was reverted; the roll's content is
        // still on rolling, so only the main lane changes from `Promoted`.
        assert_eq!(graph_glyphs(&RollState::Demoted), ('◐', '●'));
    }

    #[test]
    fn hotfix_graph_glyphs_distinguish_open_from_landed() {
        assert_eq!(hotfix_graph_glyphs(HotfixState::Open), ('│', '○'));
        assert_eq!(hotfix_graph_glyphs(HotfixState::Landed), ('●', '●'));
    }

    #[test]
    fn base_graph_glyphs_put_stable_on_main_and_rolling_behind_a_passthrough() {
        assert_eq!(base_graph_glyphs(BaseRole::Stable), ('●', ' '));
        assert_eq!(base_graph_glyphs(BaseRole::Rolling), ('│', '●'));
    }

    #[test]
    fn the_graph_column_sits_left_of_the_roll_number_for_every_row_kind() {
        let bases = vec![
            BaseBranch {
                role: BaseRole::Stable,
                branch: "main".to_string(),
                location: BranchLocation::Local,
                is_current: false,
            },
            BaseBranch {
                role: BaseRole::Rolling,
                branch: "rolling".to_string(),
                location: BranchLocation::Local,
                is_current: false,
            },
        ];
        let mut app = StatusApp {
            config: config("main", "rolling"),
            current_branch: "main".to_string(),
            bases,
            rolls: vec![roll_n(1, RollState::Active), roll_n(2, RollState::Promoted)],
            hotfixes: Vec::new(),
            show_deps: false,
            tracking: HashMap::new(),
            versions: HashMap::new(),
            version: None,
            table: TableState::default(),
            mode: Mode::Browsing,
            message: None,
            job: None,
            panel: None,
            pending_g: false,
            pending_push: None,
            command_history: Vec::new(),
        };
        let out = draw(|f, area| app.render_table(f, area));

        let stable = out
            .lines()
            .find(|l| l.contains("main"))
            .expect("stable row");
        assert!(
            stable.contains("● "),
            "stable row missing its glyph: {stable}"
        );

        let rolling = out
            .lines()
            .find(|l| l.contains("rolling") && !l.contains("roll/"))
            .expect("rolling row");
        assert!(
            rolling.contains("│●"),
            "rolling row missing its glyph: {rolling}"
        );

        let active = out
            .lines()
            .find(|l| l.contains("roll/1-0101-x"))
            .expect("active roll row");
        assert!(
            active.contains("│○"),
            "active roll missing its glyph: {active}"
        );

        let promoted = out
            .lines()
            .find(|l| l.contains("roll/2-0101-x"))
            .expect("promoted roll row");
        assert!(
            promoted.contains("●●"),
            "promoted roll missing its glyph: {promoted}"
        );
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
            tag_on_graduate: true,
            push_tag: true,
            dev_versions: true,
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
    fn the_search_finds_pp_and_offers_to_run_it() {
        // The reason roll/8 had to be integrated here rather than merged at
        // graduation: `PP` only exists as a key if it is a row in BINDINGS.
        let hits = filter_bindings("push all");
        let top = &BINDINGS[hits[0]];
        assert_eq!(top.keys, "PP", "got {} instead", top.keys);
        // Two keystrokes, so the chord completes when `?` replays them.
        assert_eq!(
            top.replay,
            &[KeyCode::Char('P'), KeyCode::Char('P')],
            "PP must replay as a chord"
        );
        // And it stays off the slim status bar.
        assert!(top.hint.is_none());
    }

    #[test]
    fn the_hotfix_keys_are_in_the_keymap_and_found_by_the_search() {
        let hits: Vec<&str> = filter_bindings("hotfix")
            .into_iter()
            .map(|i| BINDINGS[i].keys)
            .collect();
        assert!(hits.contains(&"h"), "{hits:?}");
        assert!(hits.contains(&"H"), "{hits:?}");
        // Neither belongs on the slim bar.
        for b in BINDINGS.iter().filter(|b| b.keys == "h" || b.keys == "H") {
            assert!(b.hint.is_none(), "{} on the status bar", b.keys);
        }
    }

    #[test]
    fn landing_a_hotfix_needs_one_checked_out() {
        let rolls = vec![roll_n(1, RollState::Active)];
        assert!(validate_action(
            Action::LandHotfix,
            None,
            &rolls,
            "hotfix/1-0720-urgent",
            "roll/"
        )
        .is_ok());
        let err = validate_action(Action::LandHotfix, None, &rolls, "main", "roll/")
            .expect_err("main is not a hotfix");
        assert!(err.contains("[space]"), "{err}");
    }

    #[test]
    fn the_land_modal_names_both_merges() {
        let cfg = config("main", "develop");
        let out = draw(|f, area| {
            render_modal(
                f,
                area,
                &cfg,
                &ConfirmModal {
                    action: Action::LandHotfix,
                    target: None,
                    carried: &[],
                    rolls: &[],
                    hotfixes: &[],
                    current_branch: "hotfix/1-0720-urgent",
                },
            )
        });
        assert!(
            out.contains("Land hotfix/1-0720-urgent into main, then reintegrate into develop?"),
            "{out}"
        );
    }

    #[test]
    fn the_create_modal_says_which_tier_it_creates() {
        let cfg = config("main", "develop");
        let roll = draw(|f, area| render_create_input(f, area, &cfg, "", CreateKind::Roll));
        assert!(roll.contains("New roll slug"), "{roll}");
        assert!(roll.contains(" create roll "), "{roll}");
        let hot = draw(|f, area| render_create_input(f, area, &cfg, "urg", CreateKind::Hotfix));
        assert!(
            hot.contains("New hotfix slug (branched from main)"),
            "{hot}"
        );
        assert!(hot.contains("urg_"), "{hot}");
    }

    #[test]
    fn a_hotfix_row_gets_the_same_delete_shapes_as_a_roll() {
        assert_eq!(
            delete_prompt("hotfix/1-0720-x", &BranchLocation::Both, false),
            Ok(DeletePrompt::Choice)
        );
        let err = delete_prompt("hotfix/1-0720-x", &BranchLocation::Local, true)
            .expect_err("checked out");
        assert!(err.contains("checked out"), "{err}");
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
            hotfixes: Vec::new(),
            show_deps: false,
            tracking,
            versions: HashMap::new(),
            version: None,
            table: TableState::default(),
            mode: Mode::Browsing,
            message: None,
            job: None,
            panel: None,
            panel_maximized: false,
            panel_rect: None,
            pending_g: false,
            pending_push: None,
            command_history: Vec::new(),
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
    fn the_version_column_shows_the_numbers_and_a_dash_when_absent() {
        let mut rolls = vec![roll_n(1, RollState::Active), roll_n(2, RollState::Active)];
        rolls[0].branch = "roll/1-0101-versioned".to_string();
        rolls[1].branch = "roll/2-0102-bare".to_string();

        let mut versions = HashMap::new();
        versions.insert("roll/1-0101-versioned".to_string(), v(0, 12, 34));

        let mut app = StatusApp {
            config: test_config(),
            current_branch: "main".to_string(),
            bases: Vec::new(),
            rolls,
            hotfixes: Vec::new(),
            show_deps: false,
            tracking: HashMap::new(),
            versions,
            version: None,
            table: TableState::default(),
            mode: Mode::Browsing,
            message: None,
            job: None,
            panel: None,
            panel_maximized: false,
            panel_rect: None,
            pending_g: false,
            pending_push: None,
            command_history: Vec::new(),
        };
        let out = draw(|f, area| app.render_table(f, area));

        assert!(out.contains("version"), "no header:\n{out}");
        assert!(out.contains("0.12.34"), "{out}");
        // A branch whose ref carries no readable manifest gets a dash, the same
        // vocabulary the sync column uses for "nothing to say".
        assert!(out.contains("—"), "{out}");
    }

    #[test]
    fn the_version_column_is_absent_in_a_repo_with_no_manifest() {
        // The whole opt-in rule: no `Cargo.toml` anywhere, no column, no cost to
        // the `branch` column. This is every dotfiles repo.
        let mut app = StatusApp {
            config: test_config(),
            current_branch: "main".to_string(),
            bases: Vec::new(),
            rolls: vec![roll_n(1, RollState::Active)],
            hotfixes: Vec::new(),
            show_deps: false,
            tracking: HashMap::new(),
            versions: HashMap::new(),
            version: None,
            table: TableState::default(),
            mode: Mode::Browsing,
            message: None,
            job: None,
            panel: None,
            panel_maximized: false,
            panel_rect: None,
            pending_g: false,
            pending_push: None,
            command_history: Vec::new(),
        };
        let out = draw(|f, area| app.render_table(f, area));
        assert!(!out.contains("version"), "column shown anyway:\n{out}");
    }

    #[test]
    fn a_version_cell_shows_the_dev_suffix() {
        // Through `Display`: the `#` column says which roll a row is, not
        // whether that roll's dev marker has actually been applied yet, so the
        // suffix has to show here.
        assert_eq!(version_cell(Some(v(0, 2, 4))), "0.2.4");
        assert_eq!(
            version_cell(Some(Semver {
                marker: crate::core::version::Marker::Roll(9),
                ..v(0, 2, 4)
            })),
            "0.2.4-roll9"
        );
        assert_eq!(version_cell(None), "—");
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
            hotfixes: Vec::new(),
            show_deps: false,
            tracking: HashMap::new(),
            versions: HashMap::new(),
            version: None,
            table: TableState::default(),
            mode: Mode::Browsing,
            message: None,
            job: None,
            panel: None,
            panel_maximized: false,
            panel_rect: None,
            pending_g: false,
            pending_push: None,
            command_history: Vec::new(),
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
            marker: crate::core::version::Marker::None,
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
    fn the_verify_many_picker_maps_digits_to_sets_in_menu_order() {
        for (i, set) in VerifySet::ALL.iter().enumerate() {
            let key = KeyCode::Char(char::from_digit(i as u32 + 1, 10).unwrap());
            assert_eq!(
                verify_many_key(key),
                VerifyManyOutcome::Set(*set),
                "{key:?}"
            );
        }
        // A digit past the menu is ignored rather than wrapping or panicking.
        assert_eq!(
            verify_many_key(KeyCode::Char('9')),
            VerifyManyOutcome::Ignore
        );
        for key in [KeyCode::Char('n'), KeyCode::Char('N'), KeyCode::Esc] {
            assert_eq!(verify_many_key(key), VerifyManyOutcome::Cancel, "{key:?}");
        }
        // Enter is deliberately unbound: this pass switches branches, so a
        // stray Enter must not start it.
        for key in [KeyCode::Enter, KeyCode::Char(' '), KeyCode::Char('V')] {
            assert_eq!(verify_many_key(key), VerifyManyOutcome::Ignore, "{key:?}");
        }
    }

    #[test]
    fn verify_sets_select_by_state_and_never_include_promoted_rolls() {
        let rolls = vec![
            roll_n(1, RollState::Active),
            roll_n(2, RollState::Blocked),
            roll_n(3, RollState::Graduated),
            roll_n(4, RollState::Diverged),
            roll_n(5, RollState::Promoted),
            RollInfo {
                location: BranchLocation::Remote,
                ..roll_n(6, RollState::Active)
            },
        ];
        let names = |set: VerifySet| -> Vec<u32> {
            set.select(&rolls)
                .iter()
                .map(|b| branches::parse_roll_number(b, "roll/").unwrap())
                .collect()
        };
        // Promoted has nowhere left to go, so it is in no set — not even All.
        assert_eq!(names(VerifySet::All), vec![1, 2, 3, 4, 6]);
        assert_eq!(names(VerifySet::Active), vec![1, 6]);
        assert_eq!(names(VerifySet::Blocked), vec![2]);
        assert_eq!(names(VerifySet::Graduated), vec![3]);
        assert_eq!(names(VerifySet::Diverged), vec![4]);
        // Local is a location filter: the remote-only roll drops out.
        assert_eq!(names(VerifySet::Local), vec![1, 2, 3, 4]);

        let counts = verify_set_counts(&rolls);
        assert_eq!(counts[0], (VerifySet::All, 5));
        assert_eq!(counts[5], (VerifySet::Local, 4));
    }

    #[test]
    fn the_verify_many_modal_shows_each_set_with_its_count() {
        let rolls = vec![
            roll_n(1, RollState::Active),
            roll_n(2, RollState::Graduated),
        ];
        let counts = verify_set_counts(&rolls);
        let out = draw(|f, area| render_verify_many_modal(f, area, &counts));
        assert!(out.contains("[1] all rolls"), "{out}");
        assert!(out.contains("2 rolls"), "{out}");
        assert!(out.contains("[2] active"), "{out}");
        assert!(out.contains("1 roll "), "{out}");
        assert!(out.contains("[n] cancel"), "{out}");
    }

    #[test]
    fn a_verify_many_report_summarises_and_names_the_failures() {
        let results = vec![
            ops::VerifyManyResult {
                branch: "roll/1-x".to_string(),
                outcome: None,
                marked: Some(crate::core::version::Semver::parse("0.0.1-roll1").unwrap()),
                verdict: ops::VerifyVerdict::Passed,
            },
            ops::VerifyManyResult {
                branch: "roll/2-y".to_string(),
                outcome: None,
                marked: None,
                verdict: ops::VerifyVerdict::Failed("gate exited 1".to_string()),
            },
            ops::VerifyManyResult {
                branch: "roll/3-z".to_string(),
                outcome: None,
                marked: None,
                verdict: ops::VerifyVerdict::Skipped("no local copy".to_string()),
            },
        ];
        let (lines, failed) = render_verify_many(&results);
        assert_eq!(failed, vec!["roll/2-y"]);
        assert!(
            lines.contains(&"version marked 0.0.1-roll1".to_string()),
            "{lines:?}"
        );
        assert!(lines.contains(&"── roll/2-y ──".to_string()), "{lines:?}");
        assert!(
            lines.contains(&"FAILED: gate exited 1".to_string()),
            "{lines:?}"
        );
        assert!(lines.contains(&"skipped: no local copy".to_string()));
        assert_eq!(lines.last().unwrap(), "1 passed, 1 failed, 1 skipped");
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
    fn the_header_names_the_running_binary_not_the_manifest() {
        // Whatever the checked-out branch's Cargo.toml says — or whether there
        // is one — the corner answers "which rf is this".
        let mut app = StatusApp::new(test_config(), "main".to_string(), Vec::new(), false);
        let expected = format!("rf v{}", env!("CARGO_PKG_VERSION"));

        app.version = Some(v(9, 9, 9));
        let with = draw(|f, area| app.render_header(f, area));
        assert!(with.contains(&expected), "{with}");
        assert!(
            !with.contains("9.9.9"),
            "manifest version leaked in:\n{with}"
        );

        app.version = None;
        let without = draw(|f, area| app.render_header(f, area));
        assert!(without.contains(&expected), "{without}");
        assert!(without.contains("Branch: main"), "{without}");
    }

    #[test]
    fn the_conflict_modal_only_offers_integrate_for_the_checked_out_roll() {
        // `ops::integrate` merges into HEAD, so [i] for any other row would
        // integrate into the wrong branch. The key is simply dead there.
        assert_eq!(
            conflict_key(KeyCode::Char('i'), true),
            ConflictOutcome::Integrate
        );
        assert_eq!(
            conflict_key(KeyCode::Char('i'), false),
            ConflictOutcome::Ignore
        );
        assert_eq!(
            conflict_key(KeyCode::Char('m'), false),
            ConflictOutcome::Stage
        );
        for key in [KeyCode::Char('n'), KeyCode::Esc] {
            assert_eq!(conflict_key(key, true), ConflictOutcome::Close);
        }
        // Enter is unbound: every choice leaves a merge to finish by hand.
        assert_eq!(conflict_key(KeyCode::Enter, true), ConflictOutcome::Ignore);
    }

    #[test]
    fn a_diagnosed_conflict_becomes_panel_lines_and_a_followup() {
        let conflict = ops::MergeConflict {
            report: ops::ConflictReport {
                source: "roll/3-x".to_string(),
                target: "rolling".to_string(),
                conflicts: vec![ops::Conflict {
                    path: "src/tui/rolls.rs".to_string(),
                    culprits: vec![ops::Culprit {
                        commit: "abcdef0123".to_string(),
                        subject: "Graduate roll/8-y into rolling".to_string(),
                        roll: Some("roll/8-y".to_string()),
                    }],
                }],
            },
            original: "roll/3-x".to_string(),
        };

        let done = conflict_job_done(&conflict, "roll/3-x");
        let text = done.lines.join("\n");
        assert!(text.contains("src/tui/rolls.rs"), "{text}");
        assert!(text.contains("roll/8-y  (abcdef0"), "{text}");
        assert!(text.contains("NOT merged"), "{text}");
        match done.next {
            Some(Followup::OfferConflictResolution {
                culprits,
                can_integrate,
                ..
            }) => {
                assert_eq!(culprits, vec!["roll/8-y".to_string()]);
                assert!(can_integrate, "source is checked out, so [i] must be live");
            }
            other => panic!("wrong followup: {other:?}"),
        }

        // Same conflict seen from a different checkout: no integrate offer.
        let done = conflict_job_done(&conflict, "rolling");
        assert!(matches!(
            done.next,
            Some(Followup::OfferConflictResolution {
                can_integrate: false,
                ..
            })
        ));
    }

    #[test]
    fn the_conflict_modal_names_the_culprits_and_the_keys() {
        let out = draw(|f, area| {
            render_conflict_modal(
                f,
                area,
                "roll/3-x",
                "rolling",
                &["roll/8-y".to_string()],
                true,
            )
        });
        assert!(out.contains("via: roll/8-y"), "{out}");
        assert!(out.contains("[i] integrate"), "{out}");
        assert!(out.contains("[m] redo the merge"), "{out}");

        let elsewhere = draw(|f, area| {
            render_conflict_modal(
                f,
                area,
                "roll/3-x",
                "rolling",
                &["roll/8-y".to_string()],
                false,
            )
        });
        assert!(!elsewhere.contains("[i] integrate"), "{elsewhere}");
        assert!(
            elsewhere.contains("[space] switch to roll/3-x"),
            "{elsewhere}"
        );
    }

    #[test]
    fn the_version_sits_in_the_top_right_corner_clear_of_the_branch_name() {
        // The branch line grows with the branch name, so the version has to be
        // somewhere that length cannot push it out of. Measured, not assumed.
        let app = StatusApp::new(
            test_config(),
            "roll/12-0918-a-deliberately-long-slug".to_string(),
            Vec::new(),
            false,
        );
        let out = draw(|f, area| app.render_header(f, area));
        let top = out.lines().next().expect("a top border row");
        let at = top
            .find("rf v")
            .unwrap_or_else(|| panic!("no version:\n{out}"));
        assert!(at > top.chars().count() / 2, "not right-aligned:\n{out}");
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
    fn integrate_rolling_merges_rolling_into_the_checked_out_roll() {
        assert_eq!(
            integrate_rolling_target_for("roll/2-0102-beta", "roll/", "rolling"),
            Ok("rolling".to_string())
        );
    }

    #[test]
    fn integrate_rolling_needs_a_roll_branch_checked_out() {
        for head in ["main", "rolling", "feature/x"] {
            let err = integrate_rolling_target_for(head, "roll/", "rolling")
                .expect_err("{head} should be refused");
            assert!(err.contains("not a roll branch"), "{head}: {err}");
        }
    }

    #[test]
    fn integrate_rolling_refuses_when_rolling_is_already_current() {
        // Can't happen through the gate above since `rolling` doesn't start
        // with the roll prefix, but a custom prefix could make it ambiguous —
        // this keeps the self-merge refusal explicit either way.
        let err = integrate_rolling_target_for("rolling", "", "rolling")
            .expect_err("self-merge should be refused");
        assert!(err.contains("already the current branch"), "{err}");
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
    fn the_graduate_modal_lists_the_chain_when_there_is_one() {
        // A blocked roll's confirm has to say what else is about to land: the
        // modal is the confirmation, and it would otherwise read as one merge.
        let cfg = config("main", "rolling");
        let mut dep = roll_n(8, RollState::Active);
        dep.branch = "roll/8-0101-dep".to_string();
        let mut target = roll_n(9, RollState::Blocked);
        target.branch = "roll/9-0102-target".to_string();
        target.deps = vec![8];
        let rolls = vec![dep, target];

        let out = draw(|f, area| {
            render_modal(
                f,
                area,
                &cfg,
                &ConfirmModal {
                    action: Action::Graduate,
                    target: Some("roll/9-0102-target"),
                    carried: &[],
                    rolls: &rolls,
                    hotfixes: &[],
                    current_branch: "roll/9-0102-target",
                },
            )
        });
        assert!(
            out.contains("Graduate 2 rolls into rolling, in this order?"),
            "{out}"
        );
        assert!(
            out.contains("1. roll/8-0101-dep  (dependency of 9, active)"),
            "{out}"
        );
        // The blocked glyph is double-width and pads the buffer text, so the
        // target row is matched in two halves.
        assert!(out.contains("2. roll/9-0102-target"), "{out}");
        assert!(out.contains("blocked)"), "{out}");

        // And a blocked roll passes validation — the chain is the answer.
        assert!(
            validate_action(Action::Graduate, Some(&rolls[1]), &rolls, "main", "roll/").is_ok()
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
                &ConfirmModal {
                    action: Action::Integrate,
                    target: Some("roll/1-0101-alpha"),
                    carried: &[],
                    rolls: &[],
                    hotfixes: &[],
                    current_branch: "roll/2-0102-beta",
                },
            )
        });
        assert!(
            out.contains("Integrate roll/1-0101-alpha into roll/2-0102-beta?"),
            "{out}"
        );
    }

    #[test]
    fn the_promote_modal_lists_the_rolls_a_roll_promotion_would_carry() {
        // `[m]` on one roll advances stable to that roll's graduation commit, so
        // earlier graduations land too. The modal is the last place the user can
        // see that before the merge, so it has to name them.
        let cfg = config("main", "rolling");
        let carried = vec![
            "roll/4-0918-version-corner".to_string(),
            "roll/7-0918-verify-button".to_string(),
        ];
        let out = draw(|f, area| {
            render_modal(
                f,
                area,
                &cfg,
                &ConfirmModal {
                    action: Action::Promote,
                    target: Some("roll/8-0918-help-menu"),
                    carried: &carried,
                    rolls: &[],
                    hotfixes: &[],
                    current_branch: "rolling",
                },
            )
        });
        assert!(
            out.contains("Promote roll/8-0918-help-menu into main?"),
            "{out}"
        );
        assert!(out.contains("also lands"), "{out}");
        for roll in &carried {
            assert!(out.contains(roll.as_str()), "{roll} missing from:\n{out}");
        }
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

        let tidy = draw(|f, area| {
            render_modal(
                f,
                area,
                &cfg,
                &ConfirmModal {
                    action: Action::Tidy,
                    target: None,
                    carried: &[],
                    rolls: &rolls,
                    hotfixes: &[],
                    current_branch: "main",
                },
            )
        });
        assert!(
            tidy.contains("Delete 2 local graduated/promoted roll branches (local only)?"),
            "{tidy}"
        );

        let prune = draw(|f, area| {
            render_modal(
                f,
                area,
                &cfg,
                &ConfirmModal {
                    action: Action::Prune,
                    target: None,
                    carried: &[],
                    rolls: &rolls,
                    hotfixes: &[],
                    current_branch: "main",
                },
            )
        });
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
    fn row_at_maps_hotfixes_after_the_rolls() {
        assert_eq!(row_at(5, 2, 3, 2), Some(RowKind::Hotfix(0)));
        assert_eq!(row_at(6, 2, 3, 2), Some(RowKind::Hotfix(1)));
        assert_eq!(row_at(7, 2, 3, 2), None);
        // No rolls at all: hotfixes follow the bases directly.
        assert_eq!(row_at(2, 2, 0, 1), Some(RowKind::Hotfix(0)));
    }

    #[test]
    fn a_hotfix_row_renders_below_the_rolls_with_its_own_number() {
        let hotfixes = vec![
            HotfixInfo {
                branch: "hotfix/1-0720-urgent".to_string(),
                number: 1,
                state: HotfixState::Open,
                location: BranchLocation::Local,
                is_current: false,
            },
            HotfixInfo {
                branch: "hotfix/2-0721-landed-one".to_string(),
                number: 2,
                state: HotfixState::Landed,
                location: BranchLocation::Both,
                is_current: false,
            },
        ];
        let mut table = TableState::default();
        table.select(initial_selection(&[], &[], &hotfixes));
        let mut app = StatusApp {
            config: test_config(),
            current_branch: "main".to_string(),
            bases: Vec::new(),
            rolls: vec![roll_n(1, RollState::Active)],
            hotfixes,
            show_deps: false,
            tracking: HashMap::new(),
            versions: HashMap::new(),
            version: None,
            table,
            mode: Mode::Browsing,
            message: None,
            job: None,
            panel: None,
            panel_maximized: false,
            panel_rect: None,
            pending_g: false,
            pending_push: None,
            command_history: Vec::new(),
        };
        let out = draw(|f, area| app.render_table(f, area));
        let lines: Vec<&str> = out.lines().collect();
        let roll_line = lines
            .iter()
            .position(|l| l.contains("roll/1-0101-x"))
            .unwrap();
        let hotfix_line = lines
            .iter()
            .position(|l| l.contains("hotfix/1-0720-urgent"))
            .unwrap();
        assert!(hotfix_line > roll_line, "hotfix above the roll:\n{out}");
        // A thin rule sits between the two tiers, in a line of its own — not a
        // row, so selection indices are unaffected.
        assert_eq!(hotfix_line, roll_line + 2, "no gap for the rule:\n{out}");
        assert!(
            lines[roll_line + 1].contains("┄┄┄"),
            "no separator between rolls and hotfixes:\n{out}"
        );
        // ...and none without hotfixes to separate.
        let hotfixes = std::mem::take(&mut app.hotfixes);
        let bare = draw(|f, area| app.render_table(f, area));
        assert!(!bare.contains('┄'), "separator with no hotfixes:\n{bare}");
        app.hotfixes = hotfixes;
        // `h1`, not a bare `1`: hotfix numbering is independent of rolls, and
        // this row sits under a roll that is also number 1.
        assert!(lines[hotfix_line].contains("h1"), "{}", lines[hotfix_line]);
        assert!(
            lines[hotfix_line].contains("hotfix"),
            "{}",
            lines[hotfix_line]
        );
        assert!(out.contains("✓ landed"), "{out}");
        // The current-branch hunt reaches hotfix rows too.
        let mut current = app.hotfixes.clone();
        current[1].is_current = true;
        assert_eq!(initial_selection(&[], &app.rolls, &current), Some(2));
    }

    #[test]
    fn base_rows_render_above_the_rolls() {
        use ratatui::{backend::TestBackend, Terminal};

        let cfg = config("main", "develop");
        let bases = base_branches(&cfg, "develop", |_| true);
        let mut table = TableState::default();
        table.select(initial_selection(&bases, &[], &[]));
        let mut app = StatusApp {
            config: cfg,
            current_branch: "develop".to_string(),
            bases,
            rolls: vec![roll_n(1, RollState::Active)],
            hotfixes: Vec::new(),
            show_deps: false,
            tracking: HashMap::new(),
            versions: HashMap::new(),
            version: None,
            table,
            mode: Mode::Browsing,
            message: None,
            job: None,
            panel: None,
            panel_maximized: false,
            panel_rect: None,
            pending_g: false,
            pending_push: None,
            command_history: Vec::new(),
        };

        // Tall enough for the header, three table rows and the five-line status
        // bar, and wide enough for the sync column.
        let mut term = Terminal::new(TestBackend::new(80, 15)).unwrap();
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

    /// Rolls 14 and 15 integrated each other; `lacks` empty makes 15 the
    /// carrier, non-empty leaves the cycle without one.
    fn cycle_14_15(lacks: &[u32]) -> Vec<RollInfo> {
        let cycle = branches::DepCycle {
            members: vec![14, 15],
            suggested: 15,
            lacks: lacks.to_vec(),
        };
        let mut r14 = roll_n(14, RollState::Blocked);
        r14.deps = vec![15];
        r14.dependents = vec![15];
        r14.cycle = Some(cycle.clone());
        let mut r15 = roll_n(15, RollState::Active);
        r15.deps = vec![14];
        r15.dependents = vec![14];
        r15.cycle = Some(cycle);
        vec![r14, r15]
    }

    #[test]
    fn a_carrier_shows_its_cycle_member_as_carried_not_blocking() {
        let all = cycle_14_15(&[]);
        // 15 carries 14: ungraduated, but it gates nothing.
        let rows = dep_rows(&all[1], &all);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].carried && !rows[0].is_blocker, "{rows:?}");
        // 14 really is waiting on 15, which is what lands it.
        let rows = dep_rows(&all[0], &all);
        assert!(rows[0].is_blocker && !rows[0].carried, "{rows:?}");
        // And from the other side: 15, as 14's dependent, is not gated by it.
        let rows = dependent_rows(&all[0], &all);
        assert!(rows[0].carried && !rows[0].is_blocker, "{rows:?}");
    }

    #[test]
    fn the_detail_cycle_note_says_which_roll_to_graduate() {
        let all = cycle_14_15(&[]);
        let lines = cycle_lines(all[0].cycle.as_ref().unwrap(), &all);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains("14 ⇄ 15"), "{lines:?}");
        assert!(lines[1].contains("graduate roll/15-0101-x"), "{lines:?}");
    }

    #[test]
    fn graduating_a_carried_member_lists_the_carrier_in_the_confirm() {
        // A one-step chain, but not of the row under the cursor — so the
        // modal must say what will actually merge.
        let all = cycle_14_15(&[]);
        let lines = chain_lines(&all, "roll/14-0101-x", ops::ChainKind::Graduate)
            .expect("the plan is listed");
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("roll/15-0101-x"), "{lines:?}");
        assert!(lines[0].contains("carries 14"), "{lines:?}");
        assert!(validate_action(Action::Graduate, Some(&all[0]), &all, "main", "roll/").is_ok());
    }

    #[test]
    fn graduating_a_cycle_with_no_carrier_is_refused_with_the_remedy() {
        let all = cycle_14_15(&[14]);
        for sel in &all {
            let err =
                validate_action(Action::Graduate, Some(sel), &all, "main", "roll/").unwrap_err();
            assert!(err.contains("`rf integrate roll/14-0101-x`"), "{err}");
        }
    }

    #[test]
    fn dep_chain_carries_the_carried_marker_onto_its_rows() {
        // Regression test: `dep_chain` builds each `ChainRow` from `dep_rows`,
        // and used to drop `carried` doing it — a merge-conflict casualty
        // (ChainRow predates the cycle feature) that left a carried member
        // reading as a plain blocker several levels down. `dep_rows` itself
        // was already covered by `a_carrier_shows_its_cycle_member_as_carried_
        // not_blocking`; this is the same fact, through `dep_chain` instead.
        let all = cycle_14_15(&[]);
        let chain = dep_chain(&all[1], &all); // 15, the carrier
                                              // 14 at depth 0, then 15 again at depth 1 (14's own dep is 15 —
                                              // the cycle), marked `repeated` since 15 is the pane's own root.
        let direct = chain.iter().find(|r| r.number == 14).expect("14 in chain");
        assert!(
            direct.carried && !direct.is_blocker,
            "carried dropped in dep_chain: {direct:?}"
        );
    }

    #[test]
    fn the_detail_pane_marks_a_carried_dependency_rather_than_a_blocker() {
        let all = cycle_14_15(&[]);
        // Selecting 15 (the carrier): its one dependency, 14, is carried.
        let out = draw(|f, area| render_detail(f, area, &all[1], None, &all, 0, &[]));
        assert!(out.contains("↻ carried"), "{out}");
        assert!(!out.contains("⛔ blocker"), "{out}");
    }
}
