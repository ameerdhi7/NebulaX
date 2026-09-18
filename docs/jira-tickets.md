# Jira ticket board

Nebula can pull the Jira tickets assigned to you and show them as a board —
cards you scan and open, kept current by the daemon whether or not the UI is
open. Press **`Shift+J`** in the TUI (or pick **Ticket board** from the `/`
palette) to open it, or run **`nebula web`** for the same board in a browser.

**Status:** the board reads your assigned tickets (five filters, per-ticket
detail, blocker reasons), **starts** one or a batch (bounded by a concurrency
limit, the rest queued), runs configured **checks** and an independent
**review** stage, tracks quality **evidence** that goes *stale* when the code
moves, keeps a durable **inbox** of events, and shows a **progress report**
(`p`). The loop closes honestly: the agent reports with **`nebula stage done`**,
the daemon captures the diff as evidence, and the card becomes *Ready* — a turn
that ends without a report is *Needs input*, never a fake *Ready*. Both a
terminal (`Shift+J`) and a browser (`nebula web`) client show the same board.
Not built yet: the Bitbucket adapter (PR sync), and the browser client's richer
detail/inbox/report panes (the terminal has them).

## The workflow loop

1. `s` on a card → the daemon cuts `feat/<key>-<slug>` and launches Claude with
   the ticket as context. The card shows *Running*.
2. The agent implements and commits, then runs (unprompted — the permission is
   pre-granted) `nebula stage done --summary "…"`.
3. The daemon captures the worktree's diff as an *Implementation* evidence badge
   and marks the run *Ready*; the card and Brief update live.
4. If the agent needs you, it runs `nebula stage needs-input --summary "…"` and
   the card moves to *Needs attention* instead.

## Try it with no Jira (fake connection)

To see the board without a real Jira, add a `fake` connection — it needs no
token and serves a handful of built-in demo tickets (one is blocked by another,
so you can see the dependency gating):

```json
{
  "connections": [
    { "id": "demo", "kind": "fake", "label": "Demo" }
  ]
}
```

Launch nebula, press `Shift+J`, and the demo tickets appear. To make `s`
(start) actually launch an agent, also give the connection a `repo` — the path
of a repo you've added to nebula as a project:

```json
{ "id": "demo", "kind": "fake", "label": "Demo", "repo": "/Users/you/code/some-project", "base_branch": "main" }
```

## Configure a connection

A connection has two halves. Non-secret settings go in `config.json`; the API
token goes in `config.local.json`, the layer nebula never exports, forwards over
`nebula ssh`, or overwrites on import.

Find both files under the data dir (`nebula config path` prints it — on macOS
`~/Library/Application Support/dev.nebula.nebula/`).

`config.json`:

```json
{
  "connections": [
    {
      "id": "aau-jira",
      "kind": "jira",
      "label": "AAU Jira",
      "base_url": "https://aslaluroba.atlassian.net",
      "account": "ameer.dh@aau.iq",
      "jql": "assignee = currentUser() AND statusCategory != Done ORDER BY Rank ASC"
    }
  ]
}
```

- `id` — a stable name you pick. Tickets are keyed by it, so don't rename it
  once the board has synced.
- `jql` — optional. Left out, the board uses
  `assignee = currentUser() AND statusCategory != Done ORDER BY Rank ASC`.
- `base_url` — your Atlassian site, no trailing path.
- `account` — the email the API token belongs to.

`config.local.json` (secret; stays on this machine):

```json
{
  "secrets": {
    "connections": {
      "aau-jira": { "token": "<your Jira API token>" }
    }
  }
}
```

Create an API token at <https://id.atlassian.com/manage-profile/security/api-tokens>.
The key under `connections` must match the connection `id`.

An env var overrides the file — handy for CI or a one-off:
`NEBULA_TOKEN_AAU_JIRA` (the id upper-cased, non-alphanumerics to `_`).

> The token is plaintext on disk in `config.local.json`. That layer is never
> exported or carried to a remote host, but it is not encrypted; OS-keychain
> storage is planned, not yet built.

## What the board shows

Down the left, one card per ticket: its key and summary, a status chip
(the Jira status category, or the workflow stage once a run has started), and —
for a blocked ticket — the reason it can't start. The card under the cursor is
read on the right: the summary, Jira status, assignee, priority, description and
acceptance criteria (fields with no value read *Not specified*, never invented),
type, components, labels, dependencies, design links, and the Jira link.

Five filters cycle with **Tab** (or jump with **1**–**5**):

| Filter | Shows |
|---|---|
| Needs attention | tickets waiting on you — a failed or interrupted run, unread PR activity |
| Queue | not yet started |
| Active | a run in progress or paused |
| Ready | a run that reached Ready |
| All | everything, blocked and removed included |

Keys: **j/k** move, **Tab/1–5** filter, **s** start, **v** review, **i** inbox,
**p** report, **c** toggle the Changes diff, **x** cancel a run, **m** mark inbox
read, **r** forces a sync now, **o** opens the ticket in the browser, **Esc/q**
closes.

## In a browser

`nebula web` serves the same board as a native web page (a small bridge that
holds its own connection to the daemon and speaks JSON over a WebSocket — not
the terminal-in-a-tab that `nebula browser` gives). It listens on
`127.0.0.1:7690` by default and opens a tab:

```
nebula web                 # serve on 127.0.0.1:7690, open a tab
nebula web --port 8090     # a specific port
nebula web --bind 0.0.0.0 --no-open   # serve to the network (guard the port)
```

The page renders the board, the five filters, evidence badges, and
Start/Sync/Restart buttons. Richer detail, inbox and report panes are in the
terminal client for now.

## Health and freshness

The daemon syncs on a beat (60 s by default; `NEBULA_TRACKER_SYNC_MS` overrides
for tests). A connection reads:

- **configured** — set up, but no token yet.
- **authenticated** — the token logs in.
- **verified** — a sync succeeded.
- **error** — the last sync failed; the board keeps the last-known tickets and
  shows the error, never dropping tickets on a failed fetch.

A ticket that leaves your assignment scope (moved to Done, reassigned, filtered
out) is kept and marked as out of scope with a reason, rather than silently
disappearing.

## Dependencies

Only explicit **Blocks** links gate a ticket — "relates to" and parent/child do
not. A blocked ticket shows *blocked by AQ-###* and sorts after the eligible
work, so what you can actually start leads the board. Starting workflows on that
order arrives with phase 2.
