# freshrss-rs

A self-hosted, multi-user web RSS reader. The backend is a Rust port of
[FreshRSS](https://github.com/FreshRSS/FreshRSS) on `axum` + SQLite; the frontend reproduces the
[Feeddi](https://feeddi.com) reader UI.

## Quick start

```sh
# 1. Build the SPA into static/
npm --prefix web install
npm --prefix web run build

# 2. Build and run the server (creates and migrates data/freshrss.sqlite)
cargo run -- serve
# → http://127.0.0.1:8080
```

For frontend development with hot reload, run the server and Vite side by side:

```sh
cargo run -- serve                 # API on :8080
npm --prefix web run dev           # UI on :5173, proxies /api to :8080
```

In debug builds the server reads `static/` from disk on every request, so a `npm run build`
shows up without recompiling. In release builds `static/` is embedded into the binary by
`rust-embed`, so build the SPA **before** `cargo build --release` to get a single-file deploy.

Other subcommands: `freshrss-rs migrate` (apply schema migrations and exit).

## Configuration

Copy `config.example.toml` to `config.toml`, or point `FRESHRSS_CONFIG` at a file. Every key can
also be set through an environment variable named `FRESHRSS_<SECTION>_<KEY>`, for example
`FRESHRSS_SERVER_BIND=0.0.0.0:8080`.

The session secret defaults to a placeholder. The server refuses to start with it unless
`session.allow_insecure_secret = true`, which is set in the shipped defaults for development only.

## Layout

```
crates/
  freshrss-search/   BooleanSearch query language: lexer → parser → AST → {SQL, matcher}
  freshrss-feed/     RSS/Atom/JSON Feed parsing, sanitization, GUID/hash identity, discovery, OPML
  freshrss-server/   domain types, SQLite DAOs, services, axum API, CLI, refresh scheduler
web/                 Svelte 5 + TypeScript SPA (Vite → ../static)
fixtures/            offline feed/HTML/OPML samples used by the tests
tools/               maintenance scripts
```

Dependency direction is one-way: `server → {feed, search}`, and neither `feed` nor `search`
depends on the other or on axum/tokio, so both are testable from fixtures with no IO.

## Ported behaviour

The following are deliberate ports of FreshRSS semantics rather than inventions:

- Entry identity — a feed-derived `guid` plus an MD5 `hash` over link/title/authors/content/tags
  used to detect upstream edits. The `hash` excludes the publication date on purpose.
- Entry `id` — a microsecond-scale, time-ordered integer, which is what makes `date:` searches and
  keyset pagination work.
- Listing scopes (`a`/`A`/`Z`/`i`/`s`/`S`/`c`/`f`/`t`/`T`/`ST`) and the entry state bitmask.
- The BooleanSearch grammar: `intitle:`, `intext:`, `author:`, `inurl:`, `#tag`, `f:`, `c:`, `L:`,
  `labels:`, `date:`, `pubdate:`, `mdate:`, `userdate:`, `S:`/`search:`, quoted phrases,
  `/regex/im`, `!`/`-` negation, `OR`, and parentheses.
- The refresh pipeline: conditional GET, unchanged-body detection, insert/update/lastSeen
  reconciliation, retention (`keep_period`, `keep_max`, `keep_min`, `keep_favourites`,
  `keep_labels`, `keep_unreads`), and filter actions (`read`, `star`, `label`).
- Full-text extraction: `pathEntries` and `path_entries_filter`, including the
  `<meta http-equiv="refresh">` hop and the `content_action` prepend/append/replace modes.

The API is guarded by a cross-origin check on mutations (a present `Origin` that is not the
`Host` is refused) and a per-session, per-minute limit on the endpoints that cost money: logins,
discovery, single-feed refreshes and whole-tree refreshes. Refreshes run as jobs — `POST
/api/v1/refresh` answers `202` with an id the client polls — because a fetch can outlast any
timeout a browser or proxy would tolerate.

Divergences from FreshRSS, and why, are documented in the plan and in the module docs. The three
that matter most:

1. **Clean UTF-8 at rest.** FreshRSS stores text columns HTML-encoded and re-encodes every search
   term to match. We store plain text, which removes that whole dance and makes search work on the
   text users actually see. Consequence: a PHP FreshRSS database cannot be attached directly.
2. **One shared SQLite database.** FreshRSS gives each user their own file. Sharing one database
   means entry ids must come from a global allocator rather than FreshRSS's
   `MAX(id) - COUNT(*)` renumbering, which would collide across users.
3. **Folded columns instead of `LIKE`.** SQLite's `LIKE` is ASCII-case-insensitive only, and the
   database driver cannot register a function that could do better. So every searchable column
   carries an accent-folded copy written at ingest (`title_folded`, `tags_folded`, …), searches
   compare folded to folded with `instr()`, and the in-memory matcher folds with the same function —
   so listing queries and filter actions cannot disagree, a bug FreshRSS itself has. A
   `fold_version` key re-runs the backfill if the fold ever changes.
4. **No `/regex/` in a listing.** SQLite has no regex operator without a user-defined function, so a
   regex in a search box is a `400` that names the pattern. Filter actions still evaluate regexes,
   because they run in Rust over the parsed feed rather than in SQL. The two backends differ only in
   a loud refusal against an answer, never in two answers.

## Tests

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
npm --prefix web run check
node web/tools/browser-check.mjs http://127.0.0.1:8080 <user> <password>
```

Tests are offline: feed parsing runs against `fixtures/`, and HTTP behaviour is exercised through
the `Fetcher` trait rather than the network.

The last command drives the built reader in a real Chromium: sign-in, the first page, infinite
scroll with a no-duplicate-rows assertion, opening an article, `j k s m u b / Escape`, sorting,
searching, mark-all-read and all three themes — 52 checks, and it fails on any console error. It
needs a running server with a user that has some articles.

## License

AGPL-3.0-or-later, matching FreshRSS, from which this is derived. See [LICENSE](LICENSE).

The reader UI's colour tokens, typography and markup conventions are reproduced from Feeddi.
The bundled fonts are Literata and Atkinson Hyperlegible Next; icons are
[Remixicon](https://remixicon.com) 4.5.0 (Apache-2.0).
