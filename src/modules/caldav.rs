use anyhow::{Context, Result};
use chrono::Utc;
use regex::Regex;
use reqwest::Client;
use tracing::{info, warn};

/// A CalDAV calendar object — either a VTODO or VEVENT.
#[derive(Debug, Clone)]
pub enum CalDavItem {
    Todo(VTodo),
    Event(VEvent),
}

/// A CalDAV VTODO item.
#[derive(Debug, Clone)]
pub struct VTodo {
    pub uid: String,
    pub summary: String,
    pub status: VTodoStatus,
    pub due: Option<String>,
    pub priority: Option<u8>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum VTodoStatus {
    NeedAction,
    InProgress,
    Completed,
    Cancelled,
}

impl VTodoStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NeedAction => "NEEDS-ACTION",
            Self::InProgress => "IN-PROCESS",
            Self::Completed => "COMPLETED",
            Self::Cancelled => "CANCELLED",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s.to_uppercase().as_str() {
            "COMPLETED" => Self::Completed,
            "IN-PROCESS" => Self::InProgress,
            "CANCELLED" => Self::Cancelled,
            _ => Self::NeedAction,
        }
    }
}

/// A CalDAV VEVENT item.
#[derive(Debug, Clone)]
pub struct VEvent {
    pub uid: String,
    pub summary: String,
    pub dtstart: String,
    pub dtend: Option<String>,
    pub description: Option<String>,
    pub all_day: bool,
}

/// A CalDAV resource (href + etag + raw data).
#[derive(Debug, Clone)]
pub struct CalDavResource {
    pub href: String,
    pub etag: String,
    pub data: Option<String>,
}

pub struct CalDavClient {
    client: Client,
    base_url: String,
    collection_url: String,
    auth: String,
}

impl CalDavClient {
    pub fn new(base_url: &str, collection_path: &str, username: &str, password: &str) -> Self {
        use base64::Engine;
        let auth = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(
            format!("{}:{}", username, password)
        ));
        let collection_url = format!("{}/{}", base_url.trim_end_matches('/'), collection_path.trim_start_matches('/'));

        Self {
            client: Client::new(),
            base_url: base_url.to_string(),
            collection_url,
            auth,
        }
    }

    /// Create the collection if it doesn't exist (MKCALENDAR).
    pub async fn ensure_collection(&self) -> Result<()> {
        let resp = self.client
            .request(reqwest::Method::from_bytes(b"PROPFIND").unwrap(), &self.collection_url)
            .header("Authorization", &self.auth)
            .header("Depth", "0")
            .body(CALENDAR_PROPFIND.to_string())
            .send()
            .await;

        match resp {
            Ok(r) if r.status().is_success() => {
                info!("calDAV collection exists: {}", self.collection_url);
                return Ok(());
            }
            Ok(r) if r.status().as_u16() == 404 => {
                info!("creating CalDAV collection: {}", self.collection_url);
            }
            Ok(r) => {
                warn!("PROPFIND failed: {} - {}", r.status(), r.text().await.unwrap_or_default());
            }
            Err(e) => {
                warn!("PROPFIND error: {}", e);
            }
        }

        let resp = self.client
            .request(reqwest::Method::from_bytes(b"MKCALENDAR").unwrap(), &self.collection_url)
            .header("Authorization", &self.auth)
            .body(MKCALENDAR_BODY.to_string())
            .send()
            .await
            .context("MKCALENDAR request")?;

        if resp.status().is_success() || resp.status().as_u16() == 201 {
            info!("created CalDAV collection");
            Ok(())
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            warn!("MKCALENDAR failed {}: {}", status, &body[..body.len().min(300)]);
            if status.as_u16() == 405 {
                Ok(())
            } else {
                anyhow::bail!("MKCALENDAR failed: {}", status)
            }
        }
    }

    /// List all calendar items (VTODO + VEVENT) in the collection.
    pub async fn list_items(&self) -> Result<Vec<CalDavResource>> {
        let resp = self.client
            .request(reqwest::Method::from_bytes(b"REPORT").unwrap(), &self.collection_url)
            .header("Authorization", &self.auth)
            .header("Depth", "1")
            .body(CALDAV_REPORT.to_string())
            .send()
            .await
            .context("REPORT request")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("list items failed {}: {}", status, &body[..body.len().min(300)]);
        }

        let body = resp.text().await?;
        parse_multistatus(&body)
    }

    /// Get a single calendar item by href.
    pub async fn get_item(&self, href: &str) -> Result<Option<CalDavResource>> {
        let url = format!("{}/{}", self.base_url.trim_end_matches('/'), href.trim_start_matches('/'));
        let resp = self.client
            .get(&url)
            .header("Authorization", &self.auth)
            .send()
            .await
            .context("GET calendar item")?;

        if resp.status().as_u16() == 404 {
            return Ok(None);
        }

        let status = resp.status();
        let etag = resp.headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = resp.text().await?;
        if !status.is_success() {
            anyhow::bail!("get item failed {}: {}", status, &body[..body.len().min(200)]);
        }

        Ok(Some(CalDavResource {
            href: href.to_string(),
            etag,
            data: Some(body),
        }))
    }

    /// Create or update a calendar item (VTODO or VEVENT).
    pub async fn put_item(&self, href: &str, item: &CalDavItem, if_match: Option<&str>) -> Result<String> {
        let url = format!("{}/{}", self.base_url.trim_end_matches('/'), href.trim_start_matches('/'));
        let ical = match item {
            CalDavItem::Todo(vt) => build_vtodo_ical(vt),
            CalDavItem::Event(ve) => build_vevent_ical(ve),
        };

        let mut req = self.client
            .put(&url)
            .header("Authorization", &self.auth)
            .header("Content-Type", "text/calendar; charset=utf-8")
            .body(ical);

        if let Some(etag) = if_match {
            req = req.header("If-Match", etag);
        }

        let resp = req.send().await.context("PUT calendar item")?;
        let status = resp.status();
        let new_etag = resp.headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        if !status.is_success() && status.as_u16() != 201 && status.as_u16() != 204 {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("put item failed {}: {}", status, &body[..body.len().min(200)]);
        }

        Ok(new_etag)
    }

    /// Delete a calendar item.
    pub async fn delete_item(&self, href: &str) -> Result<()> {
        let url = format!("{}/{}", self.base_url.trim_end_matches('/'), href.trim_start_matches('/'));
        let resp = self.client
            .delete(&url)
            .header("Authorization", &self.auth)
            .send()
            .await
            .context("DELETE calendar item")?;

        let status = resp.status();
        if !status.is_success() && status.as_u16() != 404 {
            let body = resp.text().await.unwrap_or_default();
            warn!("delete item failed {}: {}", status, &body[..body.len().min(200)]);
        }
        Ok(())
    }
}

// ─── iCalendar builders ────────────────────────────────────────────────

/// Build a full iCalendar string for a VTODO.
pub fn build_vtodo_ical(vtodo: &VTodo) -> String {
    let now = Utc::now().format("%Y%m%dT%H%M%SZ");
    let completed = if vtodo.status == VTodoStatus::Completed {
        format!("\r\nCOMPLETED:{}\r\nPERCENT-COMPLETE:100", now)
    } else {
        String::new()
    };

    let due = if let Some(ref d) = vtodo.due {
        let d_clean = d.replace('-', "");
        format!("\r\nDUE;VALUE=DATE:{}", d_clean)
    } else {
        String::new()
    };

    let priority = if let Some(p) = vtodo.priority {
        format!("\r\nPRIORITY:{}", p)
    } else {
        String::new()
    };

    let description = format_ical_description(&vtodo.description);

    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//memotag-rs//EN\r\nBEGIN:VTODO\r\nUID:{}\r\nDTSTAMP:{}\r\nSUMMARY:{}\r\nSTATUS:{}{}\r\nEND:VTODO\r\nEND:VCALENDAR\r\n",
        vtodo.uid, now, vtodo.summary, vtodo.status.as_str(), format!("{}{}{}{}", completed, due, priority, description),
    )
}

/// Build a full iCalendar string for a VEVENT.
pub fn build_vevent_ical(event: &VEvent) -> String {
    let now = Utc::now().format("%Y%m%dT%H%M%SZ");

    let dtstart = if event.all_day {
        // All-day event: VALUE=DATE, format YYYYMMDD
        format!("DTSTART;VALUE=DATE:{}", event.dtstart.replace('-', ""))
    } else {
        // Timed event: format YYYYMMDDTHHMMSSZ
        format!("DTSTART:{}", event.dtstart.replace('-', "").replace('T', "").trim_end_matches('Z'))
    };

    let dtend = if let Some(ref end) = event.dtend {
        if event.all_day {
            format!("\r\nDTEND;VALUE=DATE:{}", end.replace('-', ""))
        } else {
            format!("\r\nDTEND:{}", end.replace('-', "").replace('T', "").trim_end_matches('Z'))
        }
    } else {
        String::new()
    };

    let description = format_ical_description(&event.description);

    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//memotag-rs//EN\r\nBEGIN:VEVENT\r\nUID:{}\r\nDTSTAMP:{}\r\n{}{}\r\nSUMMARY:{}{}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        event.uid, now, dtstart, dtend, event.summary, description,
    )
}

fn format_ical_description(desc: &Option<String>) -> String {
    if let Some(ref d) = desc {
        let escaped = d.replace('\\', "\\\\").replace('\n', "\\n").replace(',', "\\,").replace(';', "\\;");
        format!("\r\nDESCRIPTION:{}", escaped)
    } else {
        String::new()
    }
}

// ─── iCalendar parsers ─────────────────────────────────────────────────

/// Detect whether raw iCalendar data is a VTODO or VEVENT, and parse it.
pub fn parse_ical(data: &str) -> Option<CalDavItem> {
    if data.contains("BEGIN:VTODO") {
        parse_vtodo(data).map(CalDavItem::Todo)
    } else if data.contains("BEGIN:VEVENT") {
        parse_vevent(data).map(CalDavItem::Event)
    } else {
        None
    }
}

/// Parse a VTODO from raw iCalendar data.
pub fn parse_vtodo(data: &str) -> Option<VTodo> {
    let re_uid = Regex::new(r"UID:(.+)").ok()?;
    let re_summary = Regex::new(r"SUMMARY:(.+)").ok()?;
    let re_status = Regex::new(r"STATUS:(.+)").ok()?;
    let re_due = Regex::new(r"DUE(?:;VALUE=DATE)?:\s*(\d{8})").ok()?;
    let re_priority = Regex::new(r"PRIORITY:(\d)").ok()?;
    let re_completed = Regex::new(r"COMPLETED:").ok()?;

    let uid = re_uid.captures(data)?.get(1)?.as_str().trim().to_string();
    let summary = re_summary.captures(data)?.get(1)?.as_str().trim().to_string();

    let mut status = VTodoStatus::NeedAction;
    if let Some(caps) = re_status.captures(data) {
        status = VTodoStatus::from_str(caps.get(1)?.as_str().trim());
    }
    if re_completed.is_match(data) && status == VTodoStatus::NeedAction {
        status = VTodoStatus::Completed;
    }

    let due = re_due.captures(data).and_then(|c| {
        let raw = c.get(1)?.as_str();
        if raw.len() == 8 {
            Some(format!("{}-{}-{}", &raw[0..4], &raw[4..6], &raw[6..8]))
        } else {
            None
        }
    });

    let priority = re_priority.captures(data)
        .and_then(|c| c.get(1)?.as_str().parse().ok());

    let description = parse_ical_field(data, "DESCRIPTION");

    Some(VTodo { uid, summary, status, due, priority, description })
}

/// Parse a VEVENT from raw iCalendar data.
pub fn parse_vevent(data: &str) -> Option<VEvent> {
    let re_uid = Regex::new(r"UID:(.+)").ok()?;
    let re_summary = Regex::new(r"SUMMARY:(.+)").ok()?;
    let re_dtstart_date = Regex::new(r"DTSTART(?:;VALUE=DATE)?:\s*(\d{8})(?:T(\d{6})Z?)?").ok()?;

    let uid = re_uid.captures(data)?.get(1)?.as_str().trim().to_string();
    let summary = re_summary.captures(data)?.get(1)?.as_str().trim().to_string();

    let caps = re_dtstart_date.captures(data)?;
    let date_raw = caps.get(1)?.as_str();
    let time_raw = caps.get(2).map(|m| m.as_str());

    let (dtstart, all_day) = if let Some(time) = time_raw {
        // Timed event: 20260415T143000 -> 2026-04-15T14:30:00
        let d = format!("{}-{}-{}", &date_raw[0..4], &date_raw[4..6], &date_raw[6..8]);
        let t = format!("{}:{}:{}", &time[0..2], &time[2..4], &time[4..6]);
        (format!("{}T{}Z", d, t), false)
    } else {
        // All-day event: 20260415 -> 2026-04-15
        let d = format!("{}-{}-{}", &date_raw[0..4], &date_raw[4..6], &date_raw[6..8]);
        (d, true)
    };

    let dtend = if let Some(caps) = Regex::new(r"DTEND(?:;VALUE=DATE)?:\s*(\d{8})(?:T(\d{6})Z?)?").ok()
        .and_then(|re| re.captures(data))
    {
        let end_date = caps.get(1)?.as_str();
        let end_time = caps.get(2).map(|m| m.as_str());
        if all_day {
            Some(format!("{}-{}-{}", &end_date[0..4], &end_date[4..6], &end_date[6..8]))
        } else if let Some(t) = end_time {
            let d = format!("{}-{}-{}", &end_date[0..4], &end_date[4..6], &end_date[6..8]);
            let tm = format!("{}:{}:{}", &t[0..2], &t[2..4], &t[4..6]);
            Some(format!("{}T{}Z", d, tm))
        } else {
            Some(format!("{}-{}-{}", &end_date[0..4], &end_date[4..6], &end_date[6..8]))
        }
    } else {
        None
    };

    let description = parse_ical_field(data, "DESCRIPTION");

    Some(VEvent { uid, summary, dtstart, dtend, description, all_day })
}

/// Extract a single text field from iCalendar data.
fn parse_ical_field(data: &str, field: &str) -> Option<String> {
    let pattern = format!("{}:(.+)", field);
    let re = Regex::new(&pattern).ok()?;
    re.captures(data)
        .and_then(|c| c.get(1))
        .map(|m| {
            m.as_str()
                .replace("\\n", "\n")
                .replace("\\,", ",")
                .replace("\\;", ";")
                .replace("\\\\", "\\")
        })
}

// ─── XML parsing ───────────────────────────────────────────────────────

/// Parse CalDAV multistatus XML response into resources.
fn parse_multistatus(xml: &str) -> Result<Vec<CalDavResource>> {
    let mut resources = Vec::new();

    let re_response = Regex::new(r"(?s)<(?:d:)?response>(.*?)</(?:d:)?response>").unwrap();
    let re_href = Regex::new(r"<(?:d:)?href>(.*?)</(?:d:)?href>").unwrap();
    let re_etag = Regex::new(r"<(?:d:)?getetag>(.*?)</(?:d:)?getetag>").unwrap();
    let re_ctag = Regex::new(r"<(?:d:)?getctag>(.*?)</(?:d:)?getctag>").unwrap();

    for caps in re_response.captures_iter(xml) {
        let response_block = &caps[1];

        if let Some(href) = re_href.captures(response_block) {
            let href = href.get(1).unwrap().as_str().trim().to_string();
            let etag = re_etag.captures(response_block)
                .or_else(|| re_ctag.captures(response_block))
                .map(|c| c.get(1).unwrap().as_str().trim().to_string())
                .unwrap_or_default();

            resources.push(CalDavResource { href, etag, data: None });
        }
    }

    Ok(resources)
}

// ─── CalDAV XML bodies ────────────────────────────────────────────────

/// REPORT body: fetch ALL calendar items (VTODO + VEVENT).
const CALDAV_REPORT: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:report xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop>
    <d:getetag/>
    <c:calendar-data/>
  </d:prop>
  <c:comp-filter name="VCALENDAR"/>
</d:report>"#;

const CALENDAR_PROPFIND: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:">
  <d:prop>
    <d:resourcetype/>
    <d:getetag/>
  </d:prop>
</d:propfind>"#;

/// MKCALENDAR: create a collection that supports both VTODO and VEVENT.
const MKCALENDAR_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<c:mkcalendar xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:set>
    <d:prop>
      <d:displayname>Memos</d:displayname>
      <c:calendar-description>Tasks and events synced from memos</c:calendar-description>
      <c:supported-component-set>
        <c:comp name="VTODO"/>
        <c:comp name="VEVENT"/>
      </c:supported-component-set>
    </d:prop>
  </d:set>
</c:mkcalendar>"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_and_parse_vtodo() {
        let vtodo = VTodo {
            uid: "test-123".into(),
            summary: "Buy milk".into(),
            status: VTodoStatus::NeedAction,
            due: Some("2026-04-15".into()),
            priority: Some(1),
            description: None,
        };
        let ical = build_vtodo_ical(&vtodo);
        let parsed = parse_vtodo(&ical).unwrap();
        assert_eq!(parsed.uid, "test-123");
        assert_eq!(parsed.summary, "Buy milk");
        assert_eq!(parsed.status, VTodoStatus::NeedAction);
        assert_eq!(parsed.due.as_deref(), Some("2026-04-15"));
        assert_eq!(parsed.priority, Some(1));
    }

    #[test]
    fn test_build_and_parse_vevent_all_day() {
        let event = VEvent {
            uid: "evt-456".into(),
            summary: "Doctor appointment".into(),
            dtstart: "2026-04-15".into(),
            dtend: Some("2026-04-16".into()),
            description: Some("Annual checkup".into()),
            all_day: true,
        };
        let ical = build_vevent_ical(&event);
        assert!(ical.contains("DTSTART;VALUE=DATE:20260415"));
        assert!(ical.contains("DTEND;VALUE=DATE:20260416"));
        assert!(ical.contains("SUMMARY:Doctor appointment"));

        let parsed = parse_vevent(&ical).unwrap();
        assert_eq!(parsed.uid, "evt-456");
        assert_eq!(parsed.summary, "Doctor appointment");
        assert!(parsed.all_day);
        assert_eq!(parsed.dtstart, "2026-04-15");
    }

    #[test]
    fn test_build_and_parse_vevent_timed() {
        let event = VEvent {
            uid: "evt-789".into(),
            summary: "Team standup".into(),
            dtstart: "2026-04-15T09:00:00Z".into(),
            dtend: Some("2026-04-15T09:30:00Z".into()),
            description: None,
            all_day: false,
        };
        let ical = build_vevent_ical(&event);
        assert!(ical.contains("DTSTART:20260415T090000Z"));

        let parsed = parse_vevent(&ical).unwrap();
        assert_eq!(parsed.uid, "evt-789");
        assert!(!parsed.all_day);
    }

    #[test]
    fn test_parse_ical_detects_type() {
        let vtodo_ical = build_vtodo_ical(&VTodo {
            uid: "x".into(), summary: "t".into(), status: VTodoStatus::NeedAction,
            due: None, priority: None, description: None,
        });
        assert!(matches!(parse_ical(&vtodo_ical), Some(CalDavItem::Todo(_))));

        let vevent_ical = build_vevent_ical(&VEvent {
            uid: "y".into(), summary: "e".into(), dtstart: "2026-01-01".into(),
            dtend: None, description: None, all_day: true,
        });
        assert!(matches!(parse_ical(&vevent_ical), Some(CalDavItem::Event(_))));
    }
}
