# From assigned ticket to reviewed change

Ameer opens Nebula in the terminal or the browser. His assigned Jira tickets are already there, ordered by what can actually start. He starts a batch, watches independent agents work, and opens a card when it needs him. That card tells him what changed, what was tested, who reviewed it, and whether Bitbucket has new feedback.

**Status:** planning only; no workflow features implemented by these documents.  
**Confirmed:** terminal **and** web in the first release; Jira Cloud + Bitbucket Cloud.  
**Baseline:** local Nebula v0.29.0, commit `12a31cc`; inspected 2026-09-17.

## The delivery sequence

| Phase | What Ameer gets | Exit condition |
|---|---|---|
| 0 — Shared foundation | One durable ticket model and one engine behind both interfaces | State, provider, client, recovery, and permission contracts proved with fixtures |
| 1 — Assigned work | Jira cards, clear ticket details, blockers, and ordering in terminal and web | Both clients show the same assigned tickets and source freshness |
| 2 — One reliable workflow | Start one ticket, use Jira/Figma context, inspect changes and execution evidence | A ticket survives client closure; interruption cannot create a duplicate writer |
| 3 — Parallel work | Start a bounded batch with dependency and repository controls | Independent tickets run together; blocked tickets wait for the right prerequisite |
| 4 — Review and verification | Claude/Codex review, configured checks, and trustworthy quality badges | Every result identifies the revision it assessed; changes stale old results |
| 5 — Daily release | Bitbucket feedback, notifications, progress reporting, and packaged dual UI | The complete daily scenario passes in both clients |

Phases 1–4 are internal checkpoints. **The first complete release includes both clients and all ten requested capabilities.** A browser showing the existing terminal through `ttyd` does not fulfill the web dashboard requirement.

## The decisions that keep the board honest

Jira status, agent workflow progress, PR status, and quality evidence remain separate. An agent finishing a turn never means a ticket is done. A reviewed or tested badge belongs to a specific code revision. “Next” respects dependencies before priority. Closing an interface leaves the daemon working; a crashed daemon requires reconciliation.

Jira and Git hosts have separate adapters, so the workflow can later support another project management tool without rebuilding the scheduler. Jira/Figma MCP tools give agents context; deterministic provider sync supplies the board. The first release works locally and does not require a hosted service.

## Read only the layer you need

[Product requirements](PRD.md) define the cards, workflow, evidence, notifications, and reporting.  
[Execution plan](EXECUTION.md) maps the work to this repository, implementation slices, dependencies, tests, and release gates.

Start implementation with **F0.1** in the execution plan. It establishes the shared contract before either interface or a scheduler gets its own interpretation of a ticket.
