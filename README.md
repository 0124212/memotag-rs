# memotag-rs

Rust daemon for [Memos](https://github.com/usememos/memos): content-based
auto-tagging plus CalDAV task/event sync. Prod runs on ubuntu/oracle as
`ghcr.io/0124212/memotag-rs:main` next to `memos` (neosmemo/memos:stable).

## Tag rules (content → `#hashtag` appended to the memo)

Memos 0.30 treats `tags` as **output-only** (parsed from `#tags` in content),
so the tagger PATCHes `content`, never a tags field.

- `link` — bare `https://…` URLs
- `image` — markdown `![](…)`, `<img>`, image extensions (also via attachments)
- `figure` — markdown-image syntax or `<figure>`/`<img>` HTML
- `table` — `|…|` rows plus a `---` separator
- `audio` / `video` — media extensions (also via attachments)
- `code` — any fenced block (` ``` `, bare or any language)
- `task` — `- [ ]` / `- [x]` anywhere: indented, `*`/`+`, numbered
- `quote` — `> blockquote` on any line, incl. indented
- `<ext>` — known file extensions (`pdf`, `rs`, `mp3`, …) as extra tags
- fallback `inbox` (configurable) only for memos with **no** tags at all

Folds (single write per memo): `#untagged`→`#inbox`, `#tasks`→`#task`,
legacy plurals (`#links`, `#images`, …)→singular; strips stale `#inbox`
once real tags exist. Curated memos (real tags, no inbox) are never
force-tagged with the fallback, but new rule hits still backfill.

Known server quirk: a doc containing HTML (`<figure>`, `<img>`…) gets
`tags=null` back — the 0.30 extractor bails on the whole doc. Hashtags stay
in content (searchable) and converge (no rewrite loop).

## Run

```bash
export MEMOS_URL="http://memos:5230"          # prod (from inside compose net)
export MEMOS_API_TOKEN="memos_pat_xxx"        # required
export AUTOTAG_DEFAULT_TAG="inbox"            # optional
export AUTOTAG_INTERVAL=300                   # optional, seconds
export AUTOTAG_CONCURRENCY=8                  # optional, 1-32 (AUTOTAG_BATCH_SIZE also read)

cargo run --release -- --once --dry-run  # test pass: log only, no writes
cargo run --release -- --once             # single live pass, then exit
cargo run --release -- --clean --dry-run  # preview junk-hashtag cleanup
memotag-rs                                # daemon: interval loop + webhook server
```

## Testing (debian dummy, before touching ubuntu)

Prod lives on ubuntu — never test against it. Use the AI-controlled dummy:

```bash
cd /root/memos-test
docker compose up -d     # http://127.0.0.1:5230, same image as prod
python3 seed.py          # 27 edge-case memos + typo/good fixtures
MEMOS_URL=http://127.0.0.1:5230 MEMOS_API_TOKEN=$MEMOS_TEST_PAT \
  /root/code/memotag-rs/target/release/memotag-rs --once --dry-run
MEMOS_URL=http://127.0.0.1:5230 MEMOS_API_TOKEN=$MEMOS_TEST_PAT \
  /root/code/memotag-rs/target/release/memotag-rs --once
python3 verify.py        # must PASS before any ubuntu deploy
```

`cargo test` runs 17 autotag unit tests (no server needed). Two unrelated
tests (caldav vevent, parser priority) fail on `main` — pre-existing, untouched.

## Deploy (ubuntu)

Push `main` → GHCR builds `:main` → on oracle: `docker compose pull
memotag-rs && docker compose up -d memotag-rs`.
