use regex::Regex;
use std::sync::LazyLock;

/// A parsed task line from a memo's markdown content.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedTask {
    pub index: usize,
    pub done: bool,
    pub text: String,
    pub raw_line: String,
    pub line_number: usize,
    pub due_date: Option<String>,
    pub priority: Option<u8>,
    pub tags: Vec<String>,
}

/// A parsed event line from a memo's markdown content.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedEvent {
    pub index: usize,
    pub date: String,
    pub time: Option<String>,
    pub summary: String,
    pub raw_line: String,
    pub line_number: usize,
    pub all_day: bool,
}

static RE_TASK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^-\s+\[([ xX])\]\s+(.*)$").unwrap()
});

static RE_DUE_DATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:📅|due:?)\s*(\d{4}-\d{2}-\d{2})").unwrap()
});

static RE_PRIORITY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:🔴|🟡|🟢|p(\d))").unwrap()
});

static RE_TAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"#([a-zA-Z][a-zA-Z0-9_-]*)").unwrap()
});

/// Matches: 📅 2026-04-15 14:30 Doctor appointment
/// Or:     📅 2026-04-15 Doctor appointment
/// Or:     📅 2026-04-15
static RE_EVENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^📅\s+(\d{4}-\d{2}-\d{2})(?:\s+(\d{1,2}:\d{2}))?(?:\s+(.+))?$").unwrap()
});

// ─── Task parsing ──────────────────────────────────────────────────────

/// Parse all task lines from a memo's content.
pub fn parse_tasks(content: &str) -> Vec<ParsedTask> {
    let mut tasks = Vec::new();
    let mut task_index = 0;

    for (line_num, line) in content.lines().enumerate() {
        if let Some(caps) = RE_TASK.captures(line) {
            let done = &caps[1] != " ";
            let text = caps[2].to_string();

            let due_date = RE_DUE_DATE.captures(&text)
                .and_then(|c| c.get(1).map(|m| m.as_str().to_string()));

            let priority = RE_PRIORITY.captures(&text)
                .and_then(|c| c.get(1).and_then(|m| m.as_str().parse().ok()));

            let tags = RE_TAG.captures_iter(&text)
                .map(|c| c[1].to_string())
                .collect();

            tasks.push(ParsedTask {
                index: task_index,
                done,
                text,
                raw_line: line.to_string(),
                line_number: line_num,
                due_date,
                priority,
                tags,
            });
            task_index += 1;
        }
    }

    tasks
}

/// Check if a line is a task line.
pub fn is_task_line(line: &str) -> bool {
    RE_TASK.is_match(line)
}

/// Replace a specific task line (by line number) in the content.
pub fn replace_task_line(content: &str, line_number: usize, new_done: bool, new_text: &str) -> (String, bool) {
    let marker = if new_done { "x" } else { " " };
    let new_line = format!("- [{}] {}", marker, new_text);
    let mut lines: Vec<&str> = content.lines().collect();
    if line_number < lines.len() {
        lines[line_number] = &new_line;
        let updated = lines.join("\n");
        let changed = updated != content;
        (updated, changed)
    } else {
        (content.to_string(), false)
    }
}

/// Remove a specific task line (by line number) from the content.
pub fn remove_task_line(content: &str, line_number: usize) -> (String, bool) {
    let lines: Vec<&str> = content.lines().collect();
    if line_number < lines.len() {
        let mut new_lines = lines;
        new_lines.remove(line_number);
        let updated = new_lines.join("\n");
        (updated, true)
    } else {
        (content.to_string(), false)
    }
}

/// Append new task lines to the end of the memo content.
pub fn append_tasks(content: &str, tasks: &[ParsedTask]) -> String {
    let mut result = content.trim_end().to_string();
    for task in tasks {
        let marker = if task.done { "x" } else { " " };
        result.push('\n');
        result.push_str(&format!("- [{}] {}", marker, task.text));
    }
    result
}

/// Generate a CalDAV-friendly UID from memo name + task index.
pub fn task_uid(memo_name: &str, task_index: usize) -> String {
    let clean = memo_name.replace('/', "-");
    format!("memotag-{}-task-{}", clean, task_index)
}

// ─── Event parsing ─────────────────────────────────────────────────────

/// Parse all event lines from a memo's content.
/// Lines starting with 📅 are treated as events.
pub fn parse_events(content: &str) -> Vec<ParsedEvent> {
    let mut events = Vec::new();
    let mut event_index = 0;

    for (line_num, line) in content.lines().enumerate() {
        if let Some(caps) = RE_EVENT.captures(line) {
            let date = caps.get(1).unwrap().as_str().to_string();
            let time = caps.get(2).map(|m| m.as_str().to_string());
            let summary = caps.get(3).map(|m| m.as_str().to_string())
                .unwrap_or_else(|| "Event".to_string());
            let all_day = time.is_none();

            events.push(ParsedEvent {
                index: event_index,
                date,
                time,
                summary,
                raw_line: line.to_string(),
                line_number: line_num,
                all_day,
            });
            event_index += 1;
        }
    }

    events
}

/// Check if a line is an event line.
pub fn is_event_line(line: &str) -> bool {
    RE_EVENT.is_match(line)
}

/// Generate a CalDAV-friendly UID from memo name + event index.
pub fn event_uid(memo_name: &str, event_index: usize) -> String {
    let clean = memo_name.replace('/', "-");
    format!("memotag-{}-event-{}", clean, event_index)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── Task tests ────────────────────────────────────────────────

    #[test]
    fn test_parse_single_task() {
        let content = "Buy groceries\n- [ ] Milk\n- [x] Eggs";
        let tasks = parse_tasks(content);
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].done, false);
        assert_eq!(tasks[0].text, "Milk");
        assert_eq!(tasks[1].done, true);
        assert_eq!(tasks[1].text, "Eggs");
    }

    #[test]
    fn test_parse_task_with_due_date() {
        let tasks = parse_tasks("- [ ] File taxes 📅 2026-04-15");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].due_date.as_deref(), Some("2026-04-15"));
    }

    #[test]
    fn test_parse_task_with_priority() {
        let tasks = parse_tasks("- [x] 🔴 Urgent task");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].priority, Some(1));
    }

    #[test]
    fn test_replace_task_line() {
        let content = "Buy groceries\n- [ ] Milk\n- [x] Eggs";
        let (updated, changed) = replace_task_line(content, 1, true, "Milk");
        assert!(changed);
        assert!(updated.contains("- [x] Milk"));
    }

    #[test]
    fn test_remove_task_line() {
        let content = "Buy groceries\n- [ ] Milk\n- [x] Eggs";
        let (updated, changed) = remove_task_line(content, 1);
        assert!(changed);
        assert!(!updated.contains("Milk"));
    }

    #[test]
    fn test_task_uid() {
        let uid = task_uid("memos/m123/abc", 2);
        assert_eq!(uid, "memotag-memos-m123-abc-task-2");
    }

    // ─── Event tests ───────────────────────────────────────────────

    #[test]
    fn test_parse_event_all_day() {
        let events = parse_events("📅 2026-04-15 Doctor appointment");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].date, "2026-04-15");
        assert_eq!(events[0].summary, "Doctor appointment");
        assert!(events[0].all_day);
        assert!(events[0].time.is_none());
    }

    #[test]
    fn test_parse_event_timed() {
        let events = parse_events("📅 2026-04-15 14:30 Team standup");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].date, "2026-04-15");
        assert_eq!(events[0].time.as_deref(), Some("14:30"));
        assert_eq!(events[0].summary, "Team standup");
        assert!(!events[0].all_day);
    }

    #[test]
    fn test_parse_event_no_summary() {
        let events = parse_events("📅 2026-12-25");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].summary, "Event");
        assert!(events[0].all_day);
    }

    #[test]
    fn test_parse_multiple_events() {
        let content = "📅 2026-04-15 Doctor\n📅 2026-04-20 09:00 Meeting\nRegular text";
        let events = parse_events(content);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].summary, "Doctor");
        assert_eq!(events[1].summary, "Meeting");
    }

    #[test]
    fn test_event_uid() {
        let uid = event_uid("memos/m123/abc", 0);
        assert_eq!(uid, "memotag-memos-m123-abc-event-0");
    }

    #[test]
    fn test_is_event_line() {
        assert!(is_event_line("📅 2026-04-15 Something"));
        assert!(is_event_line("📅 2026-12-25 10:00 Party"));
        assert!(!is_event_line("Regular text"));
        assert!(!is_event_line("- [ ] task"));
    }
}
