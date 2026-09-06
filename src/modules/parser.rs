use regex::Regex;
use std::sync::LazyLock;

/// A parsed task line from a memo's markdown content.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedTask {
    /// Index of this task in the memo (0-based)
    pub index: usize,
    /// Whether the task is completed (`[x]` vs `[ ]`)
    pub done: bool,
    /// The task text (after `- [ ] `)
    pub text: String,
    /// The full raw line
    pub raw_line: String,
    /// Line number in the memo (0-based)
    pub line_number: usize,
    /// Optional due date extracted from text
    pub due_date: Option<String>,
    /// Optional priority extracted from text
    pub priority: Option<u8>,
    /// Optional tags found in the task
    pub tags: Vec<String>,
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

/// Check if a line is a task line
pub fn is_task_line(line: &str) -> bool {
    RE_TASK.is_match(line)
}

/// Replace a specific task line (by line number) in the content.
/// Returns the updated content and whether a change was made.
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

/// Generate a CalDAV-friendly UID from memo name + task index
pub fn task_uid(memo_name: &str, task_index: usize) -> String {
    let clean = memo_name.replace('/', "-");
    format!("memotag-{}-{}", clean, task_index)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(uid, "memotag-memos-m123-abc-2");
    }
}
