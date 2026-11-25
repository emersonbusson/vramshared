// Author: Emerson Busson
// Email: emersonbusson@gmail.com
// GitHub: https://github.com/emersonbusson
// LinkedIn: https://www.linkedin.com/in/emersonbusson
// Source: https://github.com/emersonbusson/vramshared
// File: crates/ramshared-cli/src/release_coherence.rs
//! Release coherence — the product fleet, the joint move, and the client transition.
//!
//! **Why this module exists.** Selecting a release repoints
//! `/opt/ramshared/current`. A running process pins its executable inode, so a
//! repointed selector cannot change the text a live process is executing. The
//! install therefore used to complete while product processes kept running
//! older bytes: the supervisor that owns victim selection and the monitor that
//! publishes `overall_state` could both be on a previous release while status
//! reported `HEALTHY`.
//!
//! The contract, in three parts:
//! - **the set is derived, never listed** ([`product_process_set`]): what a
//!   release ships under `systemd/`, keeping only the units that execute that
//!   release's own binaries. A unit-name constant is how the drift started.
//! - **the fleet is one observation** ([`ReleaseFleet::aggregate`]): every
//!   product process is classified with the crate's single process-vs-install
//!   predicate ([`RunningInstallStatus`]), and the aggregate never says
//!   `Coherent` unless every member is `Installed*`.
//!
//! **Time and identity.** A start before selection establishes a stale
//! generation, while a later start alone does not identify an executable.
//! The identity predicate ([`classify_executable`]) must also match the
//! selected release. Unreadable executables and script interpreters remain
//! unknown until their actual images can be established.
//! - **the move is planned before it acts** ([`plan_joint_move`] /
//!   [`apply_joint_move`]): only units that are already active and stale are
//!   restarted. There is no direct `start` or `kill` effect, but systemd may
//!   escalate a restart according to the unit's own policy.
//!
//! A user client ([`ProductProcessKind::Client`]) is never in the move. It
//! refreshes itself by re-exec; this module only describes that transition
//! ([`client_update_notice`]).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::time::Duration;
use std::time::SystemTime;

use crate::installed_image::{
    ProcessInstallOrigin, RunningInstallStatus, install_entries_for_program,
    process_install_origin_for_path, running_install_status_from_metadata,
};

mod executable_observation;
mod inventory;
mod joint_move;
mod selector;
mod sync;
mod sync_custody;
mod systemd;
mod systemd_jobs;
use executable_observation::ObservedExecutable;
#[cfg(test)]
pub(crate) use joint_move::MoveOutcome;
pub(crate) use joint_move::{
    JointMoveReport, ReleaseCoherenceEffect, RestartFailure, apply_joint_move,
};
pub(crate) use sync::sync_selected_release;
pub(crate) use systemd::observe_release_coherence;
#[cfg(test)]
use systemd::parse_unit_observation;
pub(crate) use systemd_jobs::{unit_is_masked_or_absent, unit_is_quiescent};

pub(crate) fn selected_release_version() -> Result<Option<String>, String> {
    selector::selected_release_observation(Path::new(crate::installed_image::PRODUCT_ROOT))
}

/// Ephemeral hint naming the release a client left when it re-exec'd itself.
///
/// Display-only and **non-authoritative** (SPEC DT-4): the exec target is
/// chosen from the installed identity and identity is read from
/// `/proc/self/exe` plus the sealed `RELEASE_VERSION`. A forged hint can change
/// a notice string and can never move a process onto a release.
pub(crate) const PREVIOUS_RELEASE_HINT_ENV: &str = "RAMSHARED_CLIENT_PREVIOUS_RELEASE";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ReleaseCoherenceError {
    NotAReleaseRoot(PathBuf),
    UnreadableUnit { unit: String, reason: String },
}

impl std::fmt::Display for ReleaseCoherenceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAReleaseRoot(path) => {
                write!(
                    formatter,
                    "release root is not a directory: {}",
                    path.display()
                )
            }
            Self::UnreadableUnit { unit, reason } => {
                write!(formatter, "unit {unit} is not readable: {reason}")
            }
        }
    }
}

/// What a release ships as one managed product unit.
///
/// `exec_paths` starts with release-owned `ExecStart` programs and then lists
/// auxiliary programs under the release root. A unit without a release-owned
/// `ExecStart` is not declared as a product process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UnitDeclaration {
    pub(crate) unit: String,
    pub(crate) exec_paths: Vec<PathBuf>,
}

/// Read the release's own `systemd/` payload and declare the product units.
///
/// The set is whatever the payload says. A release that ships no units declares
/// no product units; a release that ships one more declares it. Nothing in this
/// module names a unit.
pub(crate) fn product_process_set(
    release_root: &Path,
) -> Result<Vec<UnitDeclaration>, ReleaseCoherenceError> {
    if !release_root.is_dir() {
        return Err(ReleaseCoherenceError::NotAReleaseRoot(
            release_root.to_path_buf(),
        ));
    }
    let unit_dir = release_root.join("systemd");
    let mut declarations = Vec::new();
    let Some(entries) = inventory::read_unit_directory(&unit_dir)? else {
        return Ok(declarations);
    };
    let mut names: BTreeSet<String> = BTreeSet::new();
    for entry in entries {
        let entry = entry.map_err(|error| ReleaseCoherenceError::UnreadableUnit {
            unit: unit_dir.display().to_string(),
            reason: error.to_string(),
        })?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".service") {
            names.insert(name);
        }
    }
    for unit in names {
        let path = unit_dir.join(&unit);
        // The same sealed-document reader the release identity goes through:
        // the open is `O_NOFOLLOW` and the regular-file test runs on the
        // descriptor the bytes come from. A unit slot occupied by a redirect
        // would otherwise be followed and its target's `Exec*` lines counted
        // as this release's declaration. The refusal reason names the rule
        // rather than an io implementation string — this caller's question is
        // whether the unit declared an executable under this release, not why
        // the bytes could not be read.
        let text = crate::release_identity::read_regular_text(&path).ok_or_else(|| {
            ReleaseCoherenceError::UnreadableUnit {
                unit: unit.clone(),
                reason: "not a readable regular file".to_string(),
            }
        })?;
        let exec_paths = declared_exec_paths(release_root, &text);
        if exec_paths.is_empty() {
            // Executing something outside this release (or nothing at all) is
            // not a product process of this release. `ramsharedd.service` is
            // inert staging for exactly this reason.
            continue;
        }
        declarations.push(UnitDeclaration { unit, exec_paths });
    }
    Ok(declarations)
}

/// The `ExecStart*` programs of a unit file that live under `release_root`.
///
/// A release-owned `ExecStart=` is required and comes before auxiliary
/// commands, so the first path names the main program rather than a precheck.
/// Paths must be absolute and resolve inside the release root.
fn declared_exec_paths(release_root: &Path, unit_text: &str) -> Vec<PathBuf> {
    let mut main = Vec::new();
    let mut auxiliary = Vec::new();
    for line in unit_text.lines() {
        let trimmed = line.trim();
        let is_main = trimmed.starts_with("ExecStart=");
        let Some(rest) = strip_exec_key(trimmed) else {
            continue;
        };
        if is_main && rest.trim().is_empty() {
            main.clear();
            continue;
        }
        let Some(program) = first_program(rest) else {
            continue;
        };
        let candidate = PathBuf::from(&program);
        if !candidate.is_absolute() {
            continue;
        }
        if path_is_under(release_root, &candidate) {
            if is_main {
                main.push(candidate);
            } else {
                auxiliary.push(candidate);
            }
        }
    }
    if !main.is_empty() {
        main.extend(auxiliary);
    }
    main
}

fn strip_exec_key(line: &str) -> Option<&str> {
    for key in ["ExecStart=", "ExecStartPre=", "ExecStartPost=", "ExecStop="] {
        if let Some(rest) = line.strip_prefix(key) {
            return Some(rest);
        }
    }
    None
}

/// First whitespace-separated token of an `Exec*` line, with systemd's
/// command-prefix characters stripped and one layer of quoting removed.
fn first_program(rest: &str) -> Option<String> {
    let rest = rest.trim_start_matches(['@', '-', '+', '!', ':']);
    let mut chars = rest.chars().peekable();
    let quote = match chars.peek() {
        Some('"') | Some('\'') => {
            let quote = *chars.peek()?;
            chars.next();
            Some(quote)
        }
        _ => None,
    };
    let mut program = String::new();
    if let Some(quote) = quote {
        let mut closed = false;
        for char in chars.by_ref() {
            if char == quote {
                closed = true;
                break;
            }
            program.push(char);
        }
        // An unterminated quote means the line is malformed. Returning the
        // partial path would declare a program the unit never named.
        if !closed {
            return None;
        }
    } else {
        for char in chars.by_ref() {
            if char.is_whitespace() {
                break;
            }
            program.push(char);
        }
    }
    (!program.is_empty()).then_some(program)
}

fn path_is_under(root: &Path, candidate: &Path) -> bool {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let candidate = candidate
        .canonicalize()
        .unwrap_or_else(|_| candidate.to_path_buf());
    candidate.starts_with(&root)
}

/// Who a product process is in the fleet.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ProductProcessKind {
    /// A unit the release declared and systemd owns.
    Systemd(String),
    /// A process running product code outside product custody — a user client
    /// such as `ramshared top`. Observed, never moved (DT-8).
    ///
    /// Today's observers only emit [`Self::Systemd`]: the unit payload is the
    /// only set this module declares. `Client` is still part of the model
    /// because DT-8 is a rule about a *kind of fleet member*, and
    /// [`ReleaseFleet::aggregate`] and [`plan_joint_move`] are written over
    /// that model. Deleting the variant would delete the only place the rule
    /// is expressible and would take
    /// `a_non_installed_client_does_not_drive_the_fleet_answer` with it.
    #[allow(dead_code)]
    Client,
}

/// One product process and whether it is running the installed image.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProductProcess {
    pub(crate) kind: ProductProcessKind,
    /// The unit's main process, as systemd reported it.
    ///
    /// `Some(n)` with `n != 0` is a running image. `Some(0)` is the observed
    /// fact that the unit has **no** main process — what a completed
    /// `Type=oneshot` `RemainAfterExit=yes` unit reports while still `active`.
    /// `None` means the pid was not observed at all, and is never read as "no
    /// process": that is how a running stale unit gets silently skipped.
    pub(crate) main_pid: Option<u32>,
    pub(crate) executable: PathBuf,
    pub(crate) status: RunningInstallStatus,
    /// For a [`ProductProcessKind::Systemd`] member: is the unit active?
    /// `Some(true)` / `Some(false)` were observed; `None` was not, and "not
    /// observed" is never treated as "off" — that is how a stale unit gets
    /// silently skipped. A client is always `Some(true)`: the process exists.
    pub(crate) active: Option<bool>,
}

impl ProductProcess {
    /// Is this member part of the coherence question — does it hold a running
    /// image that could disagree with the selected release?
    ///
    /// No, when the unit is known off, and no, when it is known up but holds no
    /// process at all (`MainPID=0`: a completed `Type=oneshot`). Neither can be
    /// running bytes from another release, so neither is news.
    ///
    /// Yes, when the unit's activity was never observed, and yes when a process
    /// exists but could not be classified. Those are the observations the
    /// aggregate exists to surface (DT-5).
    pub(crate) fn is_in_coherence_question(&self) -> bool {
        match self.active {
            None => true,
            Some(false) => false,
            Some(true) => self.main_pid.is_none_or(|pid| pid != 0),
        }
    }
}

/// The fleet aggregate. Precedence is SPEC DT-5.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReleaseCoherence {
    /// Every product unit is running the installed image.
    Coherent,
    /// At least one product unit is running something other than the installed
    /// image and that is known. This is the actionable news and outranks
    /// `Unknown`, even when another member is unclassifiable.
    Stale,
    /// Nothing is provably stale, and at least one product unit is
    /// unclassifiable.
    Unknown,
}

impl ReleaseCoherence {
    /// The status-document spelling. One place, so the JSON and the human line
    /// cannot drift apart on the word.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Coherent => "coherent",
            Self::Stale => "stale",
            Self::Unknown => "unknown",
        }
    }
}

/// The product fleet observed against the selected release.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ReleaseFleet {
    pub(crate) selected_release: Option<String>,
    pub(crate) members: Vec<ProductProcess>,
}

impl ReleaseFleet {
    /// DT-5 precedence, over **product members in the coherence question**
    /// only ([`ProductProcess::is_in_coherence_question`]).
    ///
    /// A client is observed and refreshes itself; it never moves (DT-8), so it
    /// never drives this answer. One source-tree `top` must not be able to
    /// stamp an installed fleet "stale" forever.
    ///
    /// A unit with no running image is the same class of non-answer: a unit the
    /// operator left off, and a completed `Type=oneshot` that is `active` with
    /// `MainPID=0`, both hold nothing that could disagree with the selected
    /// release. Reading their `Unknown` status into the aggregate is how one
    /// deliberately-off unit — or one oneshot gate — makes a fleet permanently
    /// non-`Coherent` under the boot policy the product mandates.
    ///
    /// `active: None` is unobserved, not "off", and still drives the answer.
    ///
    /// `Stale` outranks `Unknown` because it is the news the operator must act
    /// on. `Coherent` requires every member in the coherence question to be
    /// `Installed*`.
    pub(crate) fn aggregate(&self) -> ReleaseCoherence {
        let mut any_unknown = false;
        for member in &self.members {
            if matches!(member.kind, ProductProcessKind::Client) {
                continue;
            }
            if !member.is_in_coherence_question() {
                continue;
            }
            if member.status.is_known_stale() {
                return ReleaseCoherence::Stale;
            }
            if member.status.is_unknown() {
                any_unknown = true;
            }
        }
        if any_unknown {
            return ReleaseCoherence::Unknown;
        }
        ReleaseCoherence::Coherent
    }
}

/// The owned plan for one joint move. Built before any effect and executed as
/// written (SPEC DT-7).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct JointMovePlan {
    /// Units already active and not on the selected release. Restarted once.
    pub(crate) restart: Vec<String>,
    /// Units already on the selected release. Untouched.
    pub(crate) already_current: Vec<String>,
    /// Units the operator left off. **Never started.**
    pub(crate) left_inactive: Vec<String>,
    /// Units that are up but hold no running image — a completed
    /// `Type=oneshot` `RemainAfterExit=yes` unit. There is nothing to move and
    /// nothing to claim, so the move leaves it alone; it is still named, so it
    /// cannot vanish from the report.
    pub(crate) left_no_image: Vec<String>,
    /// Units that are up with a running image the identity predicate could not
    /// classify. Left alone: an unknown observation is never proof of
    /// staleness, and moving on it would be moving on a guess. Named, for the
    /// same reason as [`Self::left_no_image`].
    pub(crate) left_unjudged: Vec<String>,
    /// Units whose activity was not observed. Never started, never restarted:
    /// acting on an unobserved unit is acting on a guess.
    pub(crate) unobserved: Vec<String>,
}

/// Classify the fleet into a joint-move plan.
///
/// A unit is restarted only when it is **observed active** and its process is
/// known not to be the installed image. Everything else is left alone and
/// **named**, because the report's contract is that every planned unit appears
/// exactly once: a unit that vanishes is indistinguishable from one the move
/// never saw. An unclassifiable member is not a restart candidate — an unknown
/// observation is never proof of staleness, and moving on it would be moving
/// on a guess. Clients are never in `restart`.
pub(crate) fn plan_joint_move(fleet: &ReleaseFleet) -> JointMovePlan {
    let mut plan = JointMovePlan::default();
    for member in &fleet.members {
        match &member.kind {
            ProductProcessKind::Client => continue,
            ProductProcessKind::Systemd(unit) => match member.active {
                None => plan.unobserved.push(unit.clone()),
                Some(false) => plan.left_inactive.push(unit.clone()),
                // A completed `Type=oneshot` is active with `MainPID=0`: up,
                // but nothing is executing. Same DT-5 rule as the aggregate —
                // there is no image that could disagree with the selected
                // release, so the question is not asked. Named anyway, so the
                // unit cannot vanish from the report.
                Some(true) if !member.is_in_coherence_question() => {
                    plan.left_no_image.push(unit.clone());
                }
                Some(true) if member.status.is_known_stale() => {
                    plan.restart.push(unit.clone());
                }
                Some(true) if member.status.is_installed() => {
                    plan.already_current.push(unit.clone());
                }
                Some(true) => {
                    // Up, running, and the identity predicate could not say
                    // which release the bytes are on. Left alone and named: a
                    // report that omitted it would claim a decision that was
                    // never made.
                    plan.left_unjudged.push(unit.clone());
                }
            },
        }
    }
    plan
}

/// The client's view of a release transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ClientUpdateNotice {
    /// This process already runs the selected release.
    None,
    /// A different release is selected. The client will take it by re-exec.
    Pending { from: String, to: String },
    /// This process is the result of taking it. `from` is the non-authoritative
    /// hint the previous process passed along (SPEC DT-4).
    Completed { from: String, to: String },
}

impl ClientUpdateNotice {
    /// The banner text, naming **both** releases so the operator can see which
    /// transition happened. Empty when there is nothing to say.
    pub(crate) fn banner(&self) -> String {
        match self {
            Self::None => String::new(),
            Self::Pending { from, to } => format!("Update installed · {from} → {to}"),
            Self::Completed { from, to } => format!("Updated {from} → {to}"),
        }
    }
}

/// The pending transition a client should take, or [`ClientUpdateNotice::None`].
///
/// Only a known, different pair produces a notice. A missing identity on
/// either side is not a transition: it is an observation that could not be
/// made, and acting on it would re-exec on a guess.
pub(crate) fn client_update_notice(
    running: Option<&str>,
    selected: Option<&str>,
) -> ClientUpdateNotice {
    match (running, selected) {
        (Some(from), Some(to)) if from != to => ClientUpdateNotice::Pending {
            from: from.to_string(),
            to: to.to_string(),
        },
        _ => ClientUpdateNotice::None,
    }
}

/// The notice a process renders when it is the result of a self-refresh.
pub(crate) fn completed_update_notice(from: Option<&str>, to: &str) -> ClientUpdateNotice {
    match from {
        Some(from) if from != to => ClientUpdateNotice::Completed {
            from: from.to_string(),
            to: to.to_string(),
        },
        _ => ClientUpdateNotice::None,
    }
}

/// The single banner a client shows about a release transition.
///
/// Pending wins: if a different release is selected, that is the news. Only
/// when there is nothing pending does the carried hint report a completed
/// transition. Returns `None` when there is nothing to say.
pub(crate) fn client_banner(
    running: Option<&str>,
    selected: Option<&str>,
    previous_hint: Option<&str>,
) -> Option<String> {
    let pending = client_update_notice(running, selected);
    if pending != ClientUpdateNotice::None {
        return Some(pending.banner());
    }
    let to = selected.or(running)?;
    let completed = completed_update_notice(previous_hint, to);
    (completed != ClientUpdateNotice::None).then(|| completed.banner())
}

/// The non-authoritative "which release did I leave" hint, if one was passed.
///
/// Never an authority. A test proves a forged value cannot choose an exec
/// target (SPEC DT-4); this only ever feeds [`ClientUpdateNotice::Completed`].
pub(crate) fn previous_release_hint() -> Option<String> {
    read_previous_release_hint(&|key| std::env::var_os(key))
}

/// The lookup half of [`previous_release_hint`], separated so the rule is
/// testable without mutating process environment (which is unsafe in this
/// edition and racy under `--test-threads`).
fn read_previous_release_hint(
    lookup: &dyn Fn(&str) -> Option<std::ffi::OsString>,
) -> Option<String> {
    lookup(PREVIOUS_RELEASE_HINT_ENV)
        .map(|value| value.to_string_lossy().into_owned())
        .filter(|value| !value.is_empty())
}

/// The host facts a fleet observation needs. Injected so the classification is
/// testable without systemd and so a failed observation is visible as `None`
/// rather than silently becoming "off".
pub(crate) trait ProductProcessView {
    /// `Some(true)` / `Some(false)` when the unit's state could be read.
    fn unit_is_active(&self, unit: &str) -> Option<bool>;
    /// The unit's main process, when it has one.
    fn unit_main_pid(&self, unit: &str) -> Option<u32>;
    /// The executable image that pid is running (`/proc/<pid>/exe`).
    fn process_executable(&self, pid: u32) -> Option<ObservedExecutable>;
    /// Reobserve the manager's same unit execution after the image check.
    fn unit_observation_still_current(&self, unit: &str) -> bool;
    /// When that pid started. `None` when the host will not say.
    fn process_start_instant(&self, pid: u32) -> Option<SystemTime>;
    /// When the selected-release selector was last repointed.
    fn selected_release_instant(&self) -> Option<SystemTime>;
}

/// The generation answer for one process.
///
/// A process started before the selector moved cannot establish that it
/// adopted the newly selected release. A later start is not image identity.
///
/// A previous generation classifies as [`RunningInstallStatus::NotInstalled`]
/// — the signature of a process kept running after `current` moved, and one the
/// aggregate already reads as known-stale. Equality merely passes the time
/// check; the running image must still be observed independently.
fn classify_generation(start: SystemTime, selected: SystemTime) -> RunningInstallStatus {
    if start < selected {
        RunningInstallStatus::NotInstalled
    } else {
        RunningInstallStatus::InstalledActiveRelease
    }
}

/// Classify one declared unit from whichever observation this host allowed.
///
/// A pre-selector start is stale. A later start must also match an observed
/// executable. An interpreter or an unreadable image remains unknown until
/// its script and process identity can be established independently.
fn classify_member(
    declaration: &UnitDeclaration,
    running_executable: Option<&ObservedExecutable>,
    start: Option<SystemTime>,
    selected: Option<SystemTime>,
) -> RunningInstallStatus {
    if running_executable.is_some_and(|image| !image.still_matches()) {
        return RunningInstallStatus::Unknown;
    }
    let status = if let (Some(start), Some(selected)) = (start, selected) {
        if classify_generation(start, selected) == RunningInstallStatus::NotInstalled {
            RunningInstallStatus::NotInstalled
        } else {
            running_executable
                .map(|executable| classify_executable(executable, declaration))
                .filter(|status| {
                    matches!(
                        status,
                        RunningInstallStatus::InstalledActiveRelease
                            | RunningInstallStatus::InstalledBoth
                    )
                })
                .unwrap_or(RunningInstallStatus::Unknown)
        }
    } else {
        match running_executable {
            Some(executable) => classify_executable(executable, declaration),
            None => RunningInstallStatus::Unknown,
        }
    };
    if running_executable.is_some_and(|image| !image.still_matches()) {
        RunningInstallStatus::Unknown
    } else {
        status
    }
}

/// Build the fleet of a selected release from what this host is running.
///
/// The set of units is whatever that release's payload declares
/// ([`product_process_set`]); nothing here names a unit. Each declared unit is
/// then classified with the crate's one process-vs-install predicate, using the
/// program the unit declares as the active-release entry.
pub(crate) fn observe_fleet(
    release_root: &Path,
    selected_release: Option<String>,
    view: &dyn ProductProcessView,
) -> Result<ReleaseFleet, ReleaseCoherenceError> {
    let declarations = product_process_set(release_root)?;
    let selected_instant = view.selected_release_instant();
    let mut members = Vec::new();
    for declaration in declarations {
        let active = view.unit_is_active(&declaration.unit);
        // `Some(0)` is the observed fact that the unit has no main process (a
        // completed `Type=oneshot`). It is carried through as-is so the
        // aggregate can tell that from an unobserved pid, and it is never used
        // to look up `/proc`.
        let main_pid = view
            .unit_main_pid(&declaration.unit)
            .filter(|_| active == Some(true));
        let running = main_pid
            .filter(|pid| *pid != 0)
            .and_then(|pid| view.process_executable(pid));
        let start = main_pid
            .filter(|pid| *pid != 0)
            .and_then(|pid| view.process_start_instant(pid));
        let mut status = classify_member(&declaration, running.as_ref(), start, selected_instant);
        if active == Some(true)
            && main_pid.is_some_and(|pid| pid != 0)
            && (!view.unit_observation_still_current(&declaration.unit)
                || running.as_ref().is_some_and(|image| !image.still_matches()))
        {
            status = RunningInstallStatus::Unknown;
        }
        let executable = running.map_or_else(
            || declaration.exec_paths.first().cloned().unwrap_or_default(),
            |image| image.path,
        );
        members.push(ProductProcess {
            kind: ProductProcessKind::Systemd(declaration.unit),
            main_pid,
            executable,
            status,
            active,
        });
    }
    Ok(ReleaseFleet {
        selected_release,
        members,
    })
}

/// Is this running image the program the selected release declares?
///
/// Reuses [`running_install_status`] — the crate's one definition of the four
/// outcomes — with the two entries derived from the declaration. No second
/// comparison of "is this the install?" exists (SPEC DT-2).
fn classify_executable(
    running_executable: &ObservedExecutable,
    declaration: &UnitDeclaration,
) -> RunningInstallStatus {
    let Some(declared) = declaration.exec_paths.first() else {
        return RunningInstallStatus::Unknown;
    };
    let (direct_install, active_release) = install_entries_for_program(declared);
    let origin = if running_executable.path.as_os_str().is_empty() {
        ProcessInstallOrigin::Unknown
    } else {
        process_install_origin_for_path(&running_executable.path, &direct_install, &active_release)
    };
    let status = running_install_status_from_metadata(
        Some(&running_executable.metadata),
        origin,
        &direct_install,
        &active_release,
    );
    match status {
        RunningInstallStatus::InstalledDirect => {
            if running_executable.matches_target_bytes(&direct_install) {
                RunningInstallStatus::InstalledDirect
            } else {
                RunningInstallStatus::UpdatePendingDirect
            }
        }
        RunningInstallStatus::InstalledActiveRelease => {
            if running_executable.matches_target_bytes(&active_release) {
                RunningInstallStatus::InstalledActiveRelease
            } else {
                RunningInstallStatus::UpdatePendingActiveRelease
            }
        }
        RunningInstallStatus::InstalledBoth => {
            match (
                running_executable.matches_target_bytes(&direct_install),
                running_executable.matches_target_bytes(&active_release),
            ) {
                (true, true) => RunningInstallStatus::InstalledBoth,
                _ => RunningInstallStatus::UpdatePendingBoth,
            }
        }
        other => other,
    }
}

#[cfg(test)]
mod tests;
