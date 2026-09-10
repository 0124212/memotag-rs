//! Offline pitfall hunter: runs the tagger's pure decision logic over every
//! memo in a (read-only) sqlite copy and reports what WOULD change.
//!
//! Usage: scan <path-to-memos_prod_copy.db>
//! Prints aggregate counts + memo UIDs for flagged classes. Never bodies.
//! Exit 0 = converged & clean, 1 = non-convergent memos or fence hazards.

use memotag_rs::modules::autotag::Autotagger;
use memotag_rs::modules::memos::{Attachment, Memo, MemosClient};
use regex::Regex;
use rusqlite::{Connection, Result as SqlResult};
use std::collections::{BTreeMap, HashMap};

const RULE_TAGS: &[&str] = &[
    "link", "image", "figure", "table", "audio", "video", "code", "task", "quote", "inbox",
];

fn main() -> anyhow::Result<()> {
    let db_path = std::env::args()
        .nth(1)
        .expect("usage: scan <path-to-db-copy>");
    let conn = Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;

    // memo.id -> attachments
    let mut atts: HashMap<i64, Vec<Attachment>> = HashMap::new();
    let mut stmt = conn.prepare("SELECT memo_id, filename, type FROM attachment WHERE memo_id IS NOT NULL")?;
    let rows: SqlResult<Vec<(Option<i64>, String, String)>> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<SqlResult<_>>();
    for (mid, filename, mime) in rows? {
        if let Some(mid) = mid {
            atts.entry(mid).or_default().push(Attachment {
                filename,
                mime_type: mime,
            });
        }
    }

    let tagger = Autotagger::new(
        MemosClient::new("http://127.0.0.1:1".into(), "scan".into()),
        "inbox".into(),
        60,
        4,
    );

    let re_url_frag = Regex::new(r"https?://\S*#([^\s\)\]]+)").unwrap();
    let re_fence = Regex::new(r"(?s)```.*?```").unwrap();
    let re_fold_target =
        Regex::new(r"(?i)#(untagged|tasks|links|images|videos|audios|quotes|codes|tables|figures)\b")
            .unwrap();
    let re_html = Regex::new(r"(?i)<(figure|img|video|audio|table|div|span|iframe)\b").unwrap();

    let mut scanned = 0usize;
    let mut skipped_empty = 0usize;
    let mut would_change = 0usize;
    let mut added_freq: BTreeMap<String, usize> = BTreeMap::new();
    let mut cleanup_only = 0usize;
    let mut nonconvergent: Vec<String> = vec![];
    let mut frag_phantom: Vec<String> = vec![];
    let mut frag_blocks_rule = 0usize;
    let mut fence_hazards: Vec<String> = vec![];
    let mut html_docs = 0usize;
    let mut html_would_change = 0usize;

    let mut stmt =
        conn.prepare("SELECT id, uid, content FROM memo WHERE row_status='NORMAL'")?;
    let memos: Vec<(i64, String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<SqlResult<_>>()?;

    for (id, uid, content) in &memos {
        let memo = Memo {
            name: format!("memos/{}", uid),
            content: content.clone(),
            attachments: atts.remove(id).unwrap_or_default(),
            create_time: String::new(),
            update_time: String::new(),
        };
        if memo.content.trim().is_empty() && memo.attachments.is_empty() {
            skipped_empty += 1;
            continue;
        }
        scanned += 1;
        let is_html = re_html.is_match(&memo.content);
        if is_html {
            html_docs += 1;
        }

        // Fragment phantoms under CURRENT hashtag parsing.
        for cap in re_url_frag.captures_iter(&memo.content) {
            let frag = cap[1]
                .trim_matches(|c: char| ".,;:!?…`)]}\"'".contains(c))
                .to_lowercase();
            if frag.is_empty() {
                continue;
            }
            // Current logic counts any #... even mid-URL as existing hashtag.
            let counted = Autotagger::existing_hashtags(&format!("x #{}", frag)).contains(&frag);
            if counted {
                frag_phantom.push(format!("{}#{}", uid, frag));
                if RULE_TAGS.contains(&frag.as_str()) {
                    frag_blocks_rule += 1;
                }
                break;
            }
        }

        // Fence hazard: a fold target inside a code fence would be rewritten
        // inside the user's code sample.
        let mut fenced = String::new();
        for m in re_fence.find_iter(&memo.content) {
            fenced.push_str(m.as_str());
            fenced.push('\n');
        }
        if !fenced.is_empty() && re_fold_target.is_match(&fenced) {
            fence_hazards.push(uid.clone());
        }

        match tagger.build_new_content(&memo) {
            None => {}
            Some((new_content, added)) => {
                would_change += 1;
                if is_html {
                    html_would_change += 1;
                }
                for t in &added {
                    *added_freq.entry(t.clone()).or_default() += 1;
                }
                if added.is_empty() {
                    cleanup_only += 1;
                }
                // Convergence: applying the output must yield None,
                // else this memo would be rewritten every interval forever.
                let memo2 = Memo {
                    content: new_content,
                    ..memo.clone()
                };
                if tagger.build_new_content(&memo2).is_some() {
                    nonconvergent.push(uid.clone());
                }
            }
        }
    }

    println!("scanned={} skipped_empty={}", scanned, skipped_empty);
    println!("would_change={} (cleanup_only={})", would_change, cleanup_only);
    println!("added_tag_frequency={:?}", added_freq);
    println!("html_docs={} html_would_change={}", html_docs, html_would_change);
    println!("url_fragment_phantoms={} (of those blocking_a_rule_tag={})", frag_phantom.len(), frag_blocks_rule);
    for f in frag_phantom.iter().take(20) {
        println!("  frag: {}", f);
    }
    println!("fence_fold_hazards={}", fence_hazards.len());
    for u in fence_hazards.iter().take(20) {
        println!("  fence: {}", u);
    }
    println!("nonconvergent={}", nonconvergent.len());
    for u in nonconvergent.iter().take(20) {
        println!("  nonconv: {}", u);
    }

    if !nonconvergent.is_empty() || !fence_hazards.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}
