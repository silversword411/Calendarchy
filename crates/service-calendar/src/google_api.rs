use async_trait::async_trait;
use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};

const CALENDAR_API_BASE: &str = "https://www.googleapis.com/calendar/v3";

/// Supplies a fresh, valid access token for each request. Kept separate from the
/// client so token refresh (via the OS-keyring-backed refresh token, DESIGN_SPEC.md
/// §7) can be implemented and tested independently of the HTTP call sites here.
#[async_trait]
pub trait AccessTokenProvider: Send + Sync {
    async fn access_token(&self) -> anyhow::Result<String>;
}

#[derive(Debug, thiserror::Error)]
pub enum GoogleApiError {
    /// The stored `syncToken` is no longer valid (HTTP 410). Per DESIGN_SPEC.md §9,
    /// the caller must drop it and fall back to a full resync for that calendar.
    #[error("sync token expired or invalid; a full resync is required")]
    SyncTokenExpired,
    #[error("google api request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("google api returned {status}: {body}")]
    Response {
        status: reqwest::StatusCode,
        body: String,
    },
}

/// One calendar in the authenticated user's calendar list
/// (`GET /users/me/calendarList`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CalendarListEntry {
    pub id: String,
    pub summary: String,
    #[serde(rename = "backgroundColor")]
    pub background_color: Option<String>,
    #[serde(rename = "accessRole")]
    pub access_role: String,
    #[serde(default)]
    pub primary: bool,
}

#[derive(Debug, Deserialize)]
struct CalendarListResponse {
    items: Vec<CalendarListEntry>,
}

/// Google represents an all-day event with `date` and a timed event with `dateTime`
/// (+ `timeZone`); exactly one of `date`/`date_time` is set. Kept as raw strings
/// rather than eagerly parsed — `parsed_date_time` does the RFC 3339 parse on demand
/// so a single malformed field doesn't fail deserializing the whole event.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct EventDateTime {
    pub date: Option<String>,
    #[serde(rename = "dateTime")]
    pub date_time: Option<String>,
    #[serde(rename = "timeZone")]
    pub time_zone: Option<String>,
}

impl EventDateTime {
    pub fn is_all_day(&self) -> bool {
        self.date.is_some()
    }

    pub fn parsed_date_time(&self) -> Option<DateTime<FixedOffset>> {
        self.date_time
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Event {
    pub id: String,
    pub status: Option<String>,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    #[serde(default)]
    pub start: EventDateTime,
    #[serde(default)]
    pub end: EventDateTime,
    #[serde(default)]
    pub recurrence: Vec<String>,
    pub etag: Option<String>,
    pub updated: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EventsListResponseRaw {
    items: Vec<Event>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
    #[serde(rename = "nextSyncToken")]
    next_sync_token: Option<String>,
}

#[derive(Debug)]
pub struct EventsPage {
    pub items: Vec<Event>,
    /// Set when there are more pages to fetch for *this* sync pass — pass it back as
    /// `page_token` on the next call. `None` once the page carrying `next_sync_token`
    /// (the last page) has been fetched.
    pub next_page_token: Option<String>,
    /// Only present on the final page of a sync pass; store it and pass it as
    /// `sync_token` on the next incremental sync (DESIGN_SPEC.md §9).
    pub next_sync_token: Option<String>,
}

/// Thin wrapper over the subset of the Google Calendar API v3 the Calendar service
/// needs: listing calendars and (incrementally) syncing events.
pub struct GoogleCalendarClient {
    http: reqwest::Client,
    tokens: Box<dyn AccessTokenProvider>,
}

impl GoogleCalendarClient {
    pub fn new(tokens: Box<dyn AccessTokenProvider>) -> Self {
        Self {
            http: reqwest::Client::new(),
            tokens,
        }
    }

    async fn get(&self, url: &str, query: &[(&str, &str)]) -> Result<reqwest::Response, GoogleApiError> {
        let token = self
            .tokens
            .access_token()
            .await
            .map_err(|e| GoogleApiError::Response {
                status: reqwest::StatusCode::UNAUTHORIZED,
                body: e.to_string(),
            })?;

        let response = self
            .http
            .get(url)
            .bearer_auth(token)
            .query(query)
            .send()
            .await?;

        if response.status() == reqwest::StatusCode::GONE {
            return Err(GoogleApiError::SyncTokenExpired);
        }
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(GoogleApiError::Response { status, body });
        }
        Ok(response)
    }

    pub async fn list_calendars(&self) -> Result<Vec<CalendarListEntry>, GoogleApiError> {
        let url = format!("{CALENDAR_API_BASE}/users/me/calendarList");
        let response = self.get(&url, &[]).await?;
        let parsed: CalendarListResponse = response.json().await?;
        Ok(parsed.items)
    }

    /// One page of an events sync pass. Pass `sync_token` alone for an incremental
    /// sync; pass `page_token` (with no `sync_token`) to continue paging through an
    /// in-progress sync; pass neither for the first page of a full sync.
    pub async fn list_events(
        &self,
        calendar_id: &str,
        sync_token: Option<&str>,
        page_token: Option<&str>,
    ) -> Result<EventsPage, GoogleApiError> {
        let url = format!(
            "{CALENDAR_API_BASE}/calendars/{}/events",
            urlencoding_calendar_id(calendar_id)
        );

        let mut query: Vec<(&str, &str)> = vec![("singleEvents", "true")];
        if let Some(token) = sync_token {
            query.push(("syncToken", token));
        }
        if let Some(token) = page_token {
            query.push(("pageToken", token));
        }

        let response = self.get(&url, &query).await?;
        let parsed: EventsListResponseRaw = response.json().await?;
        Ok(EventsPage {
            items: parsed.items,
            next_page_token: parsed.next_page_token,
            next_sync_token: parsed.next_sync_token,
        })
    }
}

/// Calendar IDs can contain characters (like `@`) that need escaping in a URL path
/// segment; `reqwest`'s `.query()` handles query-string encoding but not path
/// segments, so this is done by hand rather than pulling in a whole crate for it.
fn urlencoding_calendar_id(calendar_id: &str) -> String {
    let mut out = String::with_capacity(calendar_id.len());
    for byte in calendar_id.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_calendar_list_response() {
        let raw = r##"{
            "kind": "calendar#calendarList",
            "items": [
                {
                    "id": "jane@gmail.com",
                    "summary": "jane@gmail.com",
                    "backgroundColor": "#9fe1e7",
                    "accessRole": "owner",
                    "primary": true
                },
                {
                    "id": "family01234@group.calendar.google.com",
                    "summary": "Family",
                    "backgroundColor": "#f83a22",
                    "accessRole": "writer"
                }
            ]
        }"##;

        let parsed: CalendarListResponse = serde_json::from_str(raw).expect("parse");
        assert_eq!(parsed.items.len(), 2);
        assert!(parsed.items[0].primary);
        assert!(!parsed.items[1].primary);
        assert_eq!(parsed.items[1].access_role, "writer");
    }

    #[test]
    fn parses_timed_and_all_day_events() {
        let raw = r#"{
            "kind": "calendar#events",
            "nextSyncToken": "CPDAy8uDo4wDEPDAy8uDo4wDGAU=",
            "items": [
                {
                    "id": "abc123",
                    "status": "confirmed",
                    "summary": "Standup",
                    "start": { "dateTime": "2026-09-05T09:00:00-04:00", "timeZone": "America/New_York" },
                    "end": { "dateTime": "2026-09-05T09:15:00-04:00", "timeZone": "America/New_York" },
                    "etag": "\"12345\"",
                    "updated": "2026-09-01T12:00:00.000Z"
                },
                {
                    "id": "def456",
                    "status": "confirmed",
                    "summary": "Company Holiday",
                    "start": { "date": "2026-09-07" },
                    "end": { "date": "2026-09-08" }
                }
            ]
        }"#;

        let parsed: EventsListResponseRaw = serde_json::from_str(raw).expect("parse");
        assert_eq!(parsed.next_sync_token.as_deref(), Some("CPDAy8uDo4wDEPDAy8uDo4wDGAU="));
        assert_eq!(parsed.items.len(), 2);

        let timed = &parsed.items[0];
        assert!(!timed.start.is_all_day());
        let start = timed.start.parsed_date_time().expect("parses");
        assert_eq!(start.to_rfc3339(), "2026-09-05T09:00:00-04:00");

        let all_day = &parsed.items[1];
        assert!(all_day.start.is_all_day());
        assert!(all_day.start.parsed_date_time().is_none());
        assert_eq!(all_day.start.date.as_deref(), Some("2026-09-07"));
    }

    #[test]
    fn calendar_id_with_at_sign_is_percent_encoded() {
        assert_eq!(urlencoding_calendar_id("jane@gmail.com"), "jane%40gmail.com");
    }
}
