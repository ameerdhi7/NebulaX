//! Which tickets can start, in what order, and why a blocked one can't —
//! PRD §7, computed as a pure function of the ticket set so it is exhaustively
//! unit-testable like `status.rs`. No I/O: the sync engine hands it the current
//! tickets and it returns the blocked reasons and the display order.
//!
//! Fetching, displaying, and dispatching are separate (PRD §7). This module
//! owns *displaying* the eligible order and *why* an ineligible ticket can't
//! start; F3.1's dispatcher consumes the same functions unchanged.

use nebula_core::ext::{SourceCategory, Ticket, TicketId};
use std::collections::HashMap;

/// Compute and stamp `blocked_reason` on every ticket in place, from the whole
/// set (a dependency's status is another ticket's business). A ticket already
/// carrying a `removed_reason` keeps that as its blocker — it is out of scope,
/// which is the strongest reason of all.
pub fn annotate_blocked_reasons(tickets: &mut [Ticket]) {
    // Native id → its coarse status, so a blocks-edge can ask "is my
    // predecessor done?" without a second provider round-trip.
    let category: HashMap<String, SourceCategory> = tickets
        .iter()
        .map(|t| (t.id.native.clone(), t.status_category))
        .collect();
    // Native id → display key, so a blocker names the predecessor by its
    // "AQ-123" key rather than the raw provider id.
    let keys: HashMap<String, String> = tickets
        .iter()
        .filter(|t| !t.key.is_empty())
        .map(|t| (t.id.native.clone(), t.key.clone()))
        .collect();

    // Snapshot the dep edges first: we need an immutable read of the map while
    // mutating each ticket's reason.
    let reasons: Vec<Option<String>> = tickets
        .iter()
        .map(|t| blocked_reason_for(t, &category, &keys))
        .collect();

    for (t, reason) in tickets.iter_mut().zip(reasons) {
        t.blocked_reason = reason;
    }
}

/// The single ticket's blocker, or `None` when nothing holds it back. Order of
/// precedence: out-of-scope first, then an unsatisfied explicit dependency.
/// Repo-mapping and tool-availability blockers join here in phase 2 — this is
/// deliberately the one place a reason is decided.
fn blocked_reason_for(
    ticket: &Ticket,
    category: &HashMap<String, SourceCategory>,
    keys: &HashMap<String, String>,
) -> Option<String> {
    if let Some(reason) = &ticket.removed_reason {
        return Some(format!("no longer in scope: {reason}"));
    }
    // The predecessor's display key when it is in the set, else its raw id.
    let name = |native: &str| keys.get(native).cloned().unwrap_or_else(|| native.to_string());
    // Only an explicit blocks-edge gates eligibility; "relates to" and
    // parent-child never do (PRD §7). A predecessor is satisfied only when it
    // is Done — a waiver overrides that, recorded per edge.
    for dep in &ticket.deps {
        if !dep.blocks_this || dep.waiver.is_some() {
            continue;
        }
        let predecessor_done =
            matches!(category.get(&dep.target_native), Some(SourceCategory::Done));
        if !predecessor_done {
            // Unknown predecessor status blocks and says so, rather than
            // guessing it is clear (PRD §7).
            return Some(match category.get(&dep.target_native) {
                Some(_) => format!("blocked by {}", name(&dep.target_native)),
                None => format!("blocked by {} (status unknown)", name(&dep.target_native)),
            });
        }
    }
    None
}

/// The default display order among the tickets (PRD §7): the eligible tickets
/// the user could start, ranked. This does not schedule — it decides the
/// order the board offers and a batch fills from.
///
/// Ranking, most significant first:
/// 1. board rank where the provider supplied one (lower first),
/// 2. else the configured priority order (a ticket's priority name's index in
///    `priority_order`; unlisted priorities sort last),
/// 3. else oldest creation timestamp,
/// 4. else the stable provider id, so the order never wobbles between beats.
///
/// A ticket with a `blocked_reason` still appears — dependencies gate
/// *dispatch*, not display — but sorts after the eligible ones so the board's
/// actionable work leads.
pub fn display_order(tickets: &[Ticket], priority_order: &[String]) -> Vec<TicketId> {
    let mut order: Vec<&Ticket> = tickets.iter().collect();
    order.sort_by(|a, b| {
        // Eligible before blocked.
        let blocked = a.blocked_reason.is_some().cmp(&b.blocked_reason.is_some());
        if blocked != std::cmp::Ordering::Equal {
            return blocked;
        }
        // Board rank: Some(lower) first, None last.
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
        // Configured priority index.
        let pa = priority_index(a, priority_order);
        let pb = priority_index(b, priority_order);
        if pa != pb {
            return pa.cmp(&pb);
        }
        // Oldest creation first (RFC-3339 sorts lexicographically).
        match (&a.created_at, &b.created_at) {
            (Some(x), Some(y)) if x != y => return x.cmp(y),
            _ => {}
        }
        // Stable tie-break.
        a.id.native.cmp(&b.id.native)
    });
    order.into_iter().map(|t| t.id.clone()).collect()
}

/// A ticket's index in the configured priority order; unlisted (or missing)
/// priorities sort after every listed one.
fn priority_index(ticket: &Ticket, priority_order: &[String]) -> usize {
    ticket
        .priority
        .as_deref()
        .and_then(|p| {
            priority_order
                .iter()
                .position(|q| q.eq_ignore_ascii_case(p))
        })
        .unwrap_or(priority_order.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket(native: &str, cat: SourceCategory) -> Ticket {
        Ticket {
            id: TicketId::new("c1", native),
            key: format!("AQ-{native}"),
            status_category: cat,
            ..Default::default()
        }
    }

    #[test]
    fn a_blocks_edge_holds_until_the_predecessor_is_done() {
        let mut blocked = ticket("2", SourceCategory::ToDo);
        blocked.deps.push(nebula_core::ext::TicketDep {
            target_native: "1".into(),
            kind: "Blocks".into(),
            blocks_this: true,
            waiver: None,
        });
        let mut set = vec![ticket("1", SourceCategory::InProgress), blocked];
        annotate_blocked_reasons(&mut set);
        // Predecessor 1 is not Done, so 2 is blocked.
        let t2 = set.iter().find(|t| t.id.native == "2").unwrap();
        assert!(t2
            .blocked_reason
            .as_deref()
            .unwrap()
            .contains("blocked by AQ-1"));
        // Now finish the predecessor: 2 clears.
        set[0].status_category = SourceCategory::Done;
        annotate_blocked_reasons(&mut set);
        let t2 = set.iter().find(|t| t.id.native == "2").unwrap();
        assert_eq!(t2.blocked_reason, None);
    }

    #[test]
    fn relates_and_parent_links_never_block() {
        let mut t = ticket("2", SourceCategory::ToDo);
        t.deps.push(nebula_core::ext::TicketDep {
            target_native: "1".into(),
            kind: "Relates".into(),
            blocks_this: false,
            waiver: None,
        });
        let mut set = vec![ticket("1", SourceCategory::ToDo), t];
        annotate_blocked_reasons(&mut set);
        assert_eq!(set[1].blocked_reason, None);
    }

    #[test]
    fn a_waived_edge_does_not_block() {
        let mut t = ticket("2", SourceCategory::ToDo);
        t.deps.push(nebula_core::ext::TicketDep {
            target_native: "1".into(),
            kind: "Blocks".into(),
            blocks_this: true,
            waiver: Some("hotfix, predecessor merged separately".into()),
        });
        let mut set = vec![ticket("1", SourceCategory::ToDo), t];
        annotate_blocked_reasons(&mut set);
        assert_eq!(set[1].blocked_reason, None);
    }

    #[test]
    fn removed_tickets_report_out_of_scope() {
        let mut t = ticket("1", SourceCategory::Done);
        t.removed_reason = Some("reassigned to a teammate".into());
        let mut set = vec![t];
        annotate_blocked_reasons(&mut set);
        assert!(set[0]
            .blocked_reason
            .as_deref()
            .unwrap()
            .contains("no longer in scope"));
    }

    #[test]
    fn order_is_rank_then_priority_then_age_then_id() {
        let mut ranked = ticket("10", SourceCategory::ToDo);
        ranked.rank = Some(1.0);
        let mut ranked2 = ticket("11", SourceCategory::ToDo);
        ranked2.rank = Some(2.0);
        let mut hi = ticket("20", SourceCategory::ToDo);
        hi.priority = Some("High".into());
        let mut lo = ticket("21", SourceCategory::ToDo);
        lo.priority = Some("Low".into());
        let order = display_order(&[lo, ranked2, hi, ranked], &["High".into(), "Low".into()]);
        // Ranked tickets lead (lower rank first), then priority order.
        let natives: Vec<&str> = order.iter().map(|id| id.native.as_str()).collect();
        assert_eq!(natives, vec!["10", "11", "20", "21"]);
    }

    #[test]
    fn blocked_tickets_sort_after_eligible_ones() {
        let mut blocked = ticket("2", SourceCategory::ToDo);
        blocked.blocked_reason = Some("blocked by 1".into());
        let eligible = ticket("3", SourceCategory::ToDo);
        let order = display_order(&[blocked, eligible], &[]);
        assert_eq!(order[0].native, "3", "eligible work leads the board");
        assert_eq!(order[1].native, "2");
    }
}
