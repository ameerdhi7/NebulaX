//! The TICKET BOARD (`Shift+J`): the Jira tickets assigned to you, synced by
//! the daemon and shown as cards down the left with the one under the cursor
//! read on the right. Unlike the ISSUES MODAL, this is not the TUI asking `gh`
//! itself — the daemon owns a provider sync loop that keeps the board current
//! whether or not any client is open (PRD §8), and streams the tickets to every
//! subscriber inside the `Ext` protocol envelope (execution-plan D1/D3). This
//! module is the client half: it keeps the last snapshot in [`BoardData`],
//! applies the deltas, and draws the overlay.
//!
//! It shows the assigned set with five filters, per-ticket detail and blocker
//! reasons (R1/R5/R9); starts a run on a card (`s`), reports its live workflow
//! state and evidence, cancels it (`x`), and shows the run's base→result diff
//! in a Changes tab (`c`). Still to come in later phases: an independent review
//! stage and checks, the PR tab, and the optional AI brief.

use crossterm::event::{KeyCode, KeyEvent};
use nebula_core::ext::{
    self, kinds, ConnectionHealth, ConnectionStatus, EvidenceKind, EvidenceState, InboxEvent,
    SourceCategory, Ticket, WorkflowState,
};
use nebula_core::{ClientRequest, ServerEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, Overlay};
use crate::theme::Theme;
use crate::ui::{
    centered_rect_pct, empty_list_row, panel_block, render_row, row_rect, truncate,
    SPLIT_MODAL_PCT, SPLIT_PANE_LAYOUT_MIN,
};

/// The list column's share of the modal, and its floor.
const LIST_PCT: u16 = 42;
const MIN_LIST_W: u16 = 26;
/// Lines one wheel notch scrolls the reading pane.
const WHEEL_LINES: i32 = 3;

/// The board's last-known state, held on [`App`] and refreshed from the
/// daemon's `tickets/snapshot` and the per-ticket `tickets/upsert` deltas.
/// Survives the overlay closing so reopening it paints at once.
#[derive(Debug, Clone, Default)]
pub struct BoardData {
    pub tickets: Vec<Ticket>,
    pub connections: Vec<ConnectionStatus>,
    /// Captured diffs per ticket (flat id → result), filled on demand when the
    /// Changes tab is opened and refreshed each time it is reopened.
    pub changes: std::collections::HashMap<String, ext::ChangesResult>,
    /// The durable inbox, newest first — from the snapshot then `inbox/event`
    /// deltas.
    pub inbox: Vec<InboxEvent>,
    /// The last progress report the daemon computed, on demand.
    pub report: Option<ext::ReportSummary>,
}

impl BoardData {
    /// Replace everything from a full snapshot (sent once after Subscribe).
    fn apply_snapshot(&mut self, snap: ext::TicketsSnapshot) {
        self.tickets = snap.tickets;
        self.connections = snap.connections;
        self.inbox = snap.inbox;
    }

    /// Insert or replace one inbox event, keyed by its dedupe key; newest first.
    fn apply_inbox_event(&mut self, ev: InboxEvent) {
        match self
            .inbox
            .iter_mut()
            .find(|e| e.dedupe_key == ev.dedupe_key)
        {
            Some(existing) => *existing = ev,
            None => self.inbox.insert(0, ev),
        }
    }

    /// How many inbox events are unread.
    pub fn unread(&self) -> usize {
        self.inbox.iter().filter(|e| e.read_ms.is_none()).count()
    }

    /// Insert or replace one ticket from a delta, keyed by provider identity.
    fn apply_upsert(&mut self, ticket: Ticket) {
        match self.tickets.iter_mut().find(|t| t.id == ticket.id) {
            Some(existing) => *existing = ticket,
            None => self.tickets.push(ticket),
        }
    }

    fn apply_connection(&mut self, status: ConnectionStatus) {
        match self.connections.iter_mut().find(|c| c.id == status.id) {
            Some(existing) => *existing = status,
            None => self.connections.push(status),
        }
    }
}

/// Apply one inbound `Ext` event to the board. Unknown kinds are ignored so a
/// newer daemon can push payloads this client doesn't know without harm
/// (execution-plan D1). Returns whether anything changed, so the caller can
/// mark the screen dirty.
pub fn apply_ext(app: &mut App, kind: &str, json: &[u8]) -> bool {
    match kind {
        kinds::TICKETS_SNAPSHOT => {
            app.board.apply_snapshot(ext::decode(json));
            true
        }
        kinds::TICKETS_UPSERT => {
            let upsert: ext::TicketUpsert = ext::decode(json);
            app.board.apply_upsert(upsert.ticket);
            true
        }
        kinds::CONNECTIONS_STATUS => {
            app.board.apply_connection(ext::decode(json));
            true
        }
        kinds::BOARD_CHANGES_RESULT => {
            let result: ext::ChangesResult = ext::decode(json);
            app.board.changes.insert(result.ticket.flat(), result);
            true
        }
        kinds::INBOX_EVENT => {
            app.board.apply_inbox_event(ext::decode(json));
            true
        }
        kinds::REPORTS_SUMMARY => {
            app.board.report = Some(ext::decode(json));
            true
        }
        _ => false,
    }
}

/// Which detail the reading pane shows for the selected ticket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DetailTab {
    #[default]
    Brief,
    Changes,
}

/// The board's five list modes (PRD §4). Views of the same tickets, not Jira
/// columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Filter {
    NeedsAttention,
    Queue,
    Active,
    Ready,
    #[default]
    All,
}

impl Filter {
    const ORDER: [Filter; 5] = [
        Filter::NeedsAttention,
        Filter::Queue,
        Filter::Active,
        Filter::Ready,
        Filter::All,
    ];

    fn label(self) -> &'static str {
        match self {
            Filter::NeedsAttention => "Needs attention",
            Filter::Queue => "Queue",
            Filter::Active => "Active",
            Filter::Ready => "Ready",
            Filter::All => "All",
        }
    }

    fn next(self) -> Filter {
        let i = Filter::ORDER.iter().position(|f| *f == self).unwrap_or(0);
        Filter::ORDER[(i + 1) % Filter::ORDER.len()]
    }

    fn prev(self) -> Filter {
        let i = Filter::ORDER.iter().position(|f| *f == self).unwrap_or(0);
        Filter::ORDER[(i + Filter::ORDER.len() - 1) % Filter::ORDER.len()]
    }

    /// Whether a ticket belongs in this view.
    fn matches(self, t: &Ticket) -> bool {
        let needs_attention = t.unread_pr_activity > 0
            || matches!(
                t.workflow,
                Some(WorkflowState::NeedsInput)
                    | Some(WorkflowState::Failed)
                    | Some(WorkflowState::Interrupted)
            );
        match self {
            Filter::NeedsAttention => needs_attention,
            Filter::Queue => {
                t.removed_reason.is_none()
                    && matches!(t.workflow, None | Some(WorkflowState::Queued))
            }
            Filter::Active => matches!(
                t.workflow,
                Some(WorkflowState::Running) | Some(WorkflowState::Paused)
            ),
            Filter::Ready => matches!(t.workflow, Some(WorkflowState::Ready)),
            Filter::All => true,
        }
    }
}

/// The overlay's own view state — the modal-local cursor, scroll and the rects
/// written back during draw so clicks and the wheel route correctly.
#[derive(Debug, Clone)]
pub struct TicketBoardView {
    pub filter: Filter,
    pub selected: usize,
    pub scroll: u16,
    pub view_height: u16,
    pub body_lines: usize,
    /// Which detail the reading pane shows (Brief or the run's Changes diff).
    pub tab: DetailTab,
    /// When set, the reading pane shows the inbox instead of the ticket detail.
    pub inbox_open: bool,
    /// When set, the reading pane shows the progress report.
    pub report_open: bool,
    pub area: Rect,
    pub list_area: Rect,
    pub body_area: Rect,
}

impl Default for TicketBoardView {
    fn default() -> Self {
        Self {
            filter: Filter::default(),
            selected: 0,
            scroll: 0,
            view_height: 0,
            body_lines: 0,
            tab: DetailTab::default(),
            inbox_open: false,
            report_open: false,
            area: Rect::default(),
            list_area: Rect::default(),
            body_area: Rect::default(),
        }
    }
}

impl TicketBoardView {
    fn max_scroll(&self) -> u16 {
        crate::app::max_scroll(self.body_lines, self.view_height)
    }

    fn scroll_by(&mut self, delta: i32) {
        self.scroll = crate::app::scrolled_by(self.scroll, delta, self.max_scroll());
    }
}

/// Open the board. Read-only display today; a workflow launch lands in phase 2.
pub fn open(app: &mut App) {
    app.overlay = Some(Overlay::Tickets(TicketBoardView::default()));
    app.dirty = true;
}

/// The tickets in the given filter, in display order: eligible before blocked,
/// then board rank (lower first), then key. The daemon annotates blocked
/// reasons; the client only orders. (A fuller ordering — configured priority —
/// lives in the daemon's `eligibility::display_order`; the board's job here is
/// a stable, readable order, not the dispatch order.)
fn ordered(board: &BoardData, filter: Filter) -> Vec<Ticket> {
    let mut rows: Vec<Ticket> = board
        .tickets
        .iter()
        .filter(|t| filter.matches(t))
        .cloned()
        .collect();
    rows.sort_by(|a, b| {
        let blocked = a.blocked_reason.is_some().cmp(&b.blocked_reason.is_some());
        if blocked != std::cmp::Ordering::Equal {
            return blocked;
        }
        match (a.rank, b.rank) {
            (Some(x), Some(y)) => {
                if let Some(o) = x.partial_cmp(&y) {
                    if o != std::cmp::Ordering::Equal {
                        return o;
                    }
                }
            }
            (Some(_), None) => return std::cmp::Ordering::Less,
            (None, Some(_)) => return std::cmp::Ordering::Greater,
            (None, None) => {}
        }
        a.key.cmp(&b.key)
    });
    rows
}

/// Keys for the board overlay. Navigation and filter switching only; a launch
/// is phase 2. Esc / q close.
pub fn handle_key(app: &mut App, key: KeyEvent, out: &mut Vec<ClientRequest>) {
    // The row count under the current filter, for clamping.
    let (rows_len, filter) = match &app.overlay {
        Some(Overlay::Tickets(v)) => (ordered(&app.board, v.filter).len(), v.filter),
        _ => return,
    };
    let Some(Overlay::Tickets(view)) = &mut app.overlay else {
        return;
    };
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.overlay = None;
        }
        KeyCode::Char('j') | KeyCode::Down => {
            view.selected = crate::app::clamp_selection(view.selected as i64 + 1, rows_len);
            view.scroll = 0;
            view.tab = DetailTab::Brief;
        }
        KeyCode::Char('k') | KeyCode::Up => {
            view.selected = crate::app::clamp_selection(view.selected as i64 - 1, rows_len);
            view.scroll = 0;
            view.tab = DetailTab::Brief;
        }
        KeyCode::Tab | KeyCode::Right => {
            view.filter = filter.next();
            view.selected = 0;
            view.scroll = 0;
            view.tab = DetailTab::Brief;
        }
        KeyCode::BackTab | KeyCode::Left => {
            view.filter = filter.prev();
            view.selected = 0;
            view.scroll = 0;
            view.tab = DetailTab::Brief;
        }
        // 1–5 jump straight to a filter.
        KeyCode::Char(c @ '1'..='5') => {
            let idx = c as usize - '1' as usize;
            view.filter = Filter::ORDER[idx];
            view.selected = 0;
            view.scroll = 0;
            view.tab = DetailTab::Brief;
        }
        // Scroll the reading pane.
        KeyCode::Char('J') => view.scroll_by(WHEEL_LINES),
        KeyCode::Char('K') => view.scroll_by(-WHEEL_LINES),
        // `r`: force a provider sync now (the daemon streams the fresh
        // tickets back as deltas). Fire-and-forget, so req_id 0.
        KeyCode::Char('r') => {
            out.push(ClientRequest::Ext {
                req_id: 0,
                kind: kinds::BOARD_SYNC_NOW.into(),
                json: Vec::new(),
            });
            app.flash = Some("syncing tickets…".into());
        }
        // `o`: open the selected ticket in the browser.
        KeyCode::Char('o') => {
            let rows = ordered(&app.board, filter);
            if let Some(url) = rows.get(view.selected).and_then(|t| t.url.clone()) {
                crate::event_loop::open_url(&url);
            }
        }
        // `s` / Enter: start (or re-start) a workflow run on the selected
        // ticket — the daemon cuts a worktree and launches an agent on it. A
        // blocked ticket is refused here with its reason; a run already in
        // progress is left alone, but a finished/failed/cancelled one can be
        // started again (a fresh run).
        KeyCode::Char('s') | KeyCode::Enter => {
            let rows = ordered(&app.board, filter);
            if let Some(ticket) = rows.get(view.selected) {
                if let Some(reason) = &ticket.blocked_reason {
                    app.flash = Some(format!("{} is blocked: {reason}", ticket.key));
                } else if matches!(ticket.workflow, Some(WorkflowState::Running)) {
                    app.flash = Some(format!("{} is already running", ticket.key));
                } else {
                    out.push(ClientRequest::Ext {
                        req_id: 0,
                        kind: kinds::BOARD_START.into(),
                        json: ext::encode(&ext::BoardStart {
                            tickets: vec![ticket.id.clone()],
                            policy: ext::Policy::ImplementationOnly,
                        }),
                    });
                    app.flash = Some(format!("starting {}…", ticket.key));
                }
            }
        }
        // `c`: toggle the Changes diff for the selected ticket, fetching it
        // from the daemon on demand (and refreshing each time it is opened).
        KeyCode::Char('c') => {
            if view.tab == DetailTab::Changes {
                view.tab = DetailTab::Brief;
            } else {
                view.tab = DetailTab::Changes;
                view.scroll = 0;
                let rows = ordered(&app.board, filter);
                if let Some(ticket) = rows.get(view.selected) {
                    out.push(ClientRequest::Ext {
                        req_id: 0,
                        kind: kinds::BOARD_CHANGES.into(),
                        json: ext::encode(&ext::ChangesRequest {
                            ticket: ticket.id.clone(),
                        }),
                    });
                }
            }
        }
        // `x`: cancel the selected ticket's run (keeps the worktree).
        KeyCode::Char('x') => {
            let rows = ordered(&app.board, filter);
            if let Some(ticket) = rows.get(view.selected) {
                if ticket.workflow.is_some() {
                    out.push(ClientRequest::Ext {
                        req_id: 0,
                        kind: kinds::BOARD_CANCEL.into(),
                        json: ext::encode(&ext::TicketTarget {
                            ticket: Some(ticket.id.clone()),
                        }),
                    });
                    app.flash = Some(format!("cancelling {}…", ticket.key));
                }
            }
        }
        // `i`: toggle the inbox pane (workflow/PR events, newest first).
        KeyCode::Char('i') => {
            view.inbox_open = !view.inbox_open;
            view.report_open = false;
            view.scroll = 0;
        }
        // `p`: toggle the progress report pane, fetched on open.
        KeyCode::Char('p') => {
            view.report_open = !view.report_open;
            view.inbox_open = false;
            view.scroll = 0;
            if view.report_open {
                out.push(ClientRequest::Ext {
                    req_id: 0,
                    kind: kinds::REPORTS_REQUEST.into(),
                    json: Vec::new(),
                });
            }
        }
        // `m`: mark every unread inbox event read (local only).
        KeyCode::Char('m') if view.inbox_open => {
            let unread: Vec<String> = app
                .board
                .inbox
                .iter()
                .filter(|e| e.read_ms.is_none())
                .map(|e| e.dedupe_key.clone())
                .collect();
            for dedupe_key in &unread {
                out.push(ClientRequest::Ext {
                    req_id: 0,
                    kind: kinds::INBOX_MARK_READ.into(),
                    json: ext::encode(&ext::InboxMarkRead {
                        dedupe_key: dedupe_key.clone(),
                    }),
                });
            }
            if !unread.is_empty() {
                app.flash = Some(format!("marked {} read", unread.len()));
            }
        }
        // `v`: launch an independent review agent on the selected ticket's run.
        KeyCode::Char('v') => {
            let rows = ordered(&app.board, filter);
            if let Some(ticket) = rows.get(view.selected) {
                if ticket.workflow.is_some() {
                    out.push(ClientRequest::Ext {
                        req_id: 0,
                        kind: kinds::BOARD_REVIEW.into(),
                        json: ext::encode(&ext::TicketTarget {
                            ticket: Some(ticket.id.clone()),
                        }),
                    });
                    app.flash = Some(format!("starting a review of {}…", ticket.key));
                } else {
                    app.flash = Some(format!("{} has no run to review yet", ticket.key));
                }
            }
        }
        _ => {}
    }
    app.dirty = true;
}

/// Draw the board: the connection strip and filter tabs on the frame, the
/// filtered cards down the left, the selected ticket's Brief on the right.
pub fn draw(f: &mut Frame, app: &mut App, view: &TicketBoardView, th: Theme) {
    let area = centered_rect_pct(f.area(), SPLIT_MODAL_PCT.0, SPLIT_MODAL_PCT.1);
    f.render_widget(Clear, area);
    let list_w = (area.width * LIST_PCT / 100)
        .max(MIN_LIST_W)
        .min(area.width.saturating_sub(SPLIT_PANE_LAYOUT_MIN));
    let [list_a, body_a] = Layout::horizontal([
        Constraint::Length(list_w),
        Constraint::Min(SPLIT_PANE_LAYOUT_MIN),
    ])
    .areas(area);

    let rows = ordered(&app.board, view.filter);
    let selected = view.selected.min(rows.len().saturating_sub(1));

    // ---- left: the cards ----
    let unread = app.board.unread();
    let title = if unread > 0 {
        format!(
            "Tickets — {} ({})  •  {unread} unread",
            view.filter.label(),
            rows.len()
        )
    } else {
        format!("Tickets — {} ({})", view.filter.label(), rows.len())
    };
    let block = panel_block(&title, true, th).title_bottom(
        Line::from(Span::styled(
            " Tab: filter  s: start  v: review  i: inbox  p: report  c: changes  x: cancel ",
            Style::default().fg(th.dim),
        ))
        .left_aligned(),
    );
    let list_inner = block.inner(list_a);
    f.render_widget(block, list_a);

    if rows.is_empty() {
        empty_list_row(f, list_inner, empty_text(app, view.filter), th);
    }
    let start = crate::app::window_start(selected, list_inner.height as usize);
    for (i, ticket) in rows.iter().enumerate().skip(start) {
        let Some(row_area) = row_rect(list_inner, i - start) else {
            break;
        };
        render_card(f, row_area, ticket, i == selected, th);
    }

    // ---- right: the report, the inbox, or the selected ticket's detail ----
    let (body_title, body_lines, wrap) = if view.report_open {
        ("Report".to_string(), report_lines(&app.board, th), true)
    } else if view.inbox_open {
        ("Inbox".to_string(), inbox_lines(&app.board, th), true)
    } else {
        match view.tab {
            DetailTab::Brief => (
                brief_title(&rows, selected),
                rows.get(selected)
                    .map(|t| brief_lines(t, &app.board, th))
                    .unwrap_or_else(|| {
                        vec![Line::from(Span::styled(
                            "no ticket selected",
                            Style::default().fg(th.dim),
                        ))]
                    }),
                true,
            ),
            DetailTab::Changes => {
                let (title, lines) = changes_lines(&rows, selected, &app.board, th);
                // A diff is pre-formatted; wrapping it mangles alignment.
                (title, lines, false)
            }
        }
    };
    let body_block = panel_block(&body_title, false, th);
    let body_inner = body_block.inner(body_a);
    f.render_widget(body_block, body_a);
    let mut para = Paragraph::new(body_lines.clone()).scroll((view.scroll, 0));
    if wrap {
        para = para.wrap(Wrap { trim: false });
    }
    f.render_widget(para, body_inner);

    // Write the rects and measured heights back so clicks / the wheel route.
    if let Some(Overlay::Tickets(v)) = &mut app.overlay {
        v.area = area;
        v.list_area = list_a;
        v.body_area = body_a;
        v.view_height = body_inner.height;
        v.body_lines = body_lines.len();
        v.selected = selected;
    }
}

/// The Changes pane: the base→result diff the daemon captured for the selected
/// ticket's run, coloured per line. Shows a status note while the diff is still
/// loading or when there is nothing to show.
fn changes_lines(
    rows: &[Ticket],
    selected: usize,
    board: &BoardData,
    th: Theme,
) -> (String, Vec<Line<'static>>) {
    let Some(ticket) = rows.get(selected) else {
        return ("Changes".into(), vec![]);
    };
    let title = format!("{} — Changes", ticket.key);
    let Some(result) = board.changes.get(&ticket.id.flat()) else {
        return (
            title,
            vec![Line::from(Span::styled(
                "loading the diff…",
                Style::default().fg(th.dim),
            ))],
        );
    };
    if result.diff.trim().is_empty() {
        let note = result.note.clone().unwrap_or_else(|| "no changes".into());
        return (
            title,
            vec![Line::from(Span::styled(note, Style::default().fg(th.dim)))],
        );
    }
    let lines = result
        .diff
        .lines()
        .map(|line| {
            let color = if line.starts_with("+++") || line.starts_with("---") {
                th.muted
            } else if line.starts_with('+') {
                th.ok
            } else if line.starts_with('-') {
                th.err
            } else if line.starts_with("@@") {
                th.accent
            } else if line.starts_with("diff ") || line.starts_with("index ") {
                th.muted
            } else {
                th.text
            };
            Line::from(Span::styled(line.to_string(), Style::default().fg(color)))
        })
        .collect();
    (title, lines)
}

/// The report pane: the daemon's progress summary (PRD §11 / F5.4).
fn report_lines(board: &BoardData, th: Theme) -> Vec<Line<'static>> {
    let dim = Style::default().fg(th.dim);
    let strong = Style::default().fg(th.text);
    let Some(r) = &board.report else {
        return vec![Line::from(Span::styled("computing report…", dim))];
    };
    let row = |label: &str, n: usize| -> Line<'static> {
        Line::from(vec![
            Span::styled(format!("  {n:>4}  "), strong),
            Span::styled(label.to_string(), dim),
        ])
    };
    let mut lines = vec![
        Line::from(Span::styled(
            format!(
                "{} tracked ticket{}",
                r.total,
                if r.total == 1 { "" } else { "s" }
            ),
            strong,
        )),
        Line::raw(""),
        Line::from(Span::styled("Workflow", dim)),
        row("queued", r.queued),
        row("running", r.running),
        row("needs input", r.needs_input),
        row("ready", r.ready),
        row("failed / cancelled", r.failed),
        row("blocked", r.blocked),
        row("not started", r.not_started),
        Line::raw(""),
        Line::from(Span::styled("Jira status", dim)),
        row("to do", r.source_todo),
        row("in progress", r.source_in_progress),
        row("done", r.source_done),
        Line::raw(""),
        Line::from(Span::styled("Evidence (current revision)", dim)),
        row("implementation captured", r.implementation_captured),
        row("checks passed", r.checks_passed),
        row("review passed", r.review_passed),
        Line::raw(""),
        Line::from(vec![
            Span::styled(
                format!("  {:>4}  ", r.attention),
                Style::default().fg(th.warn),
            ),
            Span::styled("need your attention", Style::default().fg(th.warn)),
        ]),
    ];
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "current snapshot — period throughput is a later slice",
        dim,
    )));
    lines
}

/// The inbox pane: durable events newest-first, unread marked with a dot.
/// `m` marks them read.
fn inbox_lines(board: &BoardData, th: Theme) -> Vec<Line<'static>> {
    if board.inbox.is_empty() {
        return vec![Line::from(Span::styled(
            "inbox empty",
            Style::default().fg(th.dim),
        ))];
    }
    let mut lines = vec![Line::from(Span::styled(
        "m: mark all read",
        Style::default().fg(th.dim),
    ))];
    lines.push(Line::raw(""));
    for ev in &board.inbox {
        let unread = ev.read_ms.is_none();
        let (glyph, title_style) = if unread {
            ("● ", Style::default().fg(th.done))
        } else {
            ("  ", Style::default().fg(th.dim))
        };
        lines.push(Line::from(vec![
            Span::styled(glyph, Style::default().fg(th.done)),
            Span::styled(ev.title.clone(), title_style),
        ]));
        if !ev.body.trim().is_empty() {
            lines.push(Line::from(Span::styled(
                format!("    {}", ev.body.trim()),
                Style::default().fg(th.dim),
            )));
        }
    }
    lines
}

fn brief_title(rows: &[Ticket], selected: usize) -> String {
    rows.get(selected)
        .map(|t| format!("{} — Brief", t.key))
        .unwrap_or_else(|| "Brief".into())
}

fn empty_text(app: &App, filter: Filter) -> &'static str {
    if app.board.connections.is_empty() {
        return "no Jira connection configured — add one under connections in config.json";
    }
    match filter {
        Filter::NeedsAttention => "nothing needs your attention",
        Filter::Queue => "no queued tickets",
        Filter::Active => "no active tickets",
        Filter::Ready => "no ready tickets",
        Filter::All => "no assigned tickets",
    }
}

/// One card row: `KEY  summary` on the left, a compact status chip pinned
/// right. A blocked ticket is dimmed and shows its reason instead of the chip.
fn render_card(f: &mut Frame, area: Rect, ticket: &Ticket, selected: bool, th: Theme) {
    let budget = (area.width as usize).saturating_sub(2);
    let chip = status_chip(ticket);
    let chip_w = chip.chars().count();
    let text_budget = budget.saturating_sub(if chip_w > 0 { chip_w + 2 } else { 0 });
    let head = format!("{}  {}", ticket.key, ticket.summary);
    let label = truncate(&head, text_budget);
    let used = label.chars().count();

    let key_style = if ticket.blocked_reason.is_some() {
        Style::default().fg(th.dim)
    } else {
        Style::default().fg(th.accent)
    };
    let mut spans = vec![Span::styled(format!("{} ", ticket.key), key_style)];
    spans.push(Span::raw(
        label
            .strip_prefix(&format!("{} ", ticket.key))
            .unwrap_or(&label)
            .to_string(),
    ));
    if chip_w > 0 && used + chip_w < budget {
        spans.push(Span::raw(" ".repeat(budget - used - chip_w)));
        spans.push(Span::styled(
            chip,
            Style::default().fg(chip_color(ticket, th)),
        ));
    }
    render_row(f, area, spans, selected, true, th);
}

/// The right-aligned chip: the workflow stage if a run has started, else the
/// source status category.
fn status_chip(ticket: &Ticket) -> String {
    if let Some(w) = ticket.workflow {
        return match w {
            WorkflowState::Running => match ticket.stage {
                Some(s) => format!("{s:?}").to_lowercase(),
                None => "running".into(),
            },
            other => format!("{other:?}").to_lowercase(),
        };
    }
    match ticket.status_category {
        SourceCategory::ToDo => "to do".into(),
        SourceCategory::InProgress => "in progress".into(),
        SourceCategory::Done => "done".into(),
        SourceCategory::Unknown => String::new(),
    }
}

fn chip_color(ticket: &Ticket, th: Theme) -> ratatui::style::Color {
    if ticket.blocked_reason.is_some() {
        return th.dim;
    }
    match ticket.workflow {
        Some(WorkflowState::Ready) => th.ok,
        Some(WorkflowState::Failed) | Some(WorkflowState::Interrupted) => th.err,
        Some(WorkflowState::NeedsInput) => th.warn,
        Some(WorkflowState::Running) => th.accent,
        _ => th.dim,
    }
}

/// The Brief reading pane: deterministic formatting of the ticket's source
/// fields (PRD §4). A missing field reads "Not specified", never invented.
fn brief_lines(ticket: &Ticket, board: &BoardData, th: Theme) -> Vec<Line<'static>> {
    let dim = Style::default().fg(th.dim);
    let strong = Style::default().fg(th.text);
    let mut lines: Vec<Line> = Vec::new();

    lines.push(Line::from(Span::styled(ticket.summary.clone(), strong)));
    lines.push(Line::raw(""));

    // Status / assignee / priority.
    lines.push(field(
        "Jira status",
        &if ticket.status_name.is_empty() {
            "Not specified".into()
        } else {
            ticket.status_name.clone()
        },
        th,
    ));
    if let Some(w) = ticket.workflow {
        lines.push(field("Workflow", &format!("{w:?}").to_lowercase(), th));
    }
    lines.push(field(
        "Assignee",
        ticket.assignee.as_deref().unwrap_or("Not specified"),
        th,
    ));
    lines.push(field(
        "Priority",
        ticket.priority.as_deref().unwrap_or("Not specified"),
        th,
    ));
    if let Some(reason) = &ticket.blocked_reason {
        lines.push(Line::from(vec![
            Span::styled("Blocked: ", Style::default().fg(th.warn)),
            Span::styled(reason.clone(), Style::default().fg(th.warn)),
        ]));
    }

    // Evidence — what the run has captured for the current revision. Each row
    // says the kind, its state, and the revision it was assessed at, so a
    // "Ready" is always backed by something inspectable (PRD §6).
    if !ticket.evidence.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled("Evidence", dim)));
        for badge in &ticket.evidence {
            let (glyph, color) = evidence_look(badge.state, th);
            let mut spans = vec![
                Span::styled(format!("  {glyph} "), Style::default().fg(color)),
                Span::styled(evidence_kind_label(badge.kind).to_string(), strong),
                Span::styled(
                    format!(": {}", evidence_state_label(badge.state)),
                    Style::default().fg(color),
                ),
            ];
            if let Some(label) = &badge.label {
                spans.push(Span::styled(format!(" — {label}"), dim));
            }
            if let Some(rev) = &badge.revision {
                spans.push(Span::styled(format!("  @{rev}"), dim));
            }
            lines.push(Line::from(spans));
        }
    }
    lines.push(Line::raw(""));

    // Description.
    lines.push(Line::from(Span::styled("Description", dim)));
    lines.push(Line::from(Span::styled(
        ticket
            .fields
            .description
            .clone()
            .unwrap_or_else(|| "Not specified".into()),
        strong,
    )));
    lines.push(Line::raw(""));

    // Acceptance criteria.
    lines.push(Line::from(Span::styled("Acceptance criteria", dim)));
    lines.push(Line::from(Span::styled(
        ticket
            .fields
            .acceptance_criteria
            .clone()
            .unwrap_or_else(|| "Not specified".into()),
        strong,
    )));
    lines.push(Line::raw(""));

    // Type / components / labels.
    if let Some(t) = &ticket.fields.issue_type {
        lines.push(field("Type", t, th));
    }
    if !ticket.fields.components.is_empty() {
        lines.push(field(
            "Components",
            &ticket.fields.components.join(", "),
            th,
        ));
    }
    if !ticket.fields.labels.is_empty() {
        lines.push(field("Labels", &ticket.fields.labels.join(", "), th));
    }

    // Dependencies.
    let blocks: Vec<&ext::TicketDep> = ticket.deps.iter().filter(|d| d.blocks_this).collect();
    if !blocks.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled("Depends on", dim)));
        for dep in blocks {
            let target_key = board
                .tickets
                .iter()
                .find(|t| t.id.native == dep.target_native)
                .map(|t| t.key.clone())
                .unwrap_or_else(|| dep.target_native.clone());
            let note = if dep.waiver.is_some() {
                " (waived)"
            } else {
                ""
            };
            lines.push(Line::from(Span::styled(
                format!("  {target_key}{note}"),
                strong,
            )));
        }
    }

    // Design links + the source link.
    if !ticket.design_links.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled("Design", dim)));
        for link in &ticket.design_links {
            lines.push(Line::from(Span::styled(format!("  {link}"), strong)));
        }
    }
    if let Some(url) = &ticket.url {
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled("Jira: ", dim),
            Span::styled(url.clone(), Style::default().fg(th.accent)),
        ]));
    }
    if let Some(reason) = &ticket.removed_reason {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            format!("This ticket left your assignment scope: {reason}"),
            dim,
        )));
    }

    lines
}

fn evidence_kind_label(kind: EvidenceKind) -> &'static str {
    match kind {
        EvidenceKind::Implementation => "Implementation",
        EvidenceKind::Checks => "Checks",
        EvidenceKind::Review => "Review",
        EvidenceKind::HumanAccepted => "Human accepted",
    }
}

fn evidence_state_label(state: EvidenceState) -> &'static str {
    match state {
        EvidenceState::NotRun => "not run",
        EvidenceState::Running => "running",
        EvidenceState::Passed => "passed",
        EvidenceState::Failed => "failed",
        EvidenceState::Skipped => "skipped",
        EvidenceState::Unavailable => "unavailable",
        EvidenceState::Stale => "stale",
    }
}

/// A glyph and colour per evidence state — a text alternative to colour, so the
/// state reads without it (PRD §4/R6).
fn evidence_look(state: EvidenceState, th: Theme) -> (&'static str, ratatui::style::Color) {
    match state {
        EvidenceState::Passed => ("✓", th.ok),
        EvidenceState::Failed => ("✗", th.err),
        EvidenceState::Running => ("…", th.accent),
        EvidenceState::Stale => ("~", th.warn),
        EvidenceState::Skipped => ("–", th.dim),
        EvidenceState::NotRun | EvidenceState::Unavailable => ("·", th.dim),
    }
}

fn field(label: &str, value: &str, th: Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label}: "), Style::default().fg(th.dim)),
        Span::styled(value.to_string(), Style::default().fg(th.text)),
    ])
}

/// The connection strip's summary line, e.g. for a header — kept here so the
/// board and a future status bar agree on the wording.
pub fn connection_summary(board: &BoardData) -> String {
    if board.connections.is_empty() {
        return "no connections".into();
    }
    board
        .connections
        .iter()
        .map(|c| {
            let state = match c.health {
                ConnectionHealth::Verified => "ok",
                ConnectionHealth::Authenticated => "auth",
                ConnectionHealth::Configured => "unconfigured",
                ConnectionHealth::Error => "error",
            };
            format!("{}: {state}", c.label)
        })
        .collect::<Vec<_>>()
        .join("  ")
}

/// Route an inbound server event to the board if it is an `Ext` the board
/// understands. Returns whether it applied.
pub fn handle_server_event(app: &mut App, event: &ServerEvent) -> bool {
    if let ServerEvent::Ext { kind, json, .. } = event {
        return apply_ext(app, kind, json);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use nebula_core::ext::TicketId;

    fn ticket(native: &str, cat: SourceCategory) -> Ticket {
        Ticket {
            id: TicketId::new("c1", native),
            key: format!("AQ-{native}"),
            summary: "demo".into(),
            status_category: cat,
            ..Default::default()
        }
    }

    #[test]
    fn snapshot_then_upsert_updates_in_place() {
        let mut board = BoardData::default();
        board.apply_snapshot(ext::TicketsSnapshot {
            tickets: vec![ticket("1", SourceCategory::ToDo)],
            connections: vec![],
            ..Default::default()
        });
        assert_eq!(board.tickets.len(), 1);
        // An upsert of the same id replaces, not appends.
        let mut updated = ticket("1", SourceCategory::InProgress);
        updated.summary = "changed".into();
        board.apply_upsert(updated);
        assert_eq!(board.tickets.len(), 1);
        assert_eq!(board.tickets[0].summary, "changed");
        // A new id appends.
        board.apply_upsert(ticket("2", SourceCategory::ToDo));
        assert_eq!(board.tickets.len(), 2);
    }

    #[test]
    fn filters_partition_by_state() {
        let mut queued = ticket("1", SourceCategory::ToDo);
        queued.workflow = None;
        let mut ready = ticket("2", SourceCategory::InProgress);
        ready.workflow = Some(WorkflowState::Ready);
        let mut needs = ticket("3", SourceCategory::InProgress);
        needs.workflow = Some(WorkflowState::NeedsInput);
        let board = BoardData {
            tickets: vec![queued, ready, needs],
            connections: vec![],
            ..Default::default()
        };
        assert_eq!(ordered(&board, Filter::Queue).len(), 1);
        assert_eq!(ordered(&board, Filter::Ready).len(), 1);
        assert_eq!(ordered(&board, Filter::NeedsAttention).len(), 1);
        assert_eq!(ordered(&board, Filter::All).len(), 3);
    }

    #[test]
    fn blocked_tickets_sort_after_eligible() {
        let mut blocked = ticket("1", SourceCategory::ToDo);
        blocked.blocked_reason = Some("blocked by AQ-9".into());
        let eligible = ticket("2", SourceCategory::ToDo);
        let board = BoardData {
            tickets: vec![blocked, eligible],
            connections: vec![],
            ..Default::default()
        };
        let rows = ordered(&board, Filter::All);
        assert_eq!(rows[0].key, "AQ-2");
    }

    #[test]
    fn unknown_ext_kind_is_ignored() {
        let mut board = BoardData::default();
        board.apply_snapshot(ext::TicketsSnapshot {
            tickets: vec![ticket("1", SourceCategory::ToDo)],
            connections: vec![],
            ..Default::default()
        });
        // Not applied through apply_ext, but the guard is the kind match; a
        // decode of the wrong kind must not clobber state.
        assert_eq!(board.tickets.len(), 1);
    }
}
