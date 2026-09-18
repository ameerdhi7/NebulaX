# Product requirements: Jira-driven agentic development

Planning draft • 2026-09-17 • Companion: [execution plan](EXECUTION.md)

## 1. The daily story

Ameer starts his day with a board of tickets assigned to him. Each card explains the work, its Jira status, and why it can start or must wait. He selects a workflow and starts one ticket or a batch. Nebula creates isolated worktrees and runs the selected agents within his concurrency limit.

While agents work, Ameer switches between the terminal and a web dashboard. Both show the same state. When an agent needs information, a check fails, or someone comments on a PR, the relevant card asks for attention. When implementation is ready, opening the card shows the actual change and the evidence behind its review and test badges. Jira completion and PR merging remain visible as separate outcomes.

The product succeeds when Ameer can supervise work from these cards without repeatedly opening agent terminals, Jira pages, and PR pages just to discover what needs him.

## 2. Confirmed scope and proposed defaults

| Decision | First-release scope |
|---|---|
| Clients — confirmed | Existing terminal UI plus a native web dashboard; all core ticket actions in both |
| Tracker — confirmed | Jira Cloud, initially the authenticated user's assigned issues in configured projects/filters |
| Git host — confirmed | Bitbucket Cloud; preserve existing GitHub features |
| Runtime | Existing local Rust daemon; one developer and one authoritative daemon; multiple UI clients |
| Agents | Claude Code and Codex for managed workflow stages; keep existing harnesses usable as sessions |
| Tool context | Jira and Figma MCP access through supported agent harnesses, with explicit connection health |
| Automation | User starts a ticket or a finite batch; no automatic execution just because a ticket was imported |
| Publishing | Local implementation/review/checks first; pushing, creating PRs, and Jira writes are separate explicit actions |
| Completion | “Implementation ready” means the chosen workflow passed its declared requirements; it does not mean merged, deployed, or Jira Done |

Runtime, agent, automation, and publishing rows are recommended product defaults, not new instructions governing the current planning task. Account access, field mappings, repository paths, base branches, checks, and agent permissions are configured during implementation/onboarding.

Out of first-release scope: hosted multi-user SaaS, team access control, billing, arbitrary workflow scripting, automatic merge/deploy, automatic Jira transitions, automatic replies to PR comments, a second PM provider, and a general-purpose MCP marketplace. Local reports and both clients are in scope.

## 3. Requested capabilities and release ownership

| ID | User requirement | Acceptance outcome | Phase |
|---|---|---|---|
| R1 | Assigned tickets as cards | Complete paginated assigned set, original Jira status, workflow progress, freshness | 1 |
| R2 | Spawn workflows with many agents | Bounded parallel runs, isolated writers, pause/cancel/retry, durable history | 2–3 |
| R3 | Status and PR notifications | Workflow/Jira changes and Bitbucket discussion activity reach a durable inbox | 5 |
| R4 | Quality indicators | Implementation, reviewer identity/verdict, and check results backed by revision-specific evidence | 2, 4 |
| R5 | Correct ticket order | Dependencies gate eligibility; explicit ranking chooses among eligible tickets | 1, 3 |
| R6 | Clean UI | Matching ticket concepts in terminal and web, readable states and accessible controls | Every phase |
| R7 | Progress statistics | Distinct-ticket progress, throughput, waiting time, and evidence coverage | 5 |
| R8 | Jira/Figma MCP | Configured harness can use required tools; missing access is explained before dispatch | 2 |
| R9 | Simplified ticket details | Plain-language brief, exact source requirements, blockers, design links, source revision | 1–2 |
| R10 | Code changes on completion | Base-to-result diff, including committed changes, with file and evidence navigation | 2 |

## 4. The ticket card and its detail view

The board offers **Needs attention**, **Queue**, **Active**, **Ready**, and **All** filters. These are views of the same tickets, not replacement Jira columns. Search and filters cover project, repository, ticket key, Jira status, workflow state, and evidence. No card moves under the pointer while the user is selecting it; refresh preserves selection and scroll position.

Each card shows the ticket key and title, a short brief, source status, workflow stage, blocker/next-action reason, agent identity, evidence badges, PR activity, and last sync time. Compact layouts hide secondary detail behind selection, never behind color alone.

Illustrative card, not final styling:

```text
APP-142  Preserve the selected date after refresh
Jira: In progress       Workflow: Reviewing
Implementation: ready   Review: Codex • running
Checks: 8 passed        PR: #82 • 2 unread comments
Next: wait for review   Synced: 30 seconds ago
```

Click or Enter opens the same information architecture in either client:

| View | Content |
|---|---|
| Brief | Problem, expected behavior, acceptance criteria, constraints, dependencies, design links, original Jira link |
| Activity | Run stages, agent sessions, pending questions, source changes, chronological events |
| Changes | Repository selector, base/result revisions, changed files, diff, implementation explanation |
| Quality | Each command and result, logs, reviewer/model, findings, verdict, revision and freshness |
| Pull requests | Linked PRs, state, comments/threads, unread activity, checks where available, source links |

The brief initially formats source fields deterministically. Optional AI simplification adds a short interpretation above the original requirements; it cannot replace them or invent acceptance criteria. It records source revision, generation time, and harness/model. A changed ticket makes its old brief stale. Missing requirements say “Not specified.” Summarization failure leaves the original ticket readable.

The terminal keeps keyboard navigation and mouse activation. The web provides responsive cards, a detail drawer/full page, keyboard focus, text alternatives to status colors, and copyable ticket links. At narrow terminal widths, cards become a list with one detail pane; web supports a single-column layout. Both expose start, pause queue, cancel, retry, open session, diff, and evidence. Raw session access is an escape hatch for questions that cannot yet become structured controls.

## 5. Four independent kinds of state

| State | Owner | Examples |
|---|---|---|
| Ticket status | Jira | To Do, In Progress, In Review, Done; preserve original names/IDs |
| Workflow | Nebula | Queued, Running, Needs input, Blocked, Paused, Ready, Failed, Cancelled, Interrupted |
| Delivery | Bitbucket + explicit links | No PR, Open, Changes requested where supported, Merged, Declined |
| Evidence | Versioned run artifacts | Implementation captured; checks passed/failed; reviewed with findings; stale |

`Running` has an explicit stage: prepare, implement, verify, review, or revise. `Ready` is evaluated against the selected workflow policy. A lightweight implementation-only policy can produce Ready with “Checks: not run” and “Review: not run”; its label must say what was required. The recommended checked policy requires configured checks and an independent review. An agent's existing finished dot remains session status only.

Transitions are guarded. Starting creates a run and a stage attempt; pausing prevents new stage dispatch and does not pretend the current process stopped. “Stop current agent” is separate and preserves its work. Cancelling terminates the run without deleting its worktree. Retry creates a new attempt, retaining the failed one. Reopening a completed ticket creates a new run or explicit revision cycle, never overwrites its history.

One ticket can have multiple repository deliverables. The parent becomes Ready only when every required deliverable satisfies policy. The single-repository path ships at the internal phase-2 checkpoint; explicit multi-repository sequencing is completed in phase 3. Partial readiness stays visible.

## 6. Quality indicators that tell the truth

| Indicator | Required evidence |
|---|---|
| Implementation captured | Structured completion result, recorded base/result identity, captured diff, requirement disposition; no-change result needs an explanation |
| Reviewed by Claude/Codex | Separate review stage, harness/model, assessed revision, findings and verdict; reviewer completion alone is not approval |
| Checks passed | Configured command/suite, start/end, exit result, output artifact, assessed revision; show counts only when parsed reliably |
| Ready under policy | Required artifacts and gates satisfied for every required deliverable on its current revision |
| Human accepted | Explicit user action attached to the reviewed revision, separately from automated review |

Each evidence type distinguishes **not run**, **running**, **passed/accepted**, **failed/changes requested**, **skipped**, **unavailable**, and **stale**, as applicable. A skipped test is not a passed test. A command exiting successfully with no discovered tests must not imply tests ran. Known baseline failures may be recorded and explicitly waived, but remain visible as failures with a waiver reason.

The identity includes base commit, result commit/tree, and a fingerprint of relevant uncommitted and untracked changes. Once code changes, previous review and check results remain in history but lose their current badge. Test/review stages use a frozen revision or controlled snapshot; edits during a check invalidate its result. Rebase, dependency updates, and PR force-push also trigger re-evaluation. Requirement changes require re-acknowledgement even when code is unchanged.

There is no invented quality percentage. The user sees separate evidence and can inspect it.

## 7. What “the right order” means

Fetching, displaying, and dispatching are separate. Sync imports the entire configured assignment scope. Display can prioritize attention. Dispatch considers only eligible tickets, even when the user sorts the board differently.

Eligibility requires current assignment, a usable repository/base-branch mapping, sufficient ticket context, available required tools, no active writer conflict, and satisfied explicit dependencies. Unknown dependency status blocks dispatch and explains why. Read dependencies assigned to others when permitted, but never schedule those tickets as the user's work. “Relates to” and parent-child links do not automatically create execution dependencies.

Default order among eligible work: user queue override, then selected Jira board rank; where rank is unavailable, configured priority ordering; then oldest creation timestamp and stable provider ID. Field and priority mappings are explicit per connection. Ticket number order, newest first, and an LLM's guess are not scheduling rules. An override cannot bypass a dependency; a deliberate dependency waiver is a separate recorded action.

The default code prerequisite is available when its required PR is merged into the configured target and the successor's base contains the needed change. A predecessor agent stopping or a ticket merely moving to Done does not establish that. Non-code dependencies may use an explicitly configured provider-status rule. Cross-repository contract readiness is an explicit edge with a named satisfaction rule. Cycles and missing dependencies explain the block on the card.

Before starting a batch, the user sees selected tickets, execution order, blocked reasons, workflow policy, and concurrency. Proposed defaults are two concurrent tickets and one writing agent per deliverable worktree; configure global and per-repository limits. Review/check workers count toward a separate total process budget so nested work cannot multiply without bounds. Start with a finite batch; filling it with newly imported tickets requires opting in later.

## 8. Sync, changes, and interruptions

Jira sync runs in the daemon whether either client is open. Cache survives offline periods; sync errors preserve last known data and show age. Use paging, overlapping incremental windows, and periodic full reconciliation. Never mark a missing result deleted after an incomplete page fetch. Resolve previously tracked IDs separately so issues moved to Done, reassigned, or removed from a filter do not vanish without explanation.

Reassignment or cancellation removes queued work from eligibility. If an agent is already running, retain its work, flag the source change, and hold the next stage until acknowledged. Acceptance-criteria changes do the same. Fresh assignment/dependency checks run immediately before dispatch; if unavailable, the default is to wait with a clear reason.

Closing a client does not stop sessions. A daemon restart is different: reconcile persisted attempts, worktrees, processes, and artifacts, then mark uncertain runs Interrupted. Never silently spawn a second writer or claim a stage passed because its process disappeared. Resume/retry is explicit when liveness cannot be proven. Timeouts and retry limits prevent endless fix/review loops; proposed maximum is two automatic revision cycles per run.

Worktrees isolate file edits, not credentials, ports, databases, or machine resources. Repository profiles declare setup/cleanup and shared resource locks where needed. Unknown repository mappings and conflicting setup requirements stop dispatch before an agent starts.

## 9. Providers and MCP

Use separate provider contracts for work tracking and source control. A ticket key is display text; persistent identity is connection + provider-native issue ID. PR identity includes connection, repository ID, and native PR ID. This avoids collisions across sites/projects and permits a future Linear, GitHub Issues, or other PM adapter.

Jira Cloud REST supplies deterministic assigned-ticket synchronization, requirements, dependency metadata, and source status. Use enhanced JQL search and its pagination rather than the older search operations currently being removed; search results can lag recent updates. [Atlassian issue-search reference](https://developer.atlassian.com/cloud/jira/platform/rest/v3/api-group-issue-search/).

MCP supplies tools to the running agent. Nebula maintains connection profiles and required capabilities, validates their availability for the chosen harness, and supplies ticket/design references and cached context. It does not ask an LLM to discover the scheduler's queue. The official Atlassian MCP connection supports OAuth-based access under the user's existing permissions. [Atlassian MCP setup](https://support.atlassian.com/atlassian-ai-gateway/docs/get-started-with-the-atlassian-remote-mcp-server/).

For Figma, resolve the ticket's explicit file/node links and retrieve design context through a supported harness. Missing access blocks a design-required stage, with an optional user-recorded waiver where the work can proceed. Official Figma documentation currently restricts remote connections to listed clients, so first-release support uses an approved existing harness rather than assuming Nebula itself can register as a new client. Revalidate this during phase 0. [Figma remote MCP setup](https://developers.figma.com/docs/figma-mcp-server/remote-server-installation/).

Connection status distinguishes “configured,” “authenticated,” and “required tool verified.” A config file alone does not earn Connected. MCP profiles preserve existing harness settings. Credentials stay in the appropriate OS/harness credential store, never ticket prompts, source-controlled project files, exported configuration, or ordinary logs. Jira/Figma source content is task data, not authority to change execution permissions.

## 10. PR feedback and notifications

Bitbucket integration supports explicit PR links plus discovery by configured repository and head branch. One ticket can link multiple PRs through its deliverables. Ambiguous branch matches require selection, not a guessed association. PR creation may remain external in V1; monitoring is mandatory once linked.

Track top-level comments, inline comments, replies, review activity, PR state, and changed head revision. Bitbucket Cloud's comments endpoint is paginated and includes global/inline comments and replies. Keep provider-specific resolution semantics; if unavailable, show Unknown. [Bitbucket pull-request reference](https://developer.atlassian.com/cloud/bitbucket/rest/api-group-pullrequests/).

Reading a notification marks it read locally; it does not resolve a provider thread. A new comment is feedback, not necessarily a change request. “Address feedback” explicitly starts a revision attempt against the linked worktree; the agent does not automatically answer, resolve, push, or merge. Any new code makes relevant old evidence stale.

Events feed a persistent inbox and configurable desktop notifications: needs input, workflow failure/readiness, observed Jira status or assignment change, PR comment/review activity, and merge/decline. All meaningful stage changes remain in the timeline; routine stage notifications can be muted or bundled. Initial sync establishes an activity baseline instead of replaying every old comment as new. Stable event IDs deduplicate retries and multiple clients.

Polling is the default for a local application. Display last successful sync and configured cadence; target new activity within two successful polling intervals, subject to provider limits. Intermediate provider transitions between polls may be missed unless supported history is fetched; reports identify observed history and gaps. A closed UI still gets a durable inbox entry. Desktop delivery requires a running daemon on a machine with an available desktop session; sleeping/offline periods are caught up later. Remote/headless daemons retain events for the next connected client.

## 11. Reporting

The report is a short daily/weekly view, filterable by project/repository and time range, with links back to the tickets behind each number. Use UTC storage and the user's display timezone. State when local observation began and whether the period contains sync gaps.

| Metric | Definition |
|---|---|
| Current progress | Distinct tracked tickets by workflow state, with separate Jira and PR status breakdowns |
| Ready throughput | Distinct tickets reaching Ready during the period; repeated attempts do not add tickets |
| Merged / Jira Done throughput | Separate distinct-ticket counts from observed source transitions; never inferred from local readiness |
| Queue wait | Queue entry to first actual dispatch for a run; show sample count |
| Implementation cycle time | First dispatch to first Ready for the run, including waiting time; report median and sample count |
| Check/review coverage | Current implemented deliverables with fresh evidence divided by current implemented deliverables; separate checks and review |
| Check pass rate | Passed completed check executions / all known completed check executions; show skipped and unavailable separately |
| Attention | Tickets needing input, failed checks, unresolved findings, and unread PR activity |

Show the reporting cohort and counts of additions/removals when scope changes; a shrinking assignment list must not appear as completed work. Tickets with several deliverables count once for ticket throughput and separately for evidence coverage. Reports initially cover observed history only. Cost/token reporting is deferred until the harness provides reliable data.

## 12. Release proof

Ameer can open both clients, see the same assigned board, and read an accurate brief. He starts three tickets: two independent ones run within the configured limit, and a dependent one waits. A stopped agent without a completion artifact never gets an implementation badge. A completed committed change remains visible in Changes. Review findings can trigger a bounded revision cycle, and new code invalidates old quality evidence.

When a colleague comments inline on Bitbucket, Ameer receives one logical unread event in both clients. When Jira changes a ticket, its source status updates without inventing a local workflow outcome. Closing both clients leaves work and sync running. Restarting the daemon leaves recoverable history without duplicate writers. Reports count tickets, not agent turns. Every part of this scenario must pass before calling the first release complete.
