# Execution plan: Jira-driven agentic development

Planning draft • 2026-09-17 • Companions: [README](README.md) · [Product requirements](PRD.md)

Grounded in a full code inspection of nebula v0.29.0 (`12a31cc`). File references cite that
commit; re-verify line-sensitive claims before building on them if the tree has moved.

## 0. How to read this

Work is cut into **slices** (`F<phase>.<n>`), each one reviewable PR against `main`. A slice is
done when its listed tests pass, `make ci` passes, and its exit check holds. Phases 0–4 are
internal checkpoints; only phase 5 is a release (README, "The delivery sequence"). Order inside
a phase is the listed order unless the dependency table (§7) says otherwise.

Two documents own different truths: the [PRD](PRD.md) owns *what the product must do*; this
plan owns *where that lands in this codebase and in what order*. When they disagree, fix the
disagreement — do not silently reinterpret either.

## 1. Where the work lands

| Crate | Today | This plan adds |
|---|---|---|
| `nebula-core` | protocol, entities, IDs, paths, codec, settings, harness registry | ticket/workflow/evidence vocabulary as JSON payload types (`ext.rs`), one protocol envelope, a reusable client connection module |
| `nebula-daemon` | PTYs, SQLite store, git, hook receiver, status engine | provider sync loops (Jira, Bitbucket), scheduler + run/stage machine, check runner, evidence store, durable inbox, desktop notification dispatch |
| `nebula-tui` | ratatui client | ticket board overlay, ticket detail views, quality badges, inbox view, report view |
| `nebula` | thin CLI | `nebula web`, `nebula stage` (agent-facing one-shot), e2e tests with fake provider servers |
| `nebula-web` (new) | — | HTTP/WS bridge (ordinary daemon client) + embedded web dashboard SPA |

Five facts about the codebase that shaped every decision below:

1. **Persistence is one global SQLite DB** (`nebula-daemon/src/store.rs`): a flat
   `const MIGRATIONS: &[&str]` indexed by `PRAGMA user_version` (27 entries today), one
   `Mutex<Connection>`, no pool. Adding tables is trivial; high write volume is not what it
   was built for.
2. **The IPC protocol is positional MessagePack with an all-or-nothing version**
   (`nebula-core/src/protocol.rs`, `PROTOCOL_VERSION = 40`). *Any* change to shared types —
   even a new enum variant — breaks every older client and daemon pair.
3. **Everything GitHub/PR today is TUI-client-side** (`nebula-tui/src/pull_request.rs`,
   `pr_cache.rs`, `issues.rs`, driven by the 2 s `GIT_POLL` tick) and stops when the TUI
   quits. Only the `pr_seen` read-marker table is daemon-side. Continuous Bitbucket
   monitoring and a durable inbox are therefore *new daemon subsystems*, not extensions.
4. **The daemon already runs an axum HTTP server** (`hooks/mod.rs`: loopback, random port,
   per-boot bearer token, constant-time compare) — axum/hyper/tower are already
   dependencies. There is **no HTTP client and no TLS client stack** anywhere; outbound
   calls today are `gh` and `curl` subprocesses.
5. **No agent result protocol exists.** `AgentStatus::Finished` means "a turn ended",
   guarded by the stop gate in `status.rs` — never "the task succeeded". Structured data
   comes *out* of an agent today via one-shot CLI verbs over the socket (`nebula rename`,
   `nebula worktree`, `nebula spawn`, `nebula open`), resolved from `NEBULA_AGENT_ID` and
   pre-authorized through the `Bash(nebula …:*)` permission merge in `hooks/installer.rs`.

The ten extension points this plan builds on, in the order the phases reach them:

| # | Extension point | Used for |
|---|---|---|
| 1 | `nebula-daemon/src/lib.rs::serve()` — the `tokio::spawn` + `env_interval(var, default)` + `shutdown.cancelled()` loop idiom (five instances today) | Jira and Bitbucket sync loops (F1.2, F5.1) |
| 2 | `store.rs::MIGRATIONS` + `Store` methods beside `load_tree` | all new tables (F0.1) |
| 3 | `protocol.rs` + `server.rs::handle_client`'s request match | the one `Ext` envelope (F0.2) |
| 4 | `pr_scope.rs::combined_rule` / `launch_prompts` + the per-context-kind `agents` columns (`pr_url`, `issue_url`) | the ticket context rule on every spawn/resume (F2.2) |
| 5 | the one-shot verb channel (`cli.rs::Command` → `nebula-tui/src/lib.rs::run_*` → `ipc.rs`), with `store::rename_agent_if_auto_pending` as the "first report wins" pattern | `nebula stage` structured completion/evidence (F2.3) |
| 6 | `harness.rs::HarnessDescriptor` + `registry.rs::agent_spawn_command_with` — behavior never branches on `AgentKind` | dispatching implement/review stages to Claude or Codex as descriptor selection (F2.1, F4.2) |
| 7 | `status.rs::AgentStatusMachine` — pure, injected clock, no I/O | the run/stage machine and eligibility engine copy this test discipline and *consume* `StatusChanged`, never re-derive it (F1.3, F2.1) |
| 8 | `pr_seen` + `MarkPrSeen` + the lexicographic RFC-3339 marker compare in `PullRequest::unseen` | inbox read-markers and Bitbucket activity baselines (F5.1–F5.2) |
| 9 | `app.rs::Overlay` + `ui.rs::draw_overlay` + `event_loop::handle_overlay_key` + `event_loop::activate`, with `issues.rs::IssuesView` as the working list+detail template and the INPUT PARITY tests as the gate | the terminal ticket board (F1.4) |
| 10 | `hooks/HookEnv`'s token pattern + `nebula-tui/src/ipc.rs::split_connection` (TUI-independent) + the `nebula browser`/`tunnel` port-and-warning discipline | the web bridge (F1.5) |

## 2. Standing decisions

Recorded here so a slice never re-litigates them mid-implementation. Each names what would
reopen it.

**D1 — One protocol envelope, one version bump.** Add exactly two variants in one
`PROTOCOL_VERSION` bump (40 → 41): `ClientRequest::Ext { req_id, kind: String, json }` and
`ServerEvent::Ext { req_id: Option<u64>, kind: String, json }` (JSON bytes via
`serde_bytes`). Every ticket/workflow/evidence/inbox/report message rides inside as JSON with
the hook receiver's tolerance rules (`hooks/mod.rs::HookPayload`: every field optional,
unknown fields ignored, unknown `kind`s ignored by clients / erred by the daemon). Payload
types live in `nebula-core/src/ext.rs` so both halves and the web bridge share one
definition. Rationale: positional msgpack makes every typed variant a wire break; this
feature would otherwise bump the version dozens of times during development, and each bump
strands running daemons (`nebula kill` + reinstall). Revisit: once shapes are stable at
release, promoting hot paths to typed variants is allowed but not required.
**Consequence:** new payloads must stay JSON-lenient (`#[serde(default)]`, no
`deny_unknown_fields`) — fixture-tested like `config-0.29.0.json` is today.

**D2 — HTTP client is `reqwest` with `rustls-tls`, in the daemon only.** No OpenSSL — the
release matrix (`.github/workflows/release.yml`) cross-compiles two musl targets and
musl+OpenSSL is known friction; rustls is pure Rust. Proved by a `workflow_dispatch` dry run
of `release.yml` in F0.4 *before* any provider code depends on it. Fallback if the dry run
fails: shell out to `curl` (precedent: `update_check.rs` and the hook one-liners) behind the
same provider trait, and say so in this file.

**D3 — Tickets live beside the entity tree, not inside it.** The tree is
Workspace → Project → Worktree → Session, and everything in it is keyed to a checkout; a
ticket that hasn't started has no worktree to hang from. So tickets, deliverables, runs,
stages, evidence, PRs, and inbox events are their own tables (§3), streamed to clients as
`Ext` snapshot + deltas after `Subscribe`, and *joined* to worktrees/agents by ID where work
has started. The existing `Entity`/`EntityId`/`Snapshot` types are not touched. Identity
follows the PRD: connection + provider-native ID is the key; the ticket key ("AQ-123") is
display text.

**D4 — The web dashboard is a bridge process, not a daemon feature.** New crate
`nebula-web`, launched as `nebula web [--port --bind --credential --no-open]`. It holds an
ordinary unix-socket connection (the reusable half of `ipc.rs`, extracted to
`nebula-core::client` — pure addition, no wire change), translates protocol ↔ JSON over
WebSocket, and serves an embedded SPA. It inherits `Subscribe`/snapshot/deltas and
per-connection workspace scoping for free, and the daemon stays browser-ignorant. Auth
mirrors `nebula browser` exactly: loopback + no auth by default, `--bind`/`--public` with
escalating stderr warnings, `--credential` for basic auth; plus a bearer token for the WS
upgrade minted like `HookEnv.token`. SPA: Vite + TypeScript (React), sources in `web/`,
`dist/` embedded via `rust-embed`; the node toolchain is a build-time-only dependency and CI
builds it. Rationale: `ttyd` is explicitly ruled out by the README; baking a SPA into the
daemon couples release cycles and bloats the binary. Revisit: only if the bridge's second
socket connection proves a real limitation.

**D5 — Agent completion is a new one-shot verb, never inferred.** `nebula stage` (F2.3)
is how a managed agent reports structured results — completion summary, requirement
disposition, review findings/verdict — following the four existing verbs. The workflow
engine treats `AgentStatus::Finished` without a stage report as *stopped without artifact*
(PRD §12: that must never earn an implementation badge). Only Claude and Codex run managed
stages (PRD §2) — Cursor never reaches NeedsFeedback, Muse/Custom have no hooks at all, so
their telemetry cannot drive a workflow.

**D6 — Checks are daemon-run processes with durable artifacts, not PTY sessions.** The
check runner spawns the configured command via the same login-shell wrapper agents use,
captures output to `data_dir()/evidence/<run>/<attempt>/`, and records exit code, timing,
and revision identity in the `evidence` table. Not the RUN TERMINAL: its ring buffer dies
with the daemon and evidence must survive restarts. Revision identity = base commit +
result commit/tree + an FNV-1a fingerprint of uncommitted/untracked content — the exact
persisted-stable hash discipline `review.rs::fingerprint` already uses (never
`DefaultHasher`).

**D7 — Provider credentials are plaintext in `config.local.json`, stated as such.** That
layer is already "never exported, forwarded or overwritten by an import"
(`settings.rs`; `bundle.rs` and `nebula ssh` carry only `config.json` + presets), which is
the property tokens need. Non-secret connection config (base URL, project filters, repo
mappings) lives in `config.json`. F0.3 adds tests pinning that exports/bundles cannot
carry the secrets key, and the Settings UI writes secrets through the existing
"local-held keys stay local" save path. OS keychain support is explicitly deferred.

**D8 — Existing GitHub features are not touched in this release.** `pull_request.rs`,
`pr_cache.rs`, `issues.rs` keep working exactly as they do (PRD: "preserve existing GitHub
features"). Bitbucket is built daemon-side on the new provider contract. Migrating GitHub
onto that contract is future work, not a phase here.

**D9 — CI lands before feature code.** There is no build/test workflow today (only
release-on-tag and Claude review). F0.5 adds one running `make ci`'s substance
(`fmt --check` + `cargo test --workspace`); clippy stays advisory because the workspace
does not clear `-D warnings` (pre-existing lints in `config.rs`, `ui.rs`, `hooks/mod.rs`).

## 3. New surface inventory

The single reference for names; slices cite it rather than restating.

**Store migrations (28+),** appended to `MIGRATIONS` in house style (nullable
request-driven columns, no backfill, FKs off only for table rebuilds):

| Table | Holds |
|---|---|
| `connections` | provider connections: kind (`jira`/`bitbucket`), label, base URL, account, non-secret config JSON. Tokens never in the DB (D7). |
| `tickets` | connection + provider-native ID (unique), key, summary, source status name/category, assignee, rank, priority, provider timestamps, raw fields JSON, brief JSON + brief source-revision, last-synced marker, tracked/removed reason |
| `ticket_links` | dependency edges: kind, direction, target provider ID, satisfaction rule, waiver |
| `deliverables` | ticket → repo mapping: project ID, base branch, worktree ID (nullable until started) |
| `runs` | one workflow execution: ticket, policy, state, revision-cycle count |
| `stage_attempts` | run + deliverable + stage (`prepare`/`implement`/`verify`/`review`/`revise`), state, agent ID, timings, detail JSON |
| `evidence` | attempt + kind + state (§PRD 6's seven states), base/result commit, dirty fingerprint, artifact path, summary JSON, stale flag |
| `provider_prs` | connection + repo ID + PR ID (unique), deliverable link, state, head revision, last-activity marker, raw JSON |
| `inbox_events` | stable `dedupe_key` (unique), kind, ticket/PR refs, payload JSON, created/read timestamps |

**Protocol:** the two `Ext` variants (D1). Payload kinds are namespaced strings —
`tickets/snapshot`, `tickets/upsert`, `runs/upsert`, `evidence/upsert`, `inbox/event`,
`inbox/mark-read`, `board/start`, `board/pause`, `board/cancel`, `board/retry`,
`stage/report`, `reports/summary`, `connections/status` — registered in
`nebula-core/src/ext.rs` with their payload types.

**CLI:** `nebula web` (D4); `nebula stage <report|evidence> …` (D5, agent-facing, resolved
via `NEBULA_AGENT_ID`, merged into `permissions.allow` by the hook installer).

**Config keys:** `connections` (non-secret, `config.json`); `secrets.connections.<id>`
(`config.local.json` only); per-project `repo_mapping` (ticket project-key/component →
repo path + base branch) beside the existing `projects` map; `workflow` (policy defaults,
concurrency: global, per-repo, process budget, max auto revision cycles = 2); `checks`
per repo (command list); `notifications` (event-class toggles).

**Env (test seams, following `NEBULA_WORKTREE_SYNC_MS`):** `NEBULA_TRACKER_SYNC_MS`,
`NEBULA_SCM_SYNC_MS`; connection base URLs are per-connection config so tests point them
at local fixture servers — no special env needed.

## 4. Phases and slices

### F0 — Shared foundation

*Exit (README): state, provider, client, recovery, and permission contracts proved with fixtures.*

- **F0.1 — Domain contract + schema.** `nebula-core/src/ext.rs`: the payload vocabulary
  (Ticket, Deliverable, Run, StageAttempt, Evidence, InboxEvent, the four state enums from
  PRD §5) as lenient serde JSON types with fixture round-trip tests. Store migrations 28+
  (§3) with `Store` methods and unit tests beside the existing ones. No behavior yet.
  *Exit:* migrations apply on a copy of a real DB; fixtures pin every payload shape.
- **F0.2 — Protocol envelope.** The two `Ext` variants, `PROTOCOL_VERSION` 41,
  `server.rs::handle_client` routing (unknown kind → `Error`), snapshot-after-`Subscribe`
  plumbing for ext payloads, `version_skew_message` still correct. One bump, taken early,
  absorbing all later message evolution (D1).
- **F0.3 — Credential + connection config.** Config keys from §3 through
  `settings::parse_lenient`; secrets constrained to the local layer with tests pinning
  that `nebula config export` and `bundle.rs` cannot carry them (D7). Connection status
  vocabulary (`configured` / `authenticated` / `verified`) modeled now so no later UI
  invents its own.
- **F0.4 — Provider contract + HTTP spike.** `nebula-daemon/src/providers/` — `Tracker`
  and `Scm` traits sized to PRD §9 (deterministic sync, pagination, dependency metadata /
  PR + comment listing), a fixture-backed fake for each, and the reqwest+rustls dependency
  proved by a `release.yml` dry run on all four targets (D2). The sync-loop skeleton in
  `serve()` using the `env_interval` idiom, gated on a change-probe like the worktree
  sync, running against the fake provider in e2e tests.
- **F0.5 — CI.** `.github/workflows/ci.yml`: fmt check + `cargo test --workspace` on PRs
  (D9). Extended in F1.5 to build the SPA.

### F1 — Assigned work

*Exit (README): both clients show the same assigned tickets and source freshness.*

- **F1.1 — Jira Cloud adapter.** `providers/jira.rs` implementing `Tracker`: enhanced JQL
  search with `nextPageToken` pagination (the old search APIs are being removed — PRD §9),
  issue fetch with fields + issue links, board rank via the Agile API for a configured
  board (fallback: configured priority order). Basic auth with email + API token from D7.
  Tested only against fixture servers; a small `--probe-connection` path proves live auth.
- **F1.2 — Sync engine.** The daemon loop from F0.4 made real: full import of the
  configured assignment scope, overlapping incremental windows, periodic full
  reconciliation, separate resolution of previously tracked IDs (a ticket moved to
  Done/reassigned/unfiltered gets a `removed_reason`, never silently vanishes), and the
  PRD §8 rule *never mark missing after an incomplete page fetch* — a failed page keeps
  last-known data and stamps sync age. Writes batched per beat (one transaction) to
  respect the single-`Mutex<Connection>` store; deltas broadcast as `tickets/upsert`.
- **F1.3 — Eligibility + ordering engine.** A pure module in the daemon
  (`scheduler/eligibility.rs`), built like `status.rs`: no I/O, injected inputs,
  exhaustively unit-tested. Implements PRD §7 exactly: eligibility preconditions with a
  stated blocker reason per ticket, then user queue override → board rank → configured
  priority → oldest creation + stable provider ID. Serves both display ("why can't this
  start") and, later, dispatch (F3.1 consumes it unchanged).
- **F1.4 — Terminal board.** `Overlay::Tickets` modeled on `issues.rs::IssuesView` (list +
  reading pane), with the PRD §4 filters as list modes, card fields per the PRD's card
  sketch, and detail tabs Brief/Activity (Changes/Quality/PRs arrive with their phases).
  All activation through `event_loop::activate`; INPUT PARITY tests extended to the new
  rows. Brief = deterministic field formatting first ("Not specified" for gaps); the
  optional AI simplification is deferred until after phase 2 and never replaces source
  fields (PRD §4).
- **F1.5 — Web bridge + dashboard MVP.** `nebula-core::client` extraction, then
  `nebula-web` per D4: WS carrying JSON-translated snapshot/deltas, the board and ticket
  detail read-only, matching filter semantics with the TUI. CI builds the SPA. e2e:
  `nebula web` against a live test daemon asserts the same ticket set the socket reports.
- **F1.6 — Freshness + connection health.** Last-sync age on cards, `connections/status`
  surfaced in both clients, sync-error banners that preserve last-known data (PRD §8).

### F2 — One reliable workflow

*Exit (README): a ticket survives client closure; interruption cannot create a duplicate writer.*

- **F2.1 — Run/stage machine.** `scheduler/run.rs`: guarded transitions per PRD §5
  (start creates run + attempt; pause blocks new dispatch without pretending the process
  stopped; cancel keeps the worktree; retry is a new attempt; reopen is a new run).
  Consumes `StatusChanged` and stage reports; pure core, `status.rs`-style tests for the
  interleavings (agent dies mid-stage, report races Finished, pause during dispatch).
- **F2.2 — Ticket context rule.** Third arm of `pr_scope::combined_rule` + a
  `ticket_ref` column on `agents` beside `pr_url`/`issue_url`, so every cold spawn *and
  resume* re-derives the ticket rule (key, brief pointer, worktree, branch, "report via
  `nebula stage`"). The module's own comment invites exactly this; wording pinned in
  tests like the existing rules.
- **F2.3 — `nebula stage` verb.** The agent-facing one-shot (D5): completion report
  (summary, requirement disposition, no-change explanation) and evidence attachments.
  Daemon side captures the diff itself (base/result identity per D6) rather than trusting
  the agent's paste; "first report wins" via the atomic-conditional-update pattern.
  Installer merges the `Bash(nebula stage:*)` permission.
- **F2.4 — Ticket → worktree provisioning.** Start-ticket creates the deliverable's
  worktree through the existing `worktree_ops`-serialized path (branch name from ticket
  key + slug, base branch from repo mapping), runs worktree hooks as today, dispatches
  the implement stage through `agent_spawn_command_with`. Missing repo mapping blocks
  before any agent starts, with the reason on the card.
- **F2.5 — MCP prerequisite validation.** Connection profiles declare required tools
  (Jira, Figma); pre-dispatch validation checks the chosen harness's MCP config and
  reports `configured`/`authenticated`/`verified` per F0.3 — a config file alone is not
  Connected (PRD §9). Figma first-release stance per PRD: through a supported harness;
  revalidate the remote-client restriction here.
- **F2.6 — Changes + Activity views.** Both clients: base→result diff from the captured
  evidence (TUI reuses the `DiffView` machinery; web renders the same unified diff),
  stage timeline, agent session links (open-session stays the escape hatch).
- **F2.7 — Restart reconciliation.** Extend the boot sweep (`sweep_disconnected`
  discipline): persisted attempts whose liveness can't be proven → `Interrupted`, never
  silently respawned, never counted passed because the process vanished. e2e: kill the
  daemon mid-stage, restart, assert exactly one writer and an explicit resume action.

### F3 — Parallel work

*Exit (README): independent tickets run together; blocked tickets wait for the right prerequisite.*

- **F3.1 — Scheduler + batch start.** Bounded dispatch over F1.3's eligible order:
  global and per-repo writer limits, separate process budget for verify/review workers,
  batch preview (tickets, order, blockers, policy, concurrency) before start, finite
  batches only (PRD §7 defaults: two concurrent tickets, one writer per deliverable
  worktree).
- **F3.2 — Dependencies with teeth.** `ticket_links` satisfaction rules: the default
  code prerequisite (required PR merged into target *and* successor's base contains it —
  checked via the SCM provider), explicit provider-status rules for non-code edges,
  cross-repo contract edges, unknown-status blocks with reasons, recorded waivers as
  separate actions. "Relates to"/parent-child never auto-create edges (PRD §7).
- **F3.3 — Multi-repo deliverables.** One ticket, several deliverables; parent Ready
  only when every required deliverable satisfies policy; partial readiness visible
  (PRD §5). Fresh assignment/dependency re-check immediately before each dispatch.
- **F3.4 — Source-change interlocks.** Reassignment/cancellation dequeues; running work
  is retained, flagged, and holds the next stage until acknowledged; acceptance-criteria
  changes likewise (PRD §8).

### F4 — Review and verification

*Exit (README): every result identifies the revision it assessed; changes stale old results.*

- **F4.1 — Check runner.** Per D6: configured commands per repo, frozen-revision
  execution (dirty fingerprint re-checked after the run; a mid-check edit invalidates the
  result), durable artifacts, the seven evidence states with *skipped ≠ passed* and
  "exited 0 with no tests discovered ≠ tests ran", waivers recorded but still shown as
  failures (PRD §6).
- **F4.2 — Independent review stage.** Review as a separate stage attempt on a second
  harness (descriptor selection, not new spawn code): reviewer gets the captured diff +
  brief, reports findings/verdict via `nebula stage`; reviewer completion without a
  verdict is not approval.
- **F4.3 — Staleness engine.** One place computes evidence currency from revision
  identity: new commits, rebase, dependency updates, PR force-push, and requirement
  changes each demote current badges to history (PRD §6). This is the arbiter every view
  reads; nothing else may decide freshness.
- **F4.4 — Quality surfaces.** Badges + Quality tab in both clients: per-evidence state,
  reviewer/model, assessed revision, drill-down to logs/findings. Ready is always
  labeled with its policy ("Ready — implementation-only" vs "Ready — checked"). Human
  accept is its own recorded action.
- **F4.5 — Bounded revision cycles.** Findings can trigger a revise stage; at most two
  automatic cycles per run (PRD §8), then Needs input.

### F5 — Daily release

*Exit (README): the complete daily scenario passes in both clients.*

- **F5.1 — Bitbucket Cloud adapter + PR sync.** `providers/bitbucket.rs`: explicit PR
  links plus discovery by repo + head branch (ambiguity → user selection, never a guess),
  paginated comments/activity/state, head-revision changes, lexicographic activity
  markers per extension point 8. Daemon-side loop like F1.2, initial-sync baseline so old
  comments don't replay as new (PRD §10).
- **F5.2 — Durable inbox + desktop delivery.** `inbox_events` with stable dedupe keys;
  event classes per PRD §10 (needs input, failure/readiness, Jira status/assignment
  change, PR activity, merge/decline); daemon-side desktop dispatch reusing the
  `osascript`/`notify-send` approach from `event_loop/alerts.rs` (which stays for
  session-status sounds), degrading to inbox-only on headless daemons.
- **F5.3 — Inbox UI.** Both clients: unread counts on cards, inbox list, read-marking
  (local only — never resolves provider threads), mute/bundle for routine stage noise.
  "Address feedback" starts an explicit revision attempt; no automatic replies (PRD §10).
- **F5.4 — Reporting.** The PRD §11 metric set as SQL over the store, served via
  `reports/summary`, rendered in both clients with cohort/scope-change and observation-
  gap annotations. Distinct-ticket counting rules pinned in unit tests.
- **F5.5 — Release hardening.** The PRD §12 scenario as an executable e2e suite against
  fixture providers (three-ticket batch, dependency hold, stopped-agent-no-badge,
  staleness on new code, restart reconciliation, closed-clients-keep-working, inbox
  dedupe across two clients); docs (`docs/` page for the feature, configuration
  reference); release via the repo's `release` skill.

## 5. Cross-cutting engineering rules

- **Testing:** unit tests inline per file (the house style — `status.rs`, `pr_scope.rs`
  are the models: pure cores, injected clocks, pinned wording). e2e in
  `crates/nebula/tests/` using `TestEnv` + env-pointed data dirs; provider fakes are
  local axum servers the connection config points at (the stub-`gh`-binaries precedent
  from `e2e_tui.rs`, upgraded). Every "never" in the PRD (never mark deleted on a partial
  page, never badge a stopped agent, never a second writer) gets a test that tries it.
- **Store discipline:** batch provider writes into one transaction per sync beat; if
  profiling shows the mutex contended by PTY-driven status writes, the fix is batching
  cadence, not a pool.
- **Config discipline:** every new key goes through `parse_lenient`, gets added to the
  next version's fixture (`config-0.30.0.json`…), and unreadable keys must cost only
  themselves.
- **UI discipline:** the board is an overlay, not a new primary view — `Focus` stays a
  five-panel enum and `event_loop.rs` does not grow a router; all activation through
  `activate`; INPUT PARITY tests cover every new row type. Web and TUI ship each
  capability in the same phase, per the PRD's R6.
- **Docs:** each phase updates `docs/` (user-facing) and this file (decisions taken,
  slices re-cut). Deviations from a standing decision are edits to §2 with a dated note.

## 6. Risks and taxes

| Risk | Standing answer |
|---|---|
| Protocol churn strands running daemons during development | D1: one bump, JSON envelope; `make dev`'s isolated instance for daily testing |
| musl + TLS breaks the release matrix | D2's F0.4 dry run before any dependent code; `curl` fallback named |
| Store mutex contention under sync writes | One transaction per beat; volume profiled in F1.2's e2e before F3 raises concurrency |
| Harness telemetry variance (Cursor no NeedsFeedback; Muse/Custom no hooks; Codex missing tool-use events) | D5: managed stages are Claude/Codex only; stage reports, not status, decide outcomes |
| `event_loop.rs` (32 k lines) growth | Board logic lives in its own module like `issues.rs`; event_loop gains only routing arms |
| Figma remote MCP restricted to listed clients | PRD §9 stance (through a supported harness); revalidated in F2.5, waiver path exists |
| Jira search lag / API removal timelines | Enhanced JQL from day one (F1.1); reconciliation pass tolerates lagging search results |
| No secret storage | D7 is an explicit accepted risk, tested against export/bundle leak paths |
| Two-client feature parity doubles UI cost | Shared `ext.rs` payloads are the single source; slices ship both clients together so drift is caught per phase, not at the end |

## 7. Dependency order

F0.1 → F0.2 → {F0.3, F0.4, F0.5} → F1.1 → F1.2 → {F1.3, F1.4, F1.5} → F1.6 →
F2.1 → {F2.2, F2.3} → F2.4 → {F2.5, F2.6} → F2.7 → F3.1 → F3.2 → {F3.3, F3.4} →
F4.1 → F4.2 → F4.3 → {F4.4, F4.5} → F5.1 → F5.2 → {F5.3, F5.4} → F5.5.

Braces run in parallel. The only cross-phase early start worth taking: F5.1's Bitbucket
adapter can begin any time after F0.4 (it depends only on the provider contract), which
de-risks the release phase if phases 2–4 run long.
