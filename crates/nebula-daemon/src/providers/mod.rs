//! Provider adapters for work trackers (Jira) and source-control hosts
//! (Bitbucket, later). A provider is the deterministic sync surface behind the
//! board — distinct from the MCP tools an agent uses (PRD §9). Everything the
//! daemon needs from a tracker is the [`Tracker`] trait; the concrete adapters
//! (`jira`) live in submodules, and a [`FakeTracker`] backs the tests and the
//! `make dev` instance without touching the network.
//!
//! Outbound HTTP is `reqwest` with `rustls-tls` (execution-plan D2): no
//! OpenSSL, so the musl release targets stay buildable. Only this module tree
//! makes network calls.

pub mod jira;

use anyhow::Result;
use async_trait::async_trait;
use nebula_core::ext::Ticket;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Non-secret configuration for one provider connection, from `config.json`'s
/// `connections` array. The token is never here (execution-plan D7) — it is
/// supplied separately as a [`Secret`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ConnectionConfig {
    /// Stable id used everywhere as the connection half of a `TicketId` and as
    /// the secrets-map key. Never renamed once tickets reference it.
    pub id: String,
    /// `jira` or `bitbucket`.
    pub kind: String,
    /// Human label for the connection strip.
    pub label: String,
    /// Site base URL, e.g. `https://acme.atlassian.net`.
    pub base_url: String,
    /// The account the token belongs to (Jira: the user's email).
    pub account: String,
    /// The JQL that scopes the assignment set. Empty means the built-in
    /// "assigned to me, not done" default the adapter supplies.
    pub jql: String,
    /// A Jira board id whose rank orders the tickets, when set. Empty falls
    /// back to configured priority order (PRD §7).
    pub board_id: String,
    /// The git repo a ticket's work is done in — the path of a registered
    /// nebula project. Starting a ticket cuts a worktree here and launches the
    /// agent in it. Empty means tickets on this connection can be read but not
    /// started (a full per-key repo mapping is a later slice; this is the
    /// single-repo default).
    pub repo: String,
    /// The branch new ticket worktrees start from. Empty means the repo's
    /// configured default (origin's HEAD), like the rest of nebula.
    pub base_branch: String,
}

impl ConnectionConfig {
    pub fn is_jira(&self) -> bool {
        self.kind.eq_ignore_ascii_case("jira")
    }

    /// The testing/demo connection: no token, built-in tickets.
    pub fn is_fake(&self) -> bool {
        self.kind.eq_ignore_ascii_case("fake")
    }
}

/// The secret half of a connection, kept only in `config.local.json` (never
/// exported or bundled) or an env override. A blank token means the connection
/// is `configured` but not `authenticated`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Secret {
    /// Jira API token / Bitbucket app password.
    pub token: String,
}

/// A work tracker (Jira today). Deterministic assigned-ticket sync — never an
/// LLM guessing the queue (PRD §9). Object-safe via `async_trait` so the sync
/// engine can hold a heterogeneous set of connections.
#[async_trait]
pub trait Tracker: Send + Sync {
    /// The connection id these tickets are keyed under.
    fn connection_id(&self) -> &str;

    /// Prove the credential authenticates (the `authenticated` health step).
    /// Cheap: a whoami-style call, not a full search.
    async fn verify(&self) -> Result<()>;

    /// The complete current assignment set, fully paginated. Each ticket is
    /// returned with its `id.connection` already set to this connection's id.
    /// A page-fetch failure is an `Err` — the sync engine then keeps the
    /// last-known data rather than presuming anything deleted (PRD §8).
    async fn fetch_assigned(&self) -> Result<Vec<Ticket>>;
}

/// Build the tracker for a connection, or `None` when the kind has no adapter
/// yet (Bitbucket is an `Scm`, not a `Tracker`; an unknown kind is skipped).
///
/// The `fake` kind is a testing/demo connection: it needs no token and returns
/// a built-in set of demo tickets, so the board is demoable without a real
/// Jira. Add `{ "id": "demo", "kind": "fake", "label": "Demo" }` to
/// `config.json`'s `connections`.
pub fn build_tracker(config: &ConnectionConfig, secret: &Secret) -> Option<Box<dyn Tracker>> {
    if config.is_jira() {
        Some(Box::new(jira::JiraTracker::new(
            config.clone(),
            secret.clone(),
        )))
    } else if config.is_fake() {
        Some(Box::new(FakeTracker::new(&config.id, demo_tickets())))
    } else {
        None
    }
}

/// A small, self-contained set of demo tickets for the `fake` connection —
/// enough to show every board affordance: several statuses, a priority spread,
/// components/labels, and one ticket blocked by another so the dependency
/// gating and blocked-reason are visible. Keys and content are invented; no
/// network is touched.
pub fn demo_tickets() -> Vec<Ticket> {
    use nebula_core::ext::{SourceCategory, TicketDep, TicketFields, TicketId};

    let mk = |native: &str,
              key: &str,
              summary: &str,
              cat: SourceCategory,
              priority: &str,
              rank: f64,
              desc: &str,
              ac: Option<&str>,
              components: &[&str],
              labels: &[&str]|
     -> Ticket {
        Ticket {
            id: TicketId::new("", native),
            key: key.into(),
            summary: summary.into(),
            brief: summary.into(),
            status_name: match cat {
                SourceCategory::ToDo => "To Do",
                SourceCategory::InProgress => "In Progress",
                SourceCategory::Done => "Done",
                SourceCategory::Unknown => "Unknown",
            }
            .into(),
            status_category: cat,
            assignee: Some("Ameer Dheyaa".into()),
            priority: Some(priority.into()),
            rank: Some(rank),
            created_at: Some("2026-09-10T09:00:00.000+0000".into()),
            updated_at: Some("2026-09-17T14:00:00.000+0000".into()),
            url: Some(format!("https://demo.atlassian.net/browse/{key}")),
            fields: TicketFields {
                description: Some(desc.into()),
                acceptance_criteria: ac.map(str::to_string),
                issue_type: Some("Task".into()),
                components: components.iter().map(|s| s.to_string()).collect(),
                labels: labels.iter().map(|s| s.to_string()).collect(),
            },
            ..Default::default()
        }
    };

    let mut blocked = mk(
        "10004",
        "AQ-1069",
        "Wire the board's start action to the run engine",
        SourceCategory::ToDo,
        "High",
        4.0,
        "Pressing s on a card should provision a worktree and launch an agent on the ticket.",
        Some("- s starts a run\n- the card shows Running\n- the agent works in a fresh worktree"),
        &["daemon", "tui"],
        &["native-migration"],
    );
    blocked.deps.push(TicketDep {
        target_native: "10001".into(),
        kind: "Blocks".into(),
        blocks_this: true,
        waiver: None,
    });

    vec![
        mk(
            "10001",
            "AQ-1066",
            "Sync assigned Jira tickets into a board",
            SourceCategory::InProgress,
            "High",
            1.0,
            "The daemon should pull the tickets assigned to me and show them as cards.",
            Some("- tickets appear as cards\n- status and priority are shown\n- a failed sync keeps last-known data"),
            &["daemon"],
            &["native-migration"],
        ),
        mk(
            "10002",
            "AQ-1067",
            "Show a plain-language brief on each card",
            SourceCategory::ToDo,
            "Medium",
            2.0,
            "Opening a card should show the ticket's description and acceptance criteria, formatted deterministically.",
            None,
            &["tui"],
            &[],
        ),
        mk(
            "10003",
            "AQ-1068",
            "Order the board by what can actually start",
            SourceCategory::ToDo,
            "Low",
            3.0,
            "Eligible work should lead; blocked tickets sort after and say why.",
            Some("- eligible before blocked\n- blocker reason on the card"),
            &["daemon"],
            &[],
        ),
        blocked,
    ]
}

// ---------------------------------------------------------------------------
// Fake tracker — fixture-backed, no network. Backs unit/e2e tests and the
// `make dev` instance so the board is demonstrable offline.
// ---------------------------------------------------------------------------

/// A tracker that returns a fixed set of tickets from memory. Set
/// `verify_ok = false` to exercise the auth-failure path; set `fail_fetch` to
/// exercise the "keep last-known data on a failed beat" rule.
#[derive(Debug, Clone, Default)]
pub struct FakeTracker {
    pub connection: String,
    pub tickets: Vec<Ticket>,
    pub verify_ok: bool,
    pub fail_fetch: bool,
}

impl FakeTracker {
    pub fn new(connection: impl Into<String>, tickets: Vec<Ticket>) -> Self {
        Self {
            connection: connection.into(),
            tickets,
            verify_ok: true,
            fail_fetch: false,
        }
    }
}

#[async_trait]
impl Tracker for FakeTracker {
    fn connection_id(&self) -> &str {
        &self.connection
    }

    async fn verify(&self) -> Result<()> {
        if self.verify_ok {
            Ok(())
        } else {
            anyhow::bail!("fake: authentication rejected")
        }
    }

    async fn fetch_assigned(&self) -> Result<Vec<Ticket>> {
        if self.fail_fetch {
            anyhow::bail!("fake: page fetch failed");
        }
        let mut out = self.tickets.clone();
        for t in &mut out {
            t.id.connection = self.connection.clone();
        }
        Ok(out)
    }
}

/// The whole connection roster the daemon knows: non-secret config joined to
/// its secret, resolved once per sync beat from settings + env.
#[derive(Debug, Clone, Default)]
pub struct Connections {
    pub configs: Vec<ConnectionConfig>,
    pub secrets: BTreeMap<String, Secret>,
}

impl Connections {
    pub fn secret_for(&self, id: &str) -> Secret {
        self.secrets.get(id).cloned().unwrap_or_default()
    }
}
