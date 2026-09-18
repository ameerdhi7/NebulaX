//! The Jira-agentic feature's wire vocabulary.
//!
//! Everything the ticket board, workflow engine, evidence, inbox and reports
//! send over the socket rides inside the two `Ext { kind, json }` protocol
//! variants (see `protocol.rs`) as a JSON payload, rather than as its own
//! positional-msgpack variant. That is a deliberate trade (execution plan D1):
//! positional msgpack makes every typed enum variant a wire break and a
//! `PROTOCOL_VERSION` bump, and this feature would otherwise bump it dozens of
//! times during development, stranding every running daemon. One bump (40→41)
//! adds the envelope; all later message evolution happens in JSON here.
//!
//! Because of that, every type in this module must stay **lenient**: all fields
//! `#[serde(default)]`, no `deny_unknown_fields`, so a daemon and a client from
//! different builds still talk — a field one side doesn't know is ignored, a
//! field it expects and doesn't get takes its default. This mirrors the hook
//! receiver's `HookPayload` tolerance and the settings layer's per-key leniency.

use serde::{Deserialize, Serialize};

/// Namespaced `kind` strings for the `Ext` envelope. The daemon routes an
/// inbound `ClientRequest::Ext` on these; clients match an inbound
/// `ServerEvent::Ext` on them and ignore any they don't know.
pub mod kinds {
    // -- daemon → client (state the daemon owns) --
    /// Full ticket + connection state, sent once right after `Subscribe`.
    pub const TICKETS_SNAPSHOT: &str = "tickets/snapshot";
    /// One ticket changed (synced, started, blocked, removed).
    pub const TICKETS_UPSERT: &str = "tickets/upsert";
    /// A tracked ticket left the assignment scope (moved to Done, reassigned,
    /// unfiltered) — carried with a reason rather than vanishing.
    pub const TICKETS_REMOVED: &str = "tickets/removed";
    /// A provider connection's health changed (`configured`/`authenticated`/
    /// `verified`, or an error with the last-known age).
    pub const CONNECTIONS_STATUS: &str = "connections/status";
    /// One workflow run changed state.
    pub const RUNS_UPSERT: &str = "runs/upsert";
    /// One piece of quality evidence changed.
    pub const EVIDENCE_UPSERT: &str = "evidence/upsert";
    /// One durable inbox event (needs-input, readiness, PR activity, …).
    pub const INBOX_EVENT: &str = "inbox/event";
    /// A daily/weekly report the daemon computed.
    pub const REPORTS_SUMMARY: &str = "reports/summary";
    /// Ask the daemon to compute a fresh report (client→daemon).
    pub const REPORTS_REQUEST: &str = "reports/request";

    // -- client → daemon (actions on the board) --
    /// Start a finite batch of tickets under a workflow policy.
    pub const BOARD_START: &str = "board/start";
    /// Pause a run's queue (blocks new stage dispatch; does not stop a live
    /// process).
    pub const BOARD_PAUSE: &str = "board/pause";
    /// Cancel a run (terminates it; keeps its worktree and history).
    pub const BOARD_CANCEL: &str = "board/cancel";
    /// Launch an independent review agent on a ticket's run (F4.2).
    pub const BOARD_REVIEW: &str = "board/review";
    /// Retry a failed run as a new attempt, retaining the failed one.
    pub const BOARD_RETRY: &str = "board/retry";
    /// Mark an inbox event read locally (never resolves a provider thread).
    pub const INBOX_MARK_READ: &str = "inbox/mark-read";
    /// Force an immediate provider sync (the board's `R`-equivalent).
    pub const BOARD_SYNC_NOW: &str = "board/sync-now";
    /// Ask the daemon for the base→result diff of a ticket's run (client→daemon).
    pub const BOARD_CHANGES: &str = "board/changes";
    /// The diff the daemon captured for a ticket's run (daemon→client reply).
    pub const BOARD_CHANGES_RESULT: &str = "board/changes-result";

    // -- agent → daemon (the `nebula stage` one-shot verb, F2.3) --
    /// A managed agent reporting a stage result + requirement disposition.
    pub const STAGE_REPORT: &str = "stage/report";
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// A ticket's persistent identity: a connection id plus the provider's own
/// native issue id. The human key ("AQ-123") is display text and can change
/// or collide across sites; this cannot. (PRD §9.)
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct TicketId {
    pub connection: String,
    pub native: String,
}

impl TicketId {
    pub fn new(connection: impl Into<String>, native: impl Into<String>) -> Self {
        Self {
            connection: connection.into(),
            native: native.into(),
        }
    }

    /// A stable flat string for map keys and the store's unique index.
    pub fn flat(&self) -> String {
        format!("{}\u{1}{}", self.connection, self.native)
    }
}

// ---------------------------------------------------------------------------
// State vocabularies (PRD §5, §6) — four independent kinds of state.
// ---------------------------------------------------------------------------

/// Where a ticket sits in the tracker's own workflow. The provider's exact
/// status *name* is preserved separately (`Ticket::status_name`); this is the
/// coarse category the board colours and filters by, and the only part nebula
/// interprets. Jira's status category maps onto these three plus Unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SourceCategory {
    ToDo,
    InProgress,
    Done,
    #[default]
    Unknown,
}

/// Nebula's own workflow state for a run — distinct from the tracker's status
/// and never invented from it. `Running` carries the current stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowState {
    #[default]
    Queued,
    Running,
    NeedsInput,
    Blocked,
    Paused,
    Ready,
    Failed,
    Cancelled,
    /// A daemon restart could not prove the run's liveness. Never silently
    /// resumed; resume/retry is explicit (PRD §8).
    Interrupted,
}

/// The stage a `Running` run is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    #[default]
    Prepare,
    Implement,
    Verify,
    Review,
    Revise,
}

/// A pull request's delivery standing, kept separate from workflow and source
/// status. `ChangesRequested` is only used where the provider reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    #[default]
    NoPr,
    Open,
    ChangesRequested,
    Merged,
    Declined,
}

/// The state of one piece of quality evidence. `skipped` is not `passed`, and
/// a command that exited 0 having discovered no tests is `skipped`, never
/// `passed` (PRD §6). `stale` means the code moved under a previously good
/// result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceState {
    #[default]
    NotRun,
    Running,
    Passed,
    Failed,
    Skipped,
    Unavailable,
    Stale,
}

/// What kind of evidence a record is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    #[default]
    Implementation,
    Checks,
    Review,
    HumanAccepted,
}

/// A provider connection's health. A config file alone earns only `configured`;
/// a token that authenticates earns `authenticated`; a required tool actually
/// reachable earns `verified` (PRD §9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionHealth {
    #[default]
    Configured,
    Authenticated,
    Verified,
    Error,
}

// ---------------------------------------------------------------------------
// Ticket
// ---------------------------------------------------------------------------

/// One dependency edge declared on a ticket. Only explicit blocks-links create
/// execution dependencies; "relates to" and parent-child do not (PRD §7).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TicketDep {
    /// The native id of the ticket this one depends on.
    #[serde(default)]
    pub target_native: String,
    /// The provider's link-type name ("Blocks", "is blocked by", …).
    #[serde(default)]
    pub kind: String,
    /// True when this ticket is the *blocked* side (must wait for the target).
    #[serde(default)]
    pub blocks_this: bool,
    /// A recorded waiver reason, when the user deliberately waived the edge.
    #[serde(default)]
    pub waiver: Option<String>,
}

/// A ticket as the board shows it. Everything a card needs (PRD §4) plus the
/// pointers the detail views expand. Lenient: an older client drops fields it
/// doesn't know; a newer daemon fills the ones it has.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Ticket {
    pub id: TicketId,
    /// The human key, "AQ-123". Display text (PRD §9).
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub summary: String,
    /// A short, deterministic plain-language brief built from source fields.
    /// The optional AI simplification (later) prepends to this, never replaces
    /// the source requirements below.
    #[serde(default)]
    pub brief: String,
    /// The provider's exact status name, preserved verbatim ("In Progress").
    #[serde(default)]
    pub status_name: String,
    /// The coarse category nebula interprets.
    #[serde(default)]
    pub status_category: SourceCategory,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub priority: Option<String>,
    /// Board rank where the provider supplies one; lower sorts first. `None`
    /// falls back to configured priority, then creation order (PRD §7).
    #[serde(default)]
    pub rank: Option<f64>,
    /// Provider create/update timestamps, RFC-3339. Update drives staleness of
    /// a generated brief and re-acknowledgement of requirement changes.
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    /// The ticket's page in the tracker.
    #[serde(default)]
    pub url: Option<String>,
    /// The full source requirement fields, deterministically formatted for the
    /// Brief detail tab. Missing fields read "Not specified" (PRD §4).
    #[serde(default)]
    pub fields: TicketFields,
    #[serde(default)]
    pub deps: Vec<TicketDep>,
    #[serde(default)]
    pub design_links: Vec<String>,
    /// Why this ticket cannot start right now, or `None` when it is eligible.
    /// Computed daemon-side by the eligibility engine (PRD §7).
    #[serde(default)]
    pub blocked_reason: Option<String>,
    /// The current run's workflow state, when work has started.
    #[serde(default)]
    pub workflow: Option<WorkflowState>,
    /// The stage of a `Running` workflow.
    #[serde(default)]
    pub stage: Option<Stage>,
    /// Delivery standing of the linked PR(s).
    #[serde(default)]
    pub delivery: Delivery,
    /// Quality badges for the current revision, newest evidence per kind.
    #[serde(default)]
    pub evidence: Vec<EvidenceBadge>,
    /// Unread PR activity count on the ticket's linked PRs.
    #[serde(default)]
    pub unread_pr_activity: u32,
    /// Epoch ms of the last successful sync that touched this ticket.
    #[serde(default)]
    pub last_synced_ms: i64,
    /// Set when the ticket has left the assignment scope but is retained for
    /// explanation rather than deleted (PRD §8).
    #[serde(default)]
    pub removed_reason: Option<String>,
}

/// The launch context persisted on a ticket's agent row, so every cold spawn
/// and resume rebuilds the same ticket rule (the analogue of the PR/issue URL).
/// Just enough to name the ticket in the rule — the rest the agent reads from
/// the tracker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TicketRef {
    pub connection: String,
    pub native: String,
    pub key: String,
    pub summary: String,
}

/// Source requirement fields, formatted for the Brief tab. A missing field is
/// `None` and renders as "Not specified" — never invented (PRD §4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TicketFields {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub acceptance_criteria: Option<String>,
    #[serde(default)]
    pub issue_type: Option<String>,
    #[serde(default)]
    pub components: Vec<String>,
    #[serde(default)]
    pub labels: Vec<String>,
}

/// A compact quality badge for a card: the kind, its state, and the revision
/// it was assessed against (short commit) so a card can show "reviewed at
/// abc1234". The Quality tab expands these into full evidence records.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct EvidenceBadge {
    #[serde(default)]
    pub kind: EvidenceKind,
    #[serde(default)]
    pub state: EvidenceState,
    /// Short commit/tree the result was assessed against.
    #[serde(default)]
    pub revision: Option<String>,
    /// Reviewer/model or check-suite label, for the badge's subtitle.
    #[serde(default)]
    pub label: Option<String>,
}

// ---------------------------------------------------------------------------
// Connections
// ---------------------------------------------------------------------------

/// A provider connection's identity and health, for the board's connection
/// strip. Never carries the secret — only whether one is present and working.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ConnectionStatus {
    pub id: String,
    #[serde(default)]
    pub label: String,
    /// `jira` or `bitbucket`.
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub health: ConnectionHealth,
    /// A human message for the `Error` health (never a token or secret).
    #[serde(default)]
    pub detail: Option<String>,
    /// Epoch ms of the last successful sync on this connection; 0 = never.
    #[serde(default)]
    pub last_sync_ms: i64,
}

// ---------------------------------------------------------------------------
// Snapshot + delta payloads (daemon → client)
// ---------------------------------------------------------------------------

/// The full ticket-side state, sent once after `Subscribe` as a
/// `TICKETS_SNAPSHOT` ext event — the analogue of the entity `Snapshot`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TicketsSnapshot {
    #[serde(default)]
    pub tickets: Vec<Ticket>,
    #[serde(default)]
    pub connections: Vec<ConnectionStatus>,
    /// The durable inbox as of connect; deltas follow as `inbox/event`.
    #[serde(default)]
    pub inbox: Vec<InboxEvent>,
}

/// A single-ticket delta (`TICKETS_UPSERT`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TicketUpsert {
    pub ticket: Ticket,
}

/// A ticket-removed delta (`TICKETS_REMOVED`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TicketRemoved {
    pub id: TicketId,
    #[serde(default)]
    pub reason: String,
}

// ---------------------------------------------------------------------------
// Action payloads (client → daemon)
// ---------------------------------------------------------------------------

/// A workflow policy: what a run must satisfy to be called Ready. The label is
/// always shown with a Ready badge so "Ready" never lies about what it checked
/// (PRD §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Policy {
    /// Ready on a captured implementation alone; checks and review "not run".
    ImplementationOnly,
    /// Ready requires configured checks to pass and an independent review.
    #[default]
    Checked,
}

impl Policy {
    pub fn label(&self) -> &'static str {
        match self {
            Policy::ImplementationOnly => "implementation-only",
            Policy::Checked => "checked",
        }
    }
}

/// `BOARD_START`: start a finite batch under a policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct BoardStart {
    /// The tickets to start, by native id, in the user's chosen order. The
    /// daemon dispatches only the eligible ones and holds the rest with a
    /// reason.
    #[serde(default)]
    pub tickets: Vec<TicketId>,
    #[serde(default)]
    pub policy: Policy,
}

/// `BOARD_PAUSE`/`BOARD_CANCEL`/`BOARD_RETRY`/`BOARD_SYNC_NOW` all carry just a
/// ticket target (sync-now leaves it empty to mean "all connections").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TicketTarget {
    #[serde(default)]
    pub ticket: Option<TicketId>,
}

/// `INBOX_MARK_READ`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct InboxMarkRead {
    #[serde(default)]
    pub dedupe_key: String,
}

/// `BOARD_CHANGES`: ask for one ticket's run diff.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ChangesRequest {
    pub ticket: TicketId,
}

/// `BOARD_CHANGES_RESULT`: the base→result diff of a ticket's run, captured
/// on demand from its worktree (empty when there is no run or no diff).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ChangesResult {
    pub ticket: TicketId,
    #[serde(default)]
    pub diff: String,
    /// A human note when there is nothing to show (no run, worktree gone).
    #[serde(default)]
    pub note: Option<String>,
}

/// How an agent's `nebula stage` reports the outcome of the turn it just
/// finished. Deliberately explicit — the workflow never infers completion from
/// `AgentStatus::Finished` (PRD §12), so a run only reaches Ready when the
/// agent says `done`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StageStatus {
    /// Implementation is complete; the daemon captures the diff as evidence
    /// and the run becomes Ready under its policy.
    #[default]
    Done,
    /// The agent needs a decision from the user before it can continue.
    NeedsInput,
    /// The agent could not complete the work.
    Failed,
    /// The work is blocked by something outside this session.
    Blocked,
}

impl StageStatus {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "done" => StageStatus::Done,
            "needs-input" | "needs_input" => StageStatus::NeedsInput,
            "failed" => StageStatus::Failed,
            "blocked" => StageStatus::Blocked,
            _ => return None,
        })
    }
}

/// `STAGE_REPORT`: an agent, via the `nebula stage` one-shot, reporting its
/// stage outcome. `agent_id` comes from `NEBULA_AGENT_ID`; the daemon maps it
/// to the run and captures the evidence itself rather than trusting a paste.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct StageReport {
    #[serde(default)]
    pub agent_id: String,
    #[serde(default)]
    pub status: StageStatus,
    #[serde(default)]
    pub summary: String,
}

// ---------------------------------------------------------------------------
// Inbox
// ---------------------------------------------------------------------------

/// A durable inbox event (`INBOX_EVENT`). `dedupe_key` is stable across retries
/// and clients so the same real-world change never lands twice (PRD §10).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct InboxEvent {
    pub dedupe_key: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub ticket: Option<TicketId>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub created_ms: i64,
    #[serde(default)]
    pub read_ms: Option<i64>,
}

/// A point-in-time progress report (PRD §11, `REPORTS_SUMMARY`). First cut is a
/// snapshot of the current cohort — counts by workflow and source state, and
/// evidence coverage — plus the attention total. Period-based throughput
/// (tickets reaching Ready / merged during a window) is a later slice; this
/// covers observed current state only, which is honest about what it counts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ReportSummary {
    #[serde(default)]
    pub generated_ms: i64,
    /// Total tracked tickets (excludes those that left the assignment scope).
    #[serde(default)]
    pub total: usize,
    // Workflow-state cohort (runs that have started).
    #[serde(default)]
    pub queued: usize,
    #[serde(default)]
    pub running: usize,
    #[serde(default)]
    pub needs_input: usize,
    #[serde(default)]
    pub ready: usize,
    #[serde(default)]
    pub failed: usize,
    /// Tickets eligible but not yet started (no run).
    #[serde(default)]
    pub not_started: usize,
    /// Tickets held by a dependency or out-of-scope reason.
    #[serde(default)]
    pub blocked: usize,
    // Source (Jira) status cohort.
    #[serde(default)]
    pub source_todo: usize,
    #[serde(default)]
    pub source_in_progress: usize,
    #[serde(default)]
    pub source_done: usize,
    // Evidence coverage over current runs.
    #[serde(default)]
    pub implementation_captured: usize,
    #[serde(default)]
    pub checks_passed: usize,
    #[serde(default)]
    pub review_passed: usize,
    /// Tickets needing the user (needs-input/failed) plus unread inbox events.
    #[serde(default)]
    pub attention: usize,
}

// ---------------------------------------------------------------------------
// Encode/decode helpers so callers never hand-roll serde_json at each site.
// ---------------------------------------------------------------------------

/// Serialize a payload to the JSON bytes an `Ext` envelope carries.
pub fn encode<T: Serialize>(value: &T) -> Vec<u8> {
    serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec())
}

/// Parse an `Ext` envelope's JSON bytes, lenient: a malformed or
/// wrong-shape body degrades to the type's default rather than erroring, so a
/// build-skew payload never kills a connection.
pub fn decode<T: Default + for<'de> Deserialize<'de>>(json: &[u8]) -> T {
    serde_json::from_slice(json).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticket_id_flat_is_unambiguous() {
        let a = TicketId::new("conn-a", "10001");
        let b = TicketId::new("conn-a", "10002");
        let c = TicketId::new("conn-b", "10001");
        assert_ne!(a.flat(), b.flat());
        assert_ne!(a.flat(), c.flat());
    }

    /// A newer daemon's ticket JSON must still decode on an older client that
    /// doesn't know some fields — unknown fields ignored, missing ones default.
    #[test]
    fn ticket_decode_is_lenient_across_skew() {
        let json = br#"{
            "id": {"connection": "c1", "native": "42"},
            "key": "AQ-42",
            "summary": "Preserve the selected date",
            "status_category": "in_progress",
            "a_field_from_the_future": {"nested": true},
            "rank": 12.5
        }"#;
        let t: Ticket = decode(json);
        assert_eq!(t.key, "AQ-42");
        assert_eq!(t.status_category, SourceCategory::InProgress);
        assert_eq!(t.rank, Some(12.5));
        // A field the payload omitted takes its default.
        assert_eq!(t.delivery, Delivery::NoPr);
        assert_eq!(t.workflow, None);
    }

    #[test]
    fn snapshot_roundtrips_through_ext_bytes() {
        let snap = TicketsSnapshot {
            tickets: vec![Ticket {
                id: TicketId::new("c1", "42"),
                key: "AQ-42".into(),
                summary: "x".into(),
                status_category: SourceCategory::ToDo,
                ..Default::default()
            }],
            connections: vec![ConnectionStatus {
                id: "c1".into(),
                label: "AAU Jira".into(),
                kind: "jira".into(),
                health: ConnectionHealth::Verified,
                ..Default::default()
            }],
            ..Default::default()
        };
        let bytes = encode(&snap);
        let back: TicketsSnapshot = decode(&bytes);
        assert_eq!(snap, back);
    }

    #[test]
    fn state_enums_serialize_snake_case() {
        assert_eq!(
            serde_json::to_string(&WorkflowState::NeedsInput).unwrap(),
            "\"needs_input\""
        );
        assert_eq!(
            serde_json::to_string(&EvidenceState::Stale).unwrap(),
            "\"stale\""
        );
        assert_eq!(Policy::Checked.label(), "checked");
    }
}
