use anyhow::Result;
use regex::Regex;
use std::collections::HashSet;
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio::sync::Semaphore;
use tracing::{info, warn};

use super::memos::{Memo, MemosClient};
use super::parser;

// ─── Compiled once, shared by all instances ─────────────────────────────
// (?m) matters: ^ must match line starts, not just content start.
// (The old per-instance regexes without (?m) missed mid-content tasks/quotes.)

static RE_ANSI: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-9;]*[a-zA-Z]").unwrap());
static RE_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"https?://[^\s\)\]]+").unwrap());
static RE_IMAGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(<img\s|!\[[^\]]*\]\([^)]*\)|\.(png|jpe?g|gif|bmp|svg|webp|tiff?|ico|heic|heif|avif)[\s\)\]\?])").unwrap()
});
static RE_TABLE_ROW: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\|.*\|").unwrap());
static RE_FIGURE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(!\[[^\]]*\]\([^)]*\)|<figure|<img)").unwrap()
});
static RE_AUDIO: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\.(mp3|wav|ogg|m4a|flac|aac|wma|opus)[\s\)\]\?]").unwrap()
});
static RE_VIDEO: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\.(mp4|mkv|webm|avi|mov|flv|m4v|3gp|ogv)[\s\)\]\?]").unwrap()
});
/// Any fenced block: bare ``` plus every language (old list missed
/// javascript/typescript/shell/c/cpp/…).
static RE_CODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*```").unwrap());
/// Tasks anywhere: indented, -, *, +, numbered, [ ]/[x]/[X].
static RE_TASK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\s*(?:[-*+]|\d+[.)])\s+\[[ xX]\]").unwrap()
});
/// Quotes anywhere, incl. indented.
static RE_QUOTE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*>").unwrap());
/// Mermaid diagrams (fenced ```mermaid) → #diagram.
static RE_MERMAID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*```mermaid").unwrap());
/// Inline/display math: $$…$$ or \(frac|sum|…) commands → #math.
static RE_MATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\$\$.+?\$\$|\\(frac|sum|int|sqrt|alpha|beta|gamma|theta|lambda)\b").unwrap()
});
/// Known video hosts count as video even without a file extension.
static RE_VIDEO_SITE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(youtube\.com|youtu\.be|vimeo\.com|dailymotion\.com|twitch\.tv)").unwrap()
});
static RE_FILE_EXT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\.([a-z]{2,10})(?:\s|$|\)|\]|\?)").unwrap()
});
static RE_HASHTAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(\s*)#([^\s#]+)").unwrap());
/// #inbox as a standalone tag (never inside #inboxfoo).
/// No lookahead: the `regex` crate doesn't support it, so the trailing
/// delimiter is consumed and re-emitted via $2.
static RE_INBOX_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(^|\s)#inbox(\s|$)").unwrap());
static RE_UNTAGGED_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)#untagged\b").unwrap());
static RE_TASKS_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)#tasks\b").unwrap());
/// Legacy plurals the old field-based tagger wrote (#links, #images, …).
/// Folded to singular so old + new memos converge on one vocabulary.
static RE_PLURAL_TAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)#(links|images|videos|audios|quotes|codes|tables|figures)\b").unwrap()
});
static RE_JUNK_VER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d+v\d+$").unwrap());
static RE_BLANK_RUN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\n{3,}").unwrap());

/// Trailing punctuation the server strips when parsing #tags.
/// Without this we see `#link,` as tag `link,` and append a duplicate `#link`.
fn clean_tag(raw: &str) -> String {
    raw.trim_matches(|c: char| ".,;:!?…`)]}\"'".contains(c))
        .to_lowercase()
}

fn singular(plural: &str) -> &str {
    match plural {
        "links" => "link",
        "images" => "image",
        "videos" => "video",
        "audios" => "audio",
        "quotes" => "quote",
        "codes" => "code",
        "tables" => "table",
        "figures" => "figure",
        _ => plural,
    }
}

/// Fold legacy renames: #untagged (any case) → #inbox, #tasks → #task,
/// old plurals (#links, #images, …) → singular.
fn fold_renames(content: &str) -> (String, bool) {
    let after = RE_UNTAGGED_TAG.replace_all(content, "#inbox");
    let after = RE_TASKS_TAG.replace_all(&after, "#task");
    let after = RE_PLURAL_TAG.replace_all(&after, |caps: &regex::Captures| {
        format!("#{}", singular(&caps[1].to_lowercase()))
    });
    let changed = after != content;
    (after.into_owned(), changed)
}

/// Remove a standalone #inbox tag (keeps #inboxfoo etc. intact).
fn strip_inbox_tag(content: &str) -> String {
    let stripped = RE_INBOX_TAG.replace_all(content, "$1$2").into_owned();
    // Stripping can leave double spaces; collapse them (newlines handled later).
    stripped.replace("  ", " ")
}

#[derive(Clone)]
pub struct Autotagger {
    memos: MemosClient,
    default_tag: String,
    interval: Duration,
    concurrency: usize,
    known_exts: HashSet<&'static str>,
}

impl Autotagger {
    pub fn new(
        memos: MemosClient,
        default_tag: String,
        interval_secs: u64,
        concurrency: usize,
    ) -> Self {
        Self {
            memos,
            default_tag,
            interval: Duration::from_secs(interval_secs),
            concurrency: concurrency.clamp(1, 32),
            known_exts: [
                "txt", "md", "pdf", "docx", "pptx", "xlsx", "csv", "json", "yaml", "yml",
                "toml", "xml", "html", "css", "js", "ts", "jsx", "tsx", "rs", "go",
                "py", "rb", "java", "c", "cpp", "h", "sh", "bash", "zsh", "sql",
                "r", "lua", "zig", "nim", "ex", "exs", "erl", "hs", "ml", "swift",
                "kt", "scala", "cs", "fs", "vb", "php", "pl", "pm", "raku",
                "dockerfile", "makefile", "cmake", "gradle", "sbt", "cabal",
                "gitignore", "env", "lock", "log", "ini", "cfg", "conf",
                "tar", "gz", "zip", "bz2", "xz", "tgz", "7z", "rar",
                "jpg", "jpeg", "png", "gif", "bmp", "svg", "webp", "tiff", "ico", "heic", "avif",
                "mp3", "wav", "ogg", "m4a", "flac", "aac", "opus",
                "mp4", "mkv", "webm", "avi", "mov", "flv",
                "exe", "dmg", "rpm", "deb", "apk", "msi",
                "pem", "key", "crt", "cert",
                "patch", "diff",
            ]
            .into_iter()
            .collect(),
        }
    }

    fn detect_tags(&self, memo: &Memo) -> Vec<String> {
        let mut tags = Vec::new();
        let mut push = |t: &str| {
            if !tags.iter().any(|x: &String| x == t) {
                tags.push(t.to_string());
            }
        };

        for att in &memo.attachments {
            let mime = att.mime_type.to_lowercase();
            let fname = att.filename.to_lowercase();
            let is_image = mime.starts_with("image/")
                || ["jpg", "jpeg", "png", "gif", "webp", "heic", "heif", "avif", "bmp", "svg", "tiff", "tif", "ico"]
                    .iter()
                    .any(|e| fname.ends_with(&format!(".{}", e)));
            let is_audio = mime.starts_with("audio/")
                || ["mp3", "wav", "ogg", "m4a", "flac", "aac", "wma", "opus", "mid", "midi"]
                    .iter()
                    .any(|e| fname.ends_with(&format!(".{}", e)));
            let is_video = mime.starts_with("video/")
                || ["mp4", "mkv", "webm", "avi", "mov", "flv", "m4v", "3gp", "ogv"]
                    .iter()
                    .any(|e| fname.ends_with(&format!(".{}", e)));
            if is_image {
                push("image");
            }
            if is_audio {
                push("audio");
            }
            if is_video {
                push("video");
            }
        }

        // Cheap substring pre-filters before regex (fast path for plain notes).
        let clean = RE_ANSI.replace_all(&memo.content, "");
        let has_url = clean.contains("http");
        let has_fence = clean.contains("```");
        let has_pipe = clean.contains('|');

        if has_url && RE_LINK.is_match(&clean) {
            push("link");
        }
        if has_url && RE_VIDEO_SITE.is_match(&clean) {
            push("video");
        }
        if RE_IMAGE.is_match(&clean) {
            push("image");
        }
        if has_pipe && RE_TABLE_ROW.is_match(&clean) && clean.contains("---") {
            push("table");
        }
        if RE_FIGURE.is_match(&clean) {
            push("figure");
        }
        if RE_AUDIO.is_match(&clean) {
            push("audio");
        }
        if RE_VIDEO.is_match(&clean) {
            push("video");
        }
        if has_fence && RE_CODE.is_match(&clean) {
            push("code");
        }
        if has_fence && RE_MERMAID.is_match(&clean) {
            push("diagram");
        }
        if (clean.contains("$$") || clean.contains("\\frac")) && RE_MATH.is_match(&clean) {
            push("math");
        }
        // Calendar events (📅 lines) — the other half of the CalDAV sync.
        if clean.contains('📅') && !parser::parse_events(&clean).is_empty() {
            push("event");
        }
        if RE_TASK.is_match(&clean) {
            push("task");
        }
        if RE_QUOTE.is_match(&clean) {
            push("quote");
        }

        for cap in RE_FILE_EXT.captures_iter(&clean) {
            if let Some(ext) = cap.get(1) {
                let tag = ext.as_str().to_lowercase();
                if !tags.iter().any(|x: &String| x == &tag)
                    && self.known_exts.contains(tag.as_str())
                {
                    tags.push(tag);
                }
            }
        }

        if tags.is_empty() {
            tags.push(self.default_tag.clone());
        }

        tags
    }

    /// Hashtags the server would index for this content (lowercased,
    /// trailing punctuation stripped). Pub for the offline `scan` binary.
    pub fn existing_hashtags(content: &str) -> HashSet<String> {
        RE_HASHTAG
            .captures_iter(content)
            .map(|c| clean_tag(&c[2]))
            .filter(|t| !t.is_empty())
            .collect()
    }

    /// Build the new content for one memo. Returns None when already converged
    /// (no write needed). Single pass: folds + inbox cleanup + appends combine
    /// into ONE content string so each memo costs at most one PATCH.
    /// Pub for the offline `scan` binary (pitfall hunting on DB copies).
    pub fn build_new_content(&self, memo: &Memo) -> Option<(String, Vec<String>)> {
        let (mut new_content, _) = fold_renames(&memo.content);

        let existing = Self::existing_hashtags(&new_content);
        let detected = self.detect_tags(memo);

        let new_tags: Vec<String> = detected
            .iter()
            .filter(|t| !existing.contains(t.as_str()))
            .cloned()
            .collect();

        let has_inbox = existing.contains("inbox");
        let has_other = existing.iter().any(|t| t != "inbox");
        let new_non_inbox = new_tags.iter().any(|t| t != "inbox");
        let detected_non_inbox = detected.iter().any(|t| t != "inbox");
        let needs_cleanup = has_inbox && (has_other || detected_non_inbox);

        // When real tags exist/are added, #inbox is triage noise: drop it and
        // don't re-add it. When a curated memo yields only the fallback,
        // append nothing (but still write through any fold renames above).
        let final_tags: Vec<String> = if needs_cleanup || new_non_inbox {
            new_tags.into_iter().filter(|t| t != "inbox").collect()
        } else if has_other {
            Vec::new()
        } else {
            new_tags
        };

        if needs_cleanup {
            new_content = strip_inbox_tag(&new_content);
        }

        if !final_tags.is_empty() {
            let line = final_tags
                .iter()
                .map(|t| format!("#{}", t))
                .collect::<Vec<_>>()
                .join(" ");
            if new_content.ends_with('\n') {
                new_content.push_str(&line);
            } else {
                new_content.push('\n');
                new_content.push_str(&line);
            }
        }

        if RE_BLANK_RUN.is_match(&new_content) {
            new_content = RE_BLANK_RUN.replace_all(&new_content, "\n\n").into_owned();
        }

        if new_content == memo.content {
            return None;
        }
        // Normalise trailing whitespace-only diffs (inbox strip can leave " \n").
        if new_content.trim() == memo.content.trim()
            && final_tags.is_empty()
            && new_content.replace(' ', "") == memo.content.replace(' ', "")
        {
            return None;
        }
        Some((new_content, final_tags))
    }

    /// Process one memo. Returns true when it changed (or would change under
    /// dry_run). Never writes under dry_run.
    async fn process_memo(&self, memo: Memo, dry_run: bool) -> Result<bool> {
        let Some((new_content, added)) = self.build_new_content(&memo) else {
            return Ok(false);
        };
        if dry_run {
            info!(
                "dry-run: would update {} (+[{}])",
                memo.name,
                added.join(", ")
            );
            return Ok(true);
        }
        if self.memos.update_memo(&memo.name, &new_content).await? {
            if added.is_empty() {
                info!("cleaned tags on {}", memo.name);
            } else {
                info!("tagged {} with [{}]", memo.name, added.join(", "));
            }
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Single pass over all memos. Returns (scanned, changed).
    /// Memos in a page are processed concurrently (bounded by concurrency).
    pub async fn run_once(&self, dry_run: bool) -> Result<(usize, usize)> {
        let mut page_token: Option<String> = None;
        let mut scanned = 0usize;
        let mut changed = 0usize;

        loop {
            let (memos, next) = self.memos.list_memos(page_token.as_deref()).await?;
            let sem = Arc::new(Semaphore::new(self.concurrency));
            let mut set = tokio::task::JoinSet::new();

            for memo in memos {
                if memo.content.trim().is_empty() && memo.attachments.is_empty() {
                    continue;
                }
                scanned += 1;
                let tagger = self.clone();
                let permit_src = sem.clone();
                set.spawn(async move {
                    let _permit = permit_src.acquire_owned().await.map_err(|e| {
                        anyhow::anyhow!("semaphore closed: {}", e)
                    })?;
                    tagger.process_memo(memo, dry_run).await
                });
            }

            while let Some(res) = set.join_next().await {
                match res {
                    Ok(Ok(true)) => changed += 1,
                    Ok(Ok(false)) => {}
                    Ok(Err(e)) => warn!("memo processing failed: {}", e),
                    Err(e) => warn!("memo task panicked: {}", e),
                }
            }

            page_token = next;
            if page_token.is_none() {
                break;
            }
        }

        info!(
            "autotag pass done: scanned={} changed={} dry_run={}",
            scanned, changed, dry_run
        );
        Ok((scanned, changed))
    }

    pub async fn run(&self) -> Result<()> {
        info!(
            "autotagger starting: default_tag={}, interval={}s, concurrency={}",
            self.default_tag,
            self.interval.as_secs(),
            self.concurrency,
        );

        loop {
            if let Err(e) = self.run_once(false).await {
                warn!("error in autotagger loop: {}", e);
            }
            tokio::time::sleep(self.interval).await;
        }
    }

    fn is_junk_hashtag(tag: &str) -> bool {
        let t = tag.to_lowercase();
        let bytes = t.as_bytes();
        if t.len() == 2
            && bytes[0].is_ascii_alphabetic()
            && bytes[1].is_ascii_digit()
        {
            return true;
        }
        if RE_JUNK_VER.is_match(&t) {
            return true;
        }
        if t.len() <= 4
            && bytes.first().is_some_and(|b| b.is_ascii_digit())
        {
            return true;
        }
        false
    }

    pub async fn clean_junk_hashtags(&self, dry_run: bool) -> Result<()> {
        info!(
            "clean mode: removing junk hashtags from all memos (dry_run={})",
            dry_run
        );
        let mut page_token: Option<String> = None;
        let mut total_cleaned = 0;

        loop {
            let (memos, next) = self.memos.list_memos(page_token.as_deref()).await?;

            for memo in memos {
                if memo.content.trim().is_empty() {
                    continue;
                }

                let mut new_content = memo.content.clone();
                let mut removals: Vec<(usize, usize)> = Vec::new();
                for cap in RE_HASHTAG.captures_iter(&memo.content) {
                    if Self::is_junk_hashtag(&cap[2]) {
                        let m = cap.get(0).unwrap();
                        removals.push((m.start(), m.end()));
                    }
                }

                for (start, end) in removals.into_iter().rev() {
                    new_content.drain(start..end);
                }

                if new_content == memo.content {
                    continue;
                }
                new_content = RE_BLANK_RUN.replace_all(&new_content, "\n\n").into_owned();

                if dry_run {
                    info!("dry-run: would clean {}", memo.name);
                    total_cleaned += 1;
                    continue;
                }
                if self.memos.update_memo(&memo.name, &new_content).await? {
                    total_cleaned += 1;
                    info!("cleaned memo {}", memo.name);
                }
            }

            page_token = next;
            if page_token.is_none() {
                break;
            }
        }

        info!("cleaned {} memos", total_cleaned);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::memos::Attachment;

    fn tagger() -> Autotagger {
        Autotagger::new(
            MemosClient::new("http://127.0.0.1:1".into(), "test".into()),
            "inbox".into(),
            60,
            4,
        )
    }

    fn memo(content: &str) -> Memo {
        Memo {
            name: "memos/test".into(),
            content: content.into(),
            attachments: vec![],
            create_time: String::new(),
            update_time: String::new(),
        }
    }

    fn memo_att(content: &str, filename: &str, mime: &str) -> Memo {
        Memo {
            name: "memos/test".into(),
            content: content.into(),
            attachments: vec![Attachment {
                filename: filename.into(),
                mime_type: mime.into(),
            }],
            create_time: String::new(),
            update_time: String::new(),
        }
    }

    fn detected(content: &str) -> Vec<String> {
        tagger().detect_tags(&memo(content))
    }

    #[test]
    fn plain_falls_back_to_inbox() {
        assert_eq!(detected("just lunch thoughts"), vec!["inbox"]);
    }

    #[test]
    fn links() {
        assert!(detected("read https://example.com/a").contains(&"link".to_string()));
        assert!(detected("[t](https://example.com/a)").contains(&"link".to_string()));
    }

    #[test]
    fn images() {
        for c in [
            "look ![cat](https://x/y.png)",
            "file https://cdn/x/photo.JPG)",
            "<img src=\"x\">",
        ] {
            assert!(detected(c).contains(&"image".to_string()), "miss: {}", c);
        }
    }

    #[test]
    fn audio_video() {
        assert!(detected("ep https://cdn/x/e.mp3)").contains(&"audio".to_string()));
        assert!(detected("clip https://cdn/x/c.mp4)").contains(&"video".to_string()));
        assert!(detected("song https://cdn/x/s.ogg)").contains(&"audio".to_string()));
        assert!(detected("film https://cdn/x/f.webm)").contains(&"video".to_string()));
    }

    #[test]
    fn code_any_fence() {
        // Old regex missed bare fences and most languages.
        for c in [
            "```rust\nfn main() {}\n```",
            "```\ngeneric\n```",
            "```javascript\nconsole.log(1)\n```",
            "```typescript\nconst x = 1\n```",
            "notes\n```python\nprint(1)\n```\ndone",
            "```c\nint main(){}\n```",
            "```shell\necho hi\n```",
        ] {
            assert!(detected(c).contains(&"code".to_string()), "miss: {:?}", c);
        }
    }

    #[test]
    fn tasks_anywhere() {
        // Old ^-anchored regex missed every non-leading task.
        for c in [
            "- [ ] buy milk",
            "- [x] done",
            "- [X] upper",
            "notes:\n  - [ ] indented",
            "* [ ] star",
            "+ [ ] plus",
            "1. [ ] numbered",
            "10) [x] paren",
            "intro\n- [ ] mid-doc\noutro",
        ] {
            assert!(detected(c).contains(&"task".to_string()), "miss: {:?}", c);
        }
    }

    #[test]
    fn quotes_anywhere() {
        for c in [
            "> wise words",
            "  > indented",
            "intro\n> quoted\noutro",
        ] {
            assert!(detected(c).contains(&"quote".to_string()), "miss: {:?}", c);
        }
    }

    #[test]
    fn events_math_diagrams_video_sites() {
        // Calendar events (mirrors the CalDAV parser, not just the emoji).
        assert!(detected("📅 2026-04-15 Doctor appointment").contains(&"event".to_string()));
        assert!(detected("📅 2026-04-15 14:30 Standup").contains(&"event".to_string()));
        // Bare emoji with no date is not an event.
        assert!(!detected("I love 📅 emojis").contains(&"event".to_string()));
        // Math.
        assert!(detected("result $$x^2 + y$$ ok").contains(&"math".to_string()));
        assert!(detected("use \\frac{a}{b} here").contains(&"math".to_string()));
        assert!(!detected("price is 5 dollars").contains(&"math".to_string()));
        assert!(!detected("print this").contains(&"math".to_string()));
        // Mermaid diagrams (also still #code — it is a fence).
        let d = detected("```mermaid\ngraph TD\n```");
        assert!(d.contains(&"diagram".to_string()));
        assert!(d.contains(&"code".to_string()));
        // Video sites → video (+link).
        let v = detected("watch https://www.youtube.com/watch?v=abc123");
        assert!(v.contains(&"video".to_string()));
        assert!(v.contains(&"link".to_string()));
        let v2 = detected("clip https://vimeo.com/12345");
        assert!(v2.contains(&"video".to_string()));
    }

    #[test]
    fn tables_and_figures() {
        assert!(detected("| a | b |\n|---|---|\n| 1 | 2 |").contains(&"table".to_string()));
        // A pipe without a separator row is not a table.
        assert!(!detected("a | b pipe text").contains(&"table".to_string()));
        assert!(detected("<figure><img></figure>").contains(&"figure".to_string()));
    }

    #[test]
    fn attachments() {
        let t = tagger();
        assert!(t.detect_tags(&memo_att("", "photo.heic", "application/octet-stream")).contains(&"image".to_string()));
        assert!(t.detect_tags(&memo_att("", "song.opus", "application/octet-stream")).contains(&"audio".to_string()));
        assert!(t.detect_tags(&memo_att("", "film.mkv", "application/octet-stream")).contains(&"video".to_string()));
        assert!(t.detect_tags(&memo_att("", "x", "image/png")).contains(&"image".to_string()));
        assert!(t.detect_tags(&memo_att("", "x", "audio/mpeg")).contains(&"audio".to_string()));
        assert!(t.detect_tags(&memo_att("", "x", "video/mp4")).contains(&"video".to_string()));
    }

    #[test]
    fn hashtags_parsed_and_cleaned() {
        let set = Autotagger::existing_hashtags("a #Rust b #link, c #selfhosted!");
        assert!(set.contains("rust"));
        assert!(set.contains("link"));
        assert!(set.contains("selfhosted"));
        assert!(!set.contains("link,"));
        let ko = Autotagger::existing_hashtags("오늘 #일기 썼다");
        assert!(ko.contains("일기"));
    }

    #[test]
    fn folds_and_inbox_strip() {
        let (s, changed) = fold_renames("a #Untagged b #TASKS c #Links d #IMAGES e");
        assert!(changed);
        for good in ["#inbox", "#task", "#link", "#image"] {
            assert!(s.contains(good), "missing {} in {:?}", good, s);
        }
        assert!(!s.contains("#Untagged") && !s.contains("#TASKS"));
        assert!(!s.contains("#Links") && !s.contains("#IMAGES"));
        // #taskfoo must survive a #tasks fold; #linksup survives plural fold.
        let (s, _) = fold_renames("#taskfoo #tasks #linksup #links");
        assert!(s.contains("#taskfoo") && s.contains("#linksup"));

        assert_eq!(strip_inbox_tag("a #inbox b"), "a b");
        assert_eq!(strip_inbox_tag("#inbox a"), " a");
        // Substring tags are untouched.
        assert_eq!(strip_inbox_tag("#inboxfoo #inbox"), "#inboxfoo ");
    }

    #[test]
    fn converged_memo_returns_none() {
        let t = tagger();
        // Already has the tag content implies → no write.
        assert!(t.build_new_content(&memo("read https://example.com/a #link")).is_none());
        // Plain memo needs #inbox.
        let (content, added) = t.build_new_content(&memo("plain")).unwrap();
        assert_eq!(added, vec!["inbox"]);
        assert!(content.ends_with("#inbox"));
    }

    #[test]
    fn inbox_removed_when_real_tag_added() {
        let t = tagger();
        let (content, added) = t
            .build_new_content(&memo("read https://example.com/a #inbox"))
            .unwrap();
        assert!(added.contains(&"link".to_string()));
        assert!(!added.contains(&"inbox".to_string()));
        assert!(!RE_INBOX_TAG.is_match(&content));
        assert!(content.contains("#link"));
    }

    #[test]
    fn curated_memos_keep_no_inbox() {
        // Memo with real tags + plain content: detection yields only the
        // fallback, which must NOT be appended to curated memos.
        let t = tagger();
        assert!(t.build_new_content(&memo("plain thoughts #rust")).is_none());
    }

    #[test]
    fn typo_folds_write_without_inbox() {
        // #links/#images are folded to singular even though detection only
        // yields the fallback — and no #inbox is appended to the real tags.
        let t = tagger();
        let (content, added) = t
            .build_new_content(&memo("notes\n[seed:x] #links #images #codes"))
            .unwrap();
        assert!(added.is_empty());
        for good in ["#link", "#image", "#code"] {
            assert!(content.contains(good), "missing {} in {:?}", good, content);
        }
        assert!(!content.contains("#links") && !content.contains("#codes"));
        assert!(!content.contains("#inbox"));
    }

    #[test]
    fn backfills_new_rules_on_tagged_memos() {
        // A memo tagged #link that later gains a code block must get #code.
        // (The old early-skip returned None here and never backfilled.)
        let t = tagger();
        let (content, added) = t
            .build_new_content(&memo("read https://e.com/a #link\n```rust\nx\n```"))
            .unwrap();
        assert!(added.contains(&"code".to_string()));
        assert!(content.contains("#code") && content.contains("#link"));
    }

    #[test]
    fn junk_tags() {
        assert!(Autotagger::is_junk_hashtag("a1"));
        assert!(Autotagger::is_junk_hashtag("12v3"));
        assert!(Autotagger::is_junk_hashtag("123"));
        assert!(!Autotagger::is_junk_hashtag("link"));
        assert!(!Autotagger::is_junk_hashtag("selfhosted"));
        // Short leading-digit tags are treated as junk by clean mode
        // (pre-existing policy: e.g. OCR/version fragments like #3v2).
        assert!(Autotagger::is_junk_hashtag("2026"));
        assert!(!Autotagger::is_junk_hashtag("2026notes")); // len>4, not ver-pattern
    }
}
