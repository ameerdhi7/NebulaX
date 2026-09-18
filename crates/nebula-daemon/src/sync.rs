//! Provider sync: the daemon-side beat that keeps the ticket board current
//! whether or not any client is open (PRD §8). One beat per connection —
//! verify the credential, fetch the whole assignment scope, reconcile it
//! against the store, annotate eligibility, and broadcast the deltas.
//!
//! The rules that matter (PRD §8):
//! - A failed fetch keeps the last-known data and stamps the connection with an
//!   error and an age — it never presumes a ticket deleted.
//! - A ticket that was tracked and is now absent from a *successful* fetch is
//!   resolved individually with a `removed_reason`, not silently dropped.
//! - Writes are batched per beat against the single `Mutex<Connection>` store.

use crate::providers::{self, Connections, Tracker};
use crate::store::Store;
use nebula_core::ext::{self, kinds, ConnectionHealth, ConnectionStatus, Ticket, TicketId};
use nebula_core::ServerEvent;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::broadcast;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// One sync beat over every configured connection. Broadcasts a
/// `tickets/upsert` per changed ticket and a `connections/status` per
/// connection, and returns how many tickets it upserted (for the caller's
/// logging and the e2e tests' assertions).
pub async fn sync_once(
    store: &Arc<Store>,
    events: &broadcast::Sender<ServerEvent>,
    roster: &Connections,
    priority_order: &[String],
) -> usize {
    let mut upserted = 0;
    for cfg in &roster.configs {
        let secret = roster.secret_for(&cfg.id);
        let Some(tracker) = providers::build_tracker(cfg, &secret) else {
            continue;
        };
        upserted += sync_connection(store, events, cfg, &*tracker, &secret, priority_order).await;
    }
    upserted
}

async fn sync_connection(
    store: &Arc<Store>,
    events: &broadcast::Sender<ServerEvent>,
    cfg: &providers::ConnectionConfig,
    tracker: &dyn Tracker,
    secret: &providers::Secret,
    priority_order: &[String],
) -> usize {
    // The fake/demo connection needs no credential — it stands in for a real
    // tracker in tests and offline demos.
    let needs_token = !cfg.is_fake();

    // Health starts from the credential's presence, then the verify probe.
    let mut status = ConnectionStatus {
        id: cfg.id.clone(),
        label: cfg.label.clone(),
        kind: cfg.kind.clone(),
        health: if needs_token && secret.token.trim().is_empty() {
            ConnectionHealth::Configured
        } else {
            ConnectionHealth::Authenticated
        },
        detail: None,
        last_sync_ms: 0,
    };
    if needs_token && secret.token.trim().is_empty() {
        status.detail = Some("no API token configured".into());
        publish_connection(store, events, &status);
        return 0;
    }
    if let Err(e) = tracker.verify().await {
        status.health = ConnectionHealth::Error;
        status.detail = Some(format!("authentication failed: {e}"));
        publish_connection(store, events, &status);
        return 0;
    }

    // Fetch the whole assignment scope. A failure preserves last-known data.
    let fetched = match tracker.fetch_assigned().await {
        Ok(t) => t,
        Err(e) => {
            status.health = ConnectionHealth::Error;
            status.detail = Some(format!("sync failed, showing last known data: {e}"));
            // Keep the previous last_sync_ms (the age of the good data).
            if let Ok(conns) = store.load_connections() {
                if let Some(prev) = conns.iter().find(|c| c.id == cfg.id) {
                    status.last_sync_ms = prev.last_sync_ms;
                }
            }
            publish_connection(store, events, &status);
            return 0;
        }
    };

    let beat = now_ms();
    let mut fetched = fetched;
    let present: HashSet<String> = fetched.iter().map(|t| t.id.native.clone()).collect();

    // Eligibility over the freshly fetched set (dependencies are within a
    // connection). Stamp the sync age on each.
    for t in &mut fetched {
        t.id.connection = cfg.id.clone();
        t.last_synced_ms = beat;
        t.removed_reason = None;
    }
    crate::eligibility::annotate_blocked_reasons(&mut fetched);
    // A ticket already under a run keeps showing that run's state across a
    // sync beat (the synced ticket alone carries no workflow).
    overlay_run_state(store, &mut fetched);

    // Reconcile: tickets tracked before this beat but absent now have left the
    // assignment scope — resolved individually with a reason, never dropped.
    let previously_tracked = store.tracked_ticket_natives(&cfg.id).unwrap_or_default();
    let mut removed: Vec<TicketId> = Vec::new();
    for native in previously_tracked {
        if !present.contains(&native) {
            removed.push(TicketId::new(&cfg.id, native));
        }
    }

    // Batch the writes for the beat (one lock acquisition per statement, but a
    // single pass — the store is one Mutex<Connection>).
    for t in &fetched {
        if let Err(e) = store.upsert_ticket(t) {
            tracing::warn!(error = %e, ticket = %t.key, "ticket upsert failed");
        }
    }
    for id in &removed {
        let reason = "no longer in the assignment scope";
        let _ = store.mark_ticket_removed(id, reason);
    }

    // Broadcast the deltas.
    for t in &fetched {
        broadcast_ticket(events, t);
    }
    for id in &removed {
        // Re-read so the client gets the full row with its reason, not just an id.
        if let Ok(mut all) = store.load_tickets() {
            if let Some(t) = all.iter_mut().find(|t| t.id == *id) {
                broadcast_ticket(events, t);
            }
        }
    }

    status.health = ConnectionHealth::Verified;
    status.detail = None;
    status.last_sync_ms = beat;
    publish_connection(store, events, &status);

    let _ = priority_order; // ordering is applied client-side from display_order; kept for parity
    fetched.len()
}

/// Overlay live workflow state onto tickets from their runs (execution-plan
/// D3: runs are their own table, joined onto the ticket for display, so a sync
/// beat overwriting the ticket's synced fields never wipes the fact that work
/// has started). The mapping is deliberately honest (D5): a stopped agent with
/// no `nebula stage` report becomes `NeedsInput` or `Interrupted`, never a fake
/// `Ready` — only a real completion report earns Ready, which is a later slice.
pub fn overlay_run_state(store: &Store, tickets: &mut [Ticket]) {
    use nebula_core::ext::{Stage, WorkflowState};
    use nebula_core::AgentStatus;

    let runs = match store.latest_runs() {
        Ok(r) => r,
        Err(_) => return,
    };
    if runs.is_empty() {
        return;
    }
    for ticket in tickets.iter_mut() {
        let Some(run) = runs.iter().find(|r| r.ticket == ticket.id) else {
            continue;
        };
        // The run's stored `state` is authoritative once the agent has
        // reported through `nebula stage`. Only a `running` run (no report yet)
        // derives its display state from the agent's liveness — and even then a
        // stopped agent with no report is NeedsInput/Interrupted, never Ready
        // (PRD §12 / execution-plan D5).
        let (workflow, stage) = match run.state.as_str() {
            "queued" => (WorkflowState::Queued, None),
            "ready" => (WorkflowState::Ready, None),
            "failed" => (WorkflowState::Failed, None),
            "needs_input" => (WorkflowState::NeedsInput, None),
            "cancelled" => (WorkflowState::Cancelled, None),
            "interrupted" => (WorkflowState::Interrupted, None),
            _ => match &run.agent_id {
                Some(aid) => match store.get_agent(aid) {
                    Ok(Some(agent)) => match agent.status {
                        AgentStatus::Running | AgentStatus::Fresh => {
                            (WorkflowState::Running, Some(Stage::Implement))
                        }
                        AgentStatus::NeedsFeedback => (WorkflowState::NeedsInput, None),
                        AgentStatus::Finished => (WorkflowState::NeedsInput, None),
                        AgentStatus::Terminated | AgentStatus::Disconnected => {
                            (WorkflowState::Interrupted, None)
                        }
                    },
                    _ => (WorkflowState::Interrupted, None),
                },
                None => (WorkflowState::Running, Some(Stage::Implement)),
            },
        };
        ticket.workflow = Some(workflow);
        ticket.stage = stage;
        // Staleness (F4.3 / PRD §6): evidence assessed at a revision the
        // worktree has since moved past is demoted to `stale` — the code
        // changed under it, so it no longer speaks for the current revision.
        let mut evidence = run.evidence.clone();
        if evidence.iter().any(|b| b.revision.is_some()) {
            if let Some(head) = run
                .agent_id
                .as_ref()
                .and_then(|aid| current_head(store, aid))
            {
                for badge in &mut evidence {
                    if let Some(rev) = &badge.revision {
                        if rev != &head
                            && !matches!(
                                badge.state,
                                nebula_core::ext::EvidenceState::Running
                                    | nebula_core::ext::EvidenceState::NotRun
                            )
                        {
                            badge.state = nebula_core::ext::EvidenceState::Stale;
                        }
                    }
                }
            }
        }
        ticket.evidence = evidence;
    }
}

/// The short HEAD of the worktree an agent runs in, for staleness checks.
fn current_head(store: &Store, agent_id: &nebula_core::AgentId) -> Option<String> {
    let agent = store.get_agent(agent_id).ok().flatten()?;
    let worktree = store.get_worktree(&agent.worktree_id).ok().flatten()?;
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(&worktree.path)
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let head = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!head.is_empty()).then_some(head)
}

fn broadcast_ticket(events: &broadcast::Sender<ServerEvent>, ticket: &Ticket) {
    let _ = events.send(ServerEvent::Ext {
        req_id: None,
        kind: kinds::TICKETS_UPSERT.into(),
        json: ext::encode(&ext::TicketUpsert {
            ticket: ticket.clone(),
        }),
    });
}

fn publish_connection(
    store: &Arc<Store>,
    events: &broadcast::Sender<ServerEvent>,
    status: &ConnectionStatus,
) {
    let _ = store.upsert_connection(status);
    let _ = events.send(ServerEvent::Ext {
        req_id: None,
        kind: kinds::CONNECTIONS_STATUS.into(),
        json: ext::encode(status),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{ConnectionConfig, FakeTracker, Secret};
    use nebula_core::ext::SourceCategory;

    fn store() -> Arc<Store> {
        Arc::new(Store::open_in_memory().unwrap())
    }

    fn ticket(native: &str) -> Ticket {
        Ticket {
            id: TicketId::new("", native),
            key: format!("AQ-{native}"),
            summary: "demo".into(),
            status_category: SourceCategory::ToDo,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_successful_beat_upserts_and_verifies() {
        let store = store();
        let (events, _rx) = broadcast::channel(64);
        let cfg = ConnectionConfig {
            id: "c1".into(),
            kind: "jira".into(),
            ..Default::default()
        };
        let tracker = FakeTracker::new("c1", vec![ticket("1"), ticket("2")]);
        let secret = Secret { token: "t".into() };
        let n = sync_connection(&store, &events, &cfg, &tracker, &secret, &[]).await;
        assert_eq!(n, 2);
        let stored = store.load_tickets().unwrap();
        assert_eq!(stored.len(), 2);
        let conns = store.load_connections().unwrap();
        assert_eq!(conns[0].health, ConnectionHealth::Verified);
        assert!(conns[0].last_sync_ms > 0);
    }

    #[tokio::test]
    async fn a_missing_token_stays_configured_not_verified() {
        let store = store();
        let (events, _rx) = broadcast::channel(64);
        let cfg = ConnectionConfig {
            id: "c1".into(),
            kind: "jira".into(),
            ..Default::default()
        };
        let tracker = FakeTracker::new("c1", vec![ticket("1")]);
        let n = sync_connection(&store, &events, &cfg, &tracker, &Secret::default(), &[]).await;
        assert_eq!(n, 0, "no token means no fetch");
        let conns = store.load_connections().unwrap();
        assert_eq!(conns[0].health, ConnectionHealth::Configured);
    }

    #[tokio::test]
    async fn a_failed_fetch_keeps_last_known_data() {
        let store = store();
        let (events, _rx) = broadcast::channel(64);
        let cfg = ConnectionConfig {
            id: "c1".into(),
            kind: "jira".into(),
            ..Default::default()
        };
        let secret = Secret { token: "t".into() };
        // First a good beat lands two tickets.
        let good = FakeTracker::new("c1", vec![ticket("1"), ticket("2")]);
        sync_connection(&store, &events, &cfg, &good, &secret, &[]).await;
        // Then a failing beat must not delete them.
        let mut bad = FakeTracker::new("c1", vec![]);
        bad.fail_fetch = true;
        let n = sync_connection(&store, &events, &cfg, &bad, &secret, &[]).await;
        assert_eq!(n, 0);
        assert_eq!(
            store.load_tickets().unwrap().len(),
            2,
            "data survives a bad beat"
        );
        let conns = store.load_connections().unwrap();
        assert_eq!(conns[0].health, ConnectionHealth::Error);
    }

    #[test]
    fn overlay_maps_agent_status_to_honest_workflow_state() {
        use nebula_core::ext::{Stage, WorkflowState};
        use nebula_core::{
            Agent, AgentKind, AgentStatus, Project, ProjectId, Worktree, WorktreeId,
        };
        let store = store();
        let project = Project {
            workspace_id: Default::default(),
            id: ProjectId::generate(),
            name: "demo".into(),
            repo_path: "/tmp/demo".into(),
            sort_order: 0,
        };
        store.insert_project(&project).unwrap();
        let worktree = Worktree {
            id: WorktreeId::generate(),
            project_id: project.id.clone(),
            path: "/tmp/demo".into(),
            branch: "feat/aq-1".into(),
            is_main: false,
            sort_order: 0,
        };
        store.insert_worktree(&worktree).unwrap();
        let mut mk_agent = |status: AgentStatus| {
            let a = Agent {
                id: nebula_core::AgentId::generate(),
                worktree_id: worktree.id.clone(),
                name: "AQ-1".into(),
                status,
                archived: false,
                archived_at: 0,
                unseen: false,
                kind: AgentKind::Claude,
                custom_harness: None,
                model: None,
                effort: None,
                session_id: None,
                cloud_session_id: None,
                sort_order: 0,
                status_changed_at: 0,
                alive: false,
                recent_prompts: Vec::new(),
            };
            store.insert_agent(&a).unwrap();
            a.id
        };

        // A running agent → the card shows Running/Implement.
        let running = mk_agent(AgentStatus::Running);
        let t_running = TicketId::new("c1", "1");
        store.upsert_ticket(&ticket("1")).unwrap();
        // upsert stamps connection empty; re-key to c1 for the run join.
        let mut t1 = ticket("1");
        t1.id = t_running.clone();
        store.upsert_ticket(&t1).unwrap();
        store.start_run(&t_running, &running).unwrap();

        // A finished agent → Needs input, NOT Ready (no completion report).
        let finished = mk_agent(AgentStatus::Finished);
        let t_finished = TicketId::new("c1", "2");
        let mut t2 = ticket("2");
        t2.id = t_finished.clone();
        store.upsert_ticket(&t2).unwrap();
        store.start_run(&t_finished, &finished).unwrap();

        let mut tickets = store.load_tickets().unwrap();
        overlay_run_state(&store, &mut tickets);
        let r = tickets.iter().find(|t| t.id == t_running).unwrap();
        assert_eq!(r.workflow, Some(WorkflowState::Running));
        assert_eq!(r.stage, Some(Stage::Implement));
        let f = tickets.iter().find(|t| t.id == t_finished).unwrap();
        assert_eq!(
            f.workflow,
            Some(WorkflowState::NeedsInput),
            "a finished turn with no stage report is not Ready"
        );
    }

    #[test]
    fn a_done_report_makes_the_run_ready_with_evidence() {
        use nebula_core::ext::{EvidenceBadge, EvidenceKind, EvidenceState, WorkflowState};
        use nebula_core::AgentId;
        let store = store();
        let ticket_id = TicketId::new("c1", "1");
        let mut t = ticket("1");
        t.id = ticket_id.clone();
        store.upsert_ticket(&t).unwrap();
        let agent = AgentId::generate();
        store.start_run(&ticket_id, &agent).unwrap();

        // Before the report the run is running (agent row absent → interrupted,
        // but the point is it is not Ready).
        let mut tickets = store.load_tickets().unwrap();
        overlay_run_state(&store, &mut tickets);
        assert_ne!(tickets[0].workflow, Some(WorkflowState::Ready));

        // The agent reports done with captured evidence.
        let badge = EvidenceBadge {
            kind: EvidenceKind::Implementation,
            state: EvidenceState::Passed,
            revision: Some("abc1234".into()),
            label: Some("3 files changed".into()),
        };
        let run = store
            .report_run_for_agent(&agent, "ready", "did the thing", &[badge.clone()])
            .unwrap();
        assert!(run.is_some(), "the report found the agent's run");

        // Now the board shows Ready with the evidence, regardless of the
        // agent's liveness.
        let mut tickets = store.load_tickets().unwrap();
        overlay_run_state(&store, &mut tickets);
        assert_eq!(tickets[0].workflow, Some(WorkflowState::Ready));
        assert_eq!(tickets[0].evidence, vec![badge]);
    }

    #[test]
    fn a_report_from_an_agent_with_no_run_is_a_miss() {
        use nebula_core::AgentId;
        let store = store();
        let missed = store
            .report_run_for_agent(&AgentId::generate(), "ready", "", &[])
            .unwrap();
        assert!(missed.is_none(), "no run to report on");
    }

    #[test]
    fn boot_reconciliation_interrupts_running_runs() {
        use nebula_core::ext::WorkflowState;
        use nebula_core::AgentId;
        let store = store();
        let id = TicketId::new("c1", "1");
        let mut t = ticket("1");
        t.id = id.clone();
        store.upsert_ticket(&t).unwrap();
        store.start_run(&id, &AgentId::generate()).unwrap();
        // A daemon restart can't prove the run live → interrupted.
        assert_eq!(store.interrupt_running_runs().unwrap(), 1);
        let mut tickets = store.load_tickets().unwrap();
        overlay_run_state(&store, &mut tickets);
        assert_eq!(tickets[0].workflow, Some(WorkflowState::Interrupted));
        // A ready run is not touched by a later reconciliation.
        store
            .report_run_for_agent(&AgentId::generate(), "ready", "", &[])
            .ok();
    }

    #[test]
    fn add_run_evidence_appends_and_replaces_same_kind() {
        use nebula_core::ext::{EvidenceBadge, EvidenceKind, EvidenceState};
        use nebula_core::AgentId;
        let store = store();
        let id = TicketId::new("c1", "1");
        let mut t = ticket("1");
        t.id = id.clone();
        store.upsert_ticket(&t).unwrap();
        let agent = AgentId::generate();
        store.start_run(&id, &agent).unwrap();
        // Report done with an implementation badge.
        store
            .report_run_for_agent(
                &agent,
                "ready",
                "",
                &[EvidenceBadge {
                    kind: EvidenceKind::Implementation,
                    state: EvidenceState::Passed,
                    ..Default::default()
                }],
            )
            .unwrap();
        // A checks badge appends beside implementation.
        store
            .add_run_evidence(
                &id,
                &EvidenceBadge {
                    kind: EvidenceKind::Checks,
                    state: EvidenceState::Running,
                    ..Default::default()
                },
            )
            .unwrap();
        // The final checks result replaces the running one (same kind), not appends.
        store
            .add_run_evidence(
                &id,
                &EvidenceBadge {
                    kind: EvidenceKind::Checks,
                    state: EvidenceState::Passed,
                    ..Default::default()
                },
            )
            .unwrap();
        let mut tickets = store.load_tickets().unwrap();
        overlay_run_state(&store, &mut tickets);
        let ev = &tickets[0].evidence;
        assert_eq!(ev.len(), 2, "implementation + one checks badge");
        let checks = ev.iter().find(|b| b.kind == EvidenceKind::Checks).unwrap();
        assert_eq!(checks.state, EvidenceState::Passed);
    }

    #[test]
    fn evidence_goes_stale_when_the_worktree_moves_past_its_revision() {
        use nebula_core::ext::{EvidenceBadge, EvidenceKind, EvidenceState};
        use nebula_core::{
            Agent, AgentKind, AgentStatus, Project, ProjectId, Worktree, WorktreeId,
        };
        // A real git repo with one commit, so `current_head` resolves.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().to_path_buf();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@e.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@e.com")
                .output()
                .unwrap()
        };
        git(&["init", "-b", "main"]);
        git(&["commit", "--allow-empty", "-m", "one"]);
        let head = String::from_utf8_lossy(&git(&["rev-parse", "--short", "HEAD"]).stdout)
            .trim()
            .to_string();

        let store = store();
        let project = Project {
            workspace_id: Default::default(),
            id: ProjectId::generate(),
            name: "demo".into(),
            repo_path: repo.clone(),
            sort_order: 0,
        };
        store.insert_project(&project).unwrap();
        let worktree = Worktree {
            id: WorktreeId::generate(),
            project_id: project.id.clone(),
            path: repo.clone(),
            branch: "main".into(),
            is_main: true,
            sort_order: 0,
        };
        store.insert_worktree(&worktree).unwrap();
        let agent_id = nebula_core::AgentId::generate();
        store
            .insert_agent(&Agent {
                id: agent_id.clone(),
                worktree_id: worktree.id.clone(),
                name: "AQ-1".into(),
                status: AgentStatus::Finished,
                archived: false,
                archived_at: 0,
                unseen: false,
                kind: AgentKind::Claude,
                custom_harness: None,
                model: None,
                effort: None,
                session_id: None,
                cloud_session_id: None,
                sort_order: 0,
                status_changed_at: 0,
                alive: false,
                recent_prompts: Vec::new(),
            })
            .unwrap();
        let id = TicketId::new("c1", "1");
        let mut t = ticket("1");
        t.id = id.clone();
        store.upsert_ticket(&t).unwrap();
        store.start_run(&id, &agent_id).unwrap();

        // Evidence assessed at a *different* revision goes stale.
        store
            .report_run_for_agent(
                &agent_id,
                "ready",
                "",
                &[EvidenceBadge {
                    kind: EvidenceKind::Implementation,
                    state: EvidenceState::Passed,
                    revision: Some("deadbee".into()),
                    label: None,
                }],
            )
            .unwrap();
        let mut tickets = store.load_tickets().unwrap();
        overlay_run_state(&store, &mut tickets);
        assert_eq!(tickets[0].evidence[0].state, EvidenceState::Stale);

        // Evidence assessed at the current HEAD is not stale.
        store
            .report_run_for_agent(
                &agent_id,
                "ready",
                "",
                &[EvidenceBadge {
                    kind: EvidenceKind::Implementation,
                    state: EvidenceState::Passed,
                    revision: Some(head),
                    label: None,
                }],
            )
            .unwrap();
        let mut tickets = store.load_tickets().unwrap();
        overlay_run_state(&store, &mut tickets);
        assert_eq!(tickets[0].evidence[0].state, EvidenceState::Passed);
    }

    #[test]
    fn ticket_ref_persists_for_resume() {
        use nebula_core::ext::TicketRef;
        use nebula_core::{
            Agent, AgentKind, AgentStatus, Project, ProjectId, Worktree, WorktreeId,
        };
        let store = store();
        let project = Project {
            workspace_id: Default::default(),
            id: ProjectId::generate(),
            name: "demo".into(),
            repo_path: "/tmp/demo".into(),
            sort_order: 0,
        };
        store.insert_project(&project).unwrap();
        let worktree = Worktree {
            id: WorktreeId::generate(),
            project_id: project.id.clone(),
            path: "/tmp/demo".into(),
            branch: "feat/aq-1".into(),
            is_main: false,
            sort_order: 0,
        };
        store.insert_worktree(&worktree).unwrap();
        let agent = Agent {
            id: nebula_core::AgentId::generate(),
            worktree_id: worktree.id.clone(),
            name: "AQ-1".into(),
            status: AgentStatus::Running,
            archived: false,
            archived_at: 0,
            unseen: false,
            kind: AgentKind::Claude,
            custom_harness: None,
            model: None,
            effort: None,
            session_id: Some("sess".into()),
            cloud_session_id: None,
            sort_order: 0,
            status_changed_at: 0,
            alive: false,
            recent_prompts: Vec::new(),
        };
        store.insert_agent(&agent).unwrap();
        // No ref until set; then it round-trips (rebuilt on every resume).
        assert!(store.agent_ticket_ref(&agent.id).unwrap().is_none());
        let tref = TicketRef {
            connection: "c1".into(),
            native: "1".into(),
            key: "AQ-1".into(),
            summary: "do the thing".into(),
        };
        store.set_agent_ticket_ref(&agent.id, &tref).unwrap();
        assert_eq!(store.agent_ticket_ref(&agent.id).unwrap(), Some(tref));
    }

    #[test]
    fn inbox_dedupes_and_marks_read() {
        use nebula_core::ext::InboxEvent;
        let store = store();
        let ev = |key: &str, read: Option<i64>| InboxEvent {
            dedupe_key: key.into(),
            kind: "ready".into(),
            ticket: Some(TicketId::new("c1", "1")),
            title: "AQ-1 is ready".into(),
            body: "did it".into(),
            created_ms: 1,
            read_ms: read,
        };
        // First insert is new; the same dedupe key is ignored (PRD §10).
        assert!(store
            .insert_inbox_event(&ev("c1\u{1}1:ready:abc", None))
            .unwrap());
        assert!(!store
            .insert_inbox_event(&ev("c1\u{1}1:ready:abc", None))
            .unwrap());
        // A different revision is a new event.
        assert!(store
            .insert_inbox_event(&ev("c1\u{1}1:ready:def", None))
            .unwrap());
        assert_eq!(store.load_inbox().unwrap().len(), 2);
        // Mark one read; a second mark is a no-op.
        assert!(store.mark_inbox_read("c1\u{1}1:ready:abc").unwrap());
        assert!(!store.mark_inbox_read("c1\u{1}1:ready:abc").unwrap());
        let inbox = store.load_inbox().unwrap();
        let read = inbox
            .iter()
            .find(|e| e.dedupe_key == "c1\u{1}1:ready:abc")
            .unwrap();
        assert!(read.read_ms.is_some());
        assert_eq!(inbox.iter().filter(|e| e.read_ms.is_none()).count(), 1);
    }

    #[test]
    fn the_queue_enqueues_dispatches_and_never_double_queues() {
        use nebula_core::ext::WorkflowState;
        use nebula_core::AgentId;
        let store = store();
        for n in ["1", "2", "3"] {
            let id = TicketId::new("c1", n);
            let mut t = ticket(n);
            t.id = id.clone();
            store.upsert_ticket(&t).unwrap();
            assert!(store.enqueue_run(&id).unwrap().is_some());
        }
        // Three queued, oldest-first, and re-enqueueing one is a no-op.
        let queued = store.queued_runs().unwrap();
        assert_eq!(queued.len(), 3);
        assert_eq!(queued[0].1.native, "1");
        assert!(
            store
                .enqueue_run(&TicketId::new("c1", "1"))
                .unwrap()
                .is_none(),
            "a queued ticket is never double-queued"
        );

        // Activate the first (simulate a dispatch): it leaves the queue and
        // becomes running.
        let (run_id, tid) = queued[0].clone();
        store.activate_run(&run_id, &AgentId::generate()).unwrap();
        assert_eq!(store.queued_runs().unwrap().len(), 2);
        let running = store.running_runs().unwrap();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].0, tid);
        // The still-queued tickets show Queued on the board.
        let mut tickets = store.load_tickets().unwrap();
        overlay_run_state(&store, &mut tickets);
        let still_queued = tickets.iter().find(|t| t.id.native == "2").unwrap();
        assert_eq!(still_queued.workflow, Some(WorkflowState::Queued));
    }

    #[test]
    fn review_agent_is_a_distinct_role_on_the_run() {
        use crate::store::AgentRole;
        use nebula_core::AgentId;
        let store = store();
        let id = TicketId::new("c1", "1");
        let mut t = ticket("1");
        t.id = id.clone();
        store.upsert_ticket(&t).unwrap();
        let implementer = AgentId::generate();
        store.start_run(&id, &implementer).unwrap();
        let reviewer = AgentId::generate();
        store.set_review_agent(&id, &reviewer).unwrap();

        // Each agent resolves to its own role on the same run.
        let (t1, r1) = store.run_role_for_agent(&implementer).unwrap().unwrap();
        assert_eq!(t1, id);
        assert_eq!(r1, AgentRole::Implement);
        let (t2, r2) = store.run_role_for_agent(&reviewer).unwrap().unwrap();
        assert_eq!(t2, id);
        assert_eq!(r2, AgentRole::Review);
        // An unrelated agent has no role.
        assert!(store
            .run_role_for_agent(&AgentId::generate())
            .unwrap()
            .is_none());
    }

    #[test]
    fn cancel_marks_the_latest_run_cancelled() {
        use nebula_core::ext::WorkflowState;
        use nebula_core::AgentId;
        let store = store();
        let id = TicketId::new("c1", "1");
        let mut t = ticket("1");
        t.id = id.clone();
        store.upsert_ticket(&t).unwrap();
        store.start_run(&id, &AgentId::generate()).unwrap();
        assert!(store
            .set_latest_run_state(&id, "cancelled")
            .unwrap()
            .is_some());
        let mut tickets = store.load_tickets().unwrap();
        overlay_run_state(&store, &mut tickets);
        assert_eq!(tickets[0].workflow, Some(WorkflowState::Cancelled));
    }

    #[tokio::test]
    async fn a_ticket_that_leaves_scope_is_marked_removed_not_dropped() {
        let store = store();
        let (events, _rx) = broadcast::channel(64);
        let cfg = ConnectionConfig {
            id: "c1".into(),
            kind: "jira".into(),
            ..Default::default()
        };
        let secret = Secret { token: "t".into() };
        // Beat one tracks two.
        let first = FakeTracker::new("c1", vec![ticket("1"), ticket("2")]);
        sync_connection(&store, &events, &cfg, &first, &secret, &[]).await;
        // Beat two returns only one — the other left the assignment scope.
        let second = FakeTracker::new("c1", vec![ticket("1")]);
        sync_connection(&store, &events, &cfg, &second, &secret, &[]).await;
        let all = store.load_tickets().unwrap();
        assert_eq!(
            all.len(),
            2,
            "the departed ticket is retained for explanation"
        );
        let gone = all.iter().find(|t| t.id.native == "2").unwrap();
        assert!(gone.removed_reason.is_some(), "and carries a reason");
    }
}
