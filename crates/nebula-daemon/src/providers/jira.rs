//! The Jira Cloud adapter behind the [`Tracker`] trait.
//!
//! Two calls carry the whole tracker surface: `verify` proves the credential
//! (the cheap `myself` whoami), and `fetch_assigned` walks the assignment set.
//! Search goes through the **enhanced JQL** endpoint (`/rest/api/3/search/jql`)
//! with `nextPageToken` paging — Jira Cloud is removing the older offset
//! `/search` APIs, so the offset ones are not an option (PRD §9).
//!
//! Everything here is lenient about the wire: Jira omits empty fields, so every
//! deserialized field is optional and a missing one maps to `None`/empty rather
//! than failing the whole page. A network or parse failure is an `Err` and the
//! sync engine keeps the last-known board (PRD §8) — this adapter never presumes
//! a ticket gone because one beat failed.

use anyhow::Result;
use async_trait::async_trait;
use nebula_core::ext::{SourceCategory, Ticket, TicketDep, TicketFields, TicketId};
use serde::{Deserialize, Serialize};
use std::time::Duration;

use super::{ConnectionConfig, Secret, Tracker};

/// Matches the 20s ceiling the `gh` calls elsewhere use — long enough for a
/// cold Jira site, short enough that a wedged connection fails the beat cleanly.
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

/// The scope when a connection sets no JQL of its own: mine, still open, ranked.
const DEFAULT_JQL: &str = "assignee = currentUser() AND statusCategory != Done ORDER BY Rank ASC";

/// A hard stop on paging so a server that keeps handing back a `nextPageToken`
/// can never spin the sync beat forever. 50 × 100 = 5000 issues is far past any
/// real assignment set.
const MAX_PAGES: usize = 50;

/// Exactly the issue fields the board maps — asking for the whole issue would
/// pull renderedFields, changelog and every custom field for nothing.
const FIELDS: &[&str] = &[
    "summary",
    "status",
    "assignee",
    "priority",
    "created",
    "updated",
    "description",
    "issuetype",
    "components",
    "labels",
    "issuelinks",
];

/// A Jira Cloud connection. Holds its non-secret config, its token, and one
/// reused HTTP client (connection pooling across paged requests and beats).
pub struct JiraTracker {
    config: ConnectionConfig,
    secret: Secret,
    http: reqwest::Client,
}

impl JiraTracker {
    pub fn new(config: ConnectionConfig, secret: Secret) -> Self {
        // The builder only fails on a broken TLS backend; fall back to a default
        // client rather than unwrap-panic in a constructor the daemon calls per
        // connection.
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .unwrap_or_default();
        Self {
            config,
            secret,
            http,
        }
    }

    /// The site base with any trailing slash removed, so a `/browse/KEY` or a
    /// `/rest/...` join never doubles the slash.
    fn base_url(&self) -> &str {
        self.config.base_url.trim_end_matches('/')
    }

    /// Map one deserialized Jira issue onto the board's [`Ticket`]. Kept a method
    /// so it can borrow the connection id for the persistent [`TicketId`], and so
    /// the tests can exercise the mapping without a live search.
    fn map_issue(&self, issue: &Issue) -> Ticket {
        let f = &issue.fields;
        let summary = f.summary.clone().unwrap_or_default();

        let status_name = f
            .status
            .as_ref()
            .and_then(|s| s.name.clone())
            .unwrap_or_default();
        let status_category = f
            .status
            .as_ref()
            .and_then(|s| s.status_category.as_ref())
            .and_then(|c| c.key.as_deref())
            .map(map_category)
            .unwrap_or(SourceCategory::Unknown);

        let key = issue.key.clone();
        // The provider-native id is the numeric string, never the key — the key
        // is display text and can move under a ticket (PRD §9).
        let url = if key.is_empty() {
            None
        } else {
            Some(format!("{}/browse/{}", self.base_url(), key))
        };

        let deps = f.issuelinks.iter().filter_map(map_link).collect();

        Ticket {
            id: TicketId::new(self.config.id.as_str(), issue.id.as_str()),
            key,
            // A non-empty deterministic brief; the richer formatting lands later.
            brief: summary.clone(),
            summary,
            status_name,
            status_category,
            assignee: f.assignee.as_ref().and_then(|a| a.display_name.clone()),
            priority: f.priority.as_ref().and_then(|p| p.name.clone()),
            // Rank arrives via the Agile API, a later slice — the JQL already
            // orders by Rank, so leaving this None keeps that order.
            rank: None,
            created_at: f.created.clone(),
            updated_at: f.updated.clone(),
            url,
            fields: TicketFields {
                description: render_description(f.description.as_ref()),
                // Jira has no standard acceptance-criteria field.
                acceptance_criteria: None,
                issue_type: f.issuetype.as_ref().and_then(|t| t.name.clone()),
                components: f.components.iter().filter_map(|c| c.name.clone()).collect(),
                labels: f.labels.clone(),
            },
            deps,
            design_links: Vec::new(),
            ..Default::default()
        }
    }
}

#[async_trait]
impl Tracker for JiraTracker {
    fn connection_id(&self) -> &str {
        &self.config.id
    }

    async fn verify(&self) -> Result<()> {
        if self.secret.token.trim().is_empty() {
            anyhow::bail!("no API token configured");
        }
        let url = format!("{}/rest/api/3/myself", self.base_url());
        let resp = self
            .http
            .get(url)
            .basic_auth(&self.config.account, Some(&self.secret.token))
            .send()
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("Jira auth probe failed: {}", resp.status());
        }
        Ok(())
    }

    async fn fetch_assigned(&self) -> Result<Vec<Ticket>> {
        if self.secret.token.trim().is_empty() {
            anyhow::bail!("no API token configured");
        }
        let jql = if self.config.jql.trim().is_empty() {
            DEFAULT_JQL
        } else {
            self.config.jql.as_str()
        };
        let url = format!("{}/rest/api/3/search/jql", self.base_url());

        let mut out = Vec::new();
        let mut next_page_token: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let body = SearchRequest {
                jql,
                fields: FIELDS,
                max_results: 100,
                next_page_token: next_page_token.clone(),
            };
            let resp = self
                .http
                .post(&url)
                .basic_auth(&self.config.account, Some(&self.secret.token))
                .json(&body)
                .send()
                .await?;
            if !resp.status().is_success() {
                anyhow::bail!("Jira search failed: {}", resp.status());
            }
            let page: SearchResponse = resp.json().await?;
            for issue in &page.issues {
                out.push(self.map_issue(issue));
            }

            // `isLast` is authoritative when present; otherwise a missing/blank
            // token is the end. Older responses omit `isLast`, so the token is
            // the fallback and both must agree to continue.
            if page.is_last == Some(true) {
                break;
            }
            match page.next_page_token {
                Some(t) if !t.trim().is_empty() => next_page_token = Some(t),
                _ => break,
            }
        }
        Ok(out)
    }
}

/// Jira's `statusCategory.key` is one of `new|indeterminate|done`; `undefined`
/// is the historical spelling of the no-category state. Everything nebula acts
/// on collapses to these coarse buckets (`Ticket::status_name` keeps the exact
/// name).
fn map_category(key: &str) -> SourceCategory {
    match key {
        "new" | "undefined" => SourceCategory::ToDo,
        "indeterminate" => SourceCategory::InProgress,
        "done" => SourceCategory::Done,
        _ => SourceCategory::Unknown,
    }
}

/// One issue link → a [`TicketDep`]. Only the *blocked* side matters for
/// eligibility: a "Blocks"-type link that reached us as an `inwardIssue` means
/// the linked issue blocks this one. Non-block links and the outward direction
/// are recorded but never gate. Links with no linked-issue id are dropped.
fn map_link(link: &IssueLink) -> Option<TicketDep> {
    let kind = link.link_type.name.clone().unwrap_or_default();
    let name_blocks = link
        .link_type
        .name
        .as_deref()
        .map(|n| n.to_lowercase().contains("block"))
        .unwrap_or(false);

    if let Some(inward) = &link.inward_issue {
        let id = inward.id.clone().unwrap_or_default();
        if id.is_empty() {
            return None;
        }
        return Some(TicketDep {
            target_native: id,
            kind,
            blocks_this: name_blocks,
            waiver: None,
        });
    }
    if let Some(outward) = &link.outward_issue {
        let id = outward.id.clone().unwrap_or_default();
        if id.is_empty() {
            return None;
        }
        return Some(TicketDep {
            target_native: id,
            kind,
            blocks_this: false,
            waiver: None,
        });
    }
    None
}

/// Render a v3 `description` to plain text. It is normally an ADF document, but
/// some site configs return a plain string; both are handled, and a null/absent
/// or whitespace-only result is `None` (renders as "Not specified", never
/// invented — PRD §4).
fn render_description(desc: Option<&serde_json::Value>) -> Option<String> {
    let value = desc?;
    let text = match value.as_str() {
        Some(s) => s.to_string(),
        None => adf_to_text(value),
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Walk an ADF node tree collecting its text. Leaf `text` values concatenate;
/// block nodes (`paragraph`/`heading`/`listItem`) emit a trailing newline so the
/// result reads as lines. Deliberately forgiving — an unknown node just
/// contributes its children's text and never panics.
fn adf_to_text(node: &serde_json::Value) -> String {
    let mut out = String::new();
    if let Some(text) = node.get("text").and_then(|v| v.as_str()) {
        out.push_str(text);
    }
    if let Some(content) = node.get("content").and_then(|v| v.as_array()) {
        for child in content {
            out.push_str(&adf_to_text(child));
        }
    }
    if let Some(kind) = node.get("type").and_then(|v| v.as_str()) {
        if matches!(kind, "paragraph" | "heading" | "listItem") {
            out.push('\n');
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Wire shapes. Request is typed so `nextPageToken` is simply absent on the
// first page. Every response field is optional — Jira omits empty fields, and a
// missing one must never fail the whole page.
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct SearchRequest<'a> {
    jql: &'a str,
    fields: &'a [&'a str],
    #[serde(rename = "maxResults")]
    max_results: u32,
    #[serde(rename = "nextPageToken", skip_serializing_if = "Option::is_none")]
    next_page_token: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SearchResponse {
    issues: Vec<Issue>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
    #[serde(rename = "isLast")]
    is_last: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Issue {
    id: String,
    key: String,
    fields: IssueFields,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct IssueFields {
    summary: Option<String>,
    status: Option<Status>,
    assignee: Option<NamedField>,
    priority: Option<NamedField>,
    created: Option<String>,
    updated: Option<String>,
    // ADF shape varies (doc object, or a plain string on some configs), so it
    // stays a raw Value and is rendered by `render_description`.
    description: Option<serde_json::Value>,
    issuetype: Option<NamedField>,
    components: Vec<NamedField>,
    labels: Vec<String>,
    issuelinks: Vec<IssueLink>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Status {
    name: Option<String>,
    #[serde(rename = "statusCategory")]
    status_category: Option<StatusCategory>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct StatusCategory {
    key: Option<String>,
}

/// Assignee/priority/issuetype/component all arrive as a small named object;
/// assignee carries `displayName`, the rest carry `name`, so both are optional.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct NamedField {
    name: Option<String>,
    #[serde(rename = "displayName")]
    display_name: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct IssueLink {
    #[serde(rename = "type")]
    link_type: LinkType,
    #[serde(rename = "inwardIssue")]
    inward_issue: Option<LinkedIssue>,
    #[serde(rename = "outwardIssue")]
    outward_issue: Option<LinkedIssue>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct LinkType {
    name: Option<String>,
    inward: Option<String>,
    outward: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct LinkedIssue {
    id: Option<String>,
    key: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker() -> JiraTracker {
        let config = ConnectionConfig {
            id: "test-conn".into(),
            kind: "jira".into(),
            base_url: "https://acme.atlassian.net/".into(),
            account: "me@acme.io".into(),
            ..Default::default()
        };
        JiraTracker::new(config, Secret { token: "t".into() })
    }

    #[test]
    fn adf_renders_paragraphs_as_lines() {
        let adf = serde_json::json!({
            "type": "doc",
            "content": [
                { "type": "paragraph", "content": [
                    { "type": "text", "text": "Hello " },
                    { "type": "text", "text": "world" }
                ]},
                { "type": "paragraph", "content": [
                    { "type": "text", "text": "Second line" }
                ]}
            ]
        });
        assert_eq!(adf_to_text(&adf).trim(), "Hello world\nSecond line");

        // A plain string config and an empty doc are both handled.
        assert_eq!(
            render_description(Some(&serde_json::json!("just text"))),
            Some("just text".to_string())
        );
        assert_eq!(render_description(None), None);
    }

    #[test]
    fn search_page_maps_issue_to_ticket() {
        // A single-page response, parsed the same way `fetch_assigned` parses a
        // page, then mapped through the real `map_issue`.
        let json = r#"{
            "isLast": true,
            "issues": [
                {
                    "id": "10001",
                    "key": "AQ-123",
                    "fields": {
                        "summary": "Do the thing",
                        "status": { "name": "In Progress", "statusCategory": { "key": "indeterminate" } },
                        "assignee": { "displayName": "Ameer Dheyaa" },
                        "priority": { "name": "High" },
                        "created": "2026-01-01T00:00:00.000+0000",
                        "updated": "2026-01-02T00:00:00.000+0000",
                        "issuetype": { "name": "Task" },
                        "components": [ { "name": "api" }, { "name": "board" } ],
                        "labels": ["backend", "native"],
                        "description": { "type": "doc", "content": [
                            { "type": "paragraph", "content": [ { "type": "text", "text": "A description" } ] }
                        ]},
                        "issuelinks": [
                            {
                                "type": { "name": "Blocks", "inward": "is blocked by", "outward": "blocks" },
                                "inwardIssue": { "id": "10002", "key": "AQ-100" }
                            },
                            {
                                "type": { "name": "Relates", "inward": "relates to", "outward": "relates to" },
                                "outwardIssue": { "id": "10003", "key": "AQ-101" }
                            }
                        ]
                    }
                }
            ]
        }"#;

        let page: SearchResponse = serde_json::from_str(json).expect("fixture parses");
        assert_eq!(page.is_last, Some(true));
        let t = tracker().map_issue(&page.issues[0]);

        assert_eq!(t.key, "AQ-123");
        assert_eq!(t.id.connection, "test-conn");
        assert_eq!(t.id.native, "10001");
        assert_eq!(t.status_name, "In Progress");
        assert_eq!(t.status_category, SourceCategory::InProgress);
        assert_eq!(t.assignee.as_deref(), Some("Ameer Dheyaa"));
        assert_eq!(t.priority.as_deref(), Some("High"));
        assert_eq!(t.rank, None);
        assert_eq!(
            t.url.as_deref(),
            Some("https://acme.atlassian.net/browse/AQ-123")
        );
        assert_eq!(t.fields.description.as_deref(), Some("A description"));
        assert_eq!(t.fields.issue_type.as_deref(), Some("Task"));
        assert_eq!(t.fields.components, vec!["api", "board"]);
        assert_eq!(t.fields.labels, vec!["backend", "native"]);

        // The inward "Blocks" link gates this ticket; the outward "Relates" does
        // not. Both are recorded, in order.
        assert_eq!(t.deps.len(), 2);
        assert_eq!(t.deps[0].target_native, "10002");
        assert_eq!(t.deps[0].kind, "Blocks");
        assert!(t.deps[0].blocks_this);
        assert_eq!(t.deps[1].target_native, "10003");
        assert!(!t.deps[1].blocks_this);
    }
}
