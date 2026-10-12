-- freshrss-rs V1 — multi-user SQLite schema.
--
-- Ported from FreshRSS's app/SQL/install.sql.sqlite.php, with the deltas required by
-- sharing one database between users (every owned row carries id_user, and every
-- listing index leads with id_user or id_feed).
--
-- Text columns hold clean UTF-8. FreshRSS stores title/author/content/tags/name/description
-- HTML-encoded at rest; we deliberately do not (see the plan's Divergences §1).

CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE user (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    username      TEXT    NOT NULL COLLATE NOCASE,
    email         TEXT,
    password_hash TEXT    NOT NULL,
    is_admin      INTEGER NOT NULL DEFAULT 0,
    enabled       INTEGER NOT NULL DEFAULT 1,
    created_at    INTEGER NOT NULL,
    last_seen_at  INTEGER,
    -- UserConfiguration: ttl_default, archiving, mark_when, sort, filters, ...
    config        TEXT    NOT NULL DEFAULT '{}',
    attributes    TEXT,
    UNIQUE (username)
);

CREATE TABLE session (
    id         TEXT    PRIMARY KEY,
    id_user    INTEGER NOT NULL REFERENCES user (id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    ip         TEXT,
    user_agent TEXT
);
CREATE INDEX session_id_user_index ON session (id_user);
CREATE INDEX session_expires_index ON session (expires_at);

CREATE TABLE category (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    id_user    INTEGER NOT NULL REFERENCES user (id) ON DELETE CASCADE,
    name       TEXT    NOT NULL,
    -- 0 = normal, 2 = dynamic OPML (unsupported in v1)
    kind       INTEGER NOT NULL DEFAULT 0,
    lastUpdate INTEGER NOT NULL DEFAULT 0,
    -- 0 = ok, otherwise the unix time of the last failure
    error      INTEGER NOT NULL DEFAULT 0,
    position   INTEGER NOT NULL DEFAULT 0,
    attributes TEXT,
    UNIQUE (id_user, name)
);

CREATE TABLE feed (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    id_user           INTEGER NOT NULL REFERENCES user (id) ON DELETE CASCADE,
    url               TEXT    NOT NULL,
    -- 0 RSS, 2 RSS-forced, 10 HTML+XPath, 15 XML+XPath, 20 JSON+XPath,
    -- 25 JSON Feed, 30 JSON dot-notation, 35 HTML+XPath+JSON
    kind              INTEGER NOT NULL DEFAULT 0,
    category          INTEGER REFERENCES category (id) ON DELETE SET NULL,
    name              TEXT    NOT NULL,
    website           TEXT    NOT NULL DEFAULT '',
    description       TEXT    NOT NULL DEFAULT '',
    lastUpdate        INTEGER NOT NULL DEFAULT 0,
    -- 20 important, 10 main stream, 0 category-visibility, -5 feed-only, -10 hidden
    priority          INTEGER NOT NULL DEFAULT 10,
    pathEntries       TEXT,
    pathEntriesFilter TEXT,
    -- base64 of "user:password"
    httpAuth          TEXT,
    -- 0 = ok, otherwise the unix time of the last failed refresh
    error             INTEGER NOT NULL DEFAULT 0,
    -- negative means muted; 0 means "use the owner's ttl_default"
    ttl               INTEGER NOT NULL DEFAULT 0,
    attributes        TEXT,
    cache_nbEntries   INTEGER NOT NULL DEFAULT 0,
    cache_nbUnreads   INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX feed_id_user_index ON feed (id_user, id);
CREATE INDEX feed_category_index ON feed (id_user, category);
CREATE INDEX feed_priority_index ON feed (id_user, priority);
CREATE INDEX feed_name_index ON feed (id_user, name);

-- id is a microsecond-scale, globally monotonic timestamp allocated by service/idgen.rs.
-- FreshRSS derives it as `rowid + (MAX(id) - COUNT(*))` over the staging table, which is
-- only collision-free under its one-database-per-user assumption; see Divergences §2.
CREATE TABLE entry (
    id               INTEGER PRIMARY KEY,
    id_user          INTEGER NOT NULL REFERENCES user (id) ON DELETE CASCADE,
    guid             TEXT    NOT NULL,
    title            TEXT    NOT NULL,
    author           TEXT    NOT NULL DEFAULT '',
    content          TEXT    NOT NULL DEFAULT '',
    link             TEXT    NOT NULL,
    date             INTEGER NOT NULL DEFAULT 0,
    lastSeen         INTEGER NOT NULL DEFAULT 0,
    lastModified     INTEGER,
    lastUserModified INTEGER,
    hash             BLOB,
    is_read          INTEGER NOT NULL DEFAULT 0,
    is_favorite      INTEGER NOT NULL DEFAULT 0,
    id_feed          INTEGER NOT NULL REFERENCES feed (id) ON DELETE CASCADE,
    tags             TEXT    NOT NULL DEFAULT '',
    attributes       TEXT,
    UNIQUE (id_feed, guid)
);
CREATE INDEX entry_id_user_index ON entry (id_user, id);
CREATE INDEX entry_read_index ON entry (id_user, is_read, id);
CREATE INDEX entry_favorite_index ON entry (id_user, is_favorite, id);
CREATE INDEX entry_last_seen_index ON entry (id_user, lastSeen);
CREATE INDEX entry_feed_read_index ON entry (id_feed, is_read);
CREATE INDEX entry_feed_last_seen_index ON entry (id_feed, lastSeen);
CREATE INDEX entry_date_index ON entry (id_user, date);

-- Staging table for bulk insert. Mirrors entry minus the two modification timestamps.
-- `id` here is provisional (the microsecond clock at stage time); the commit step
-- reassigns dense ids from the global allocator, ordered by (date, id).
CREATE TABLE entrytmp (
    id          INTEGER NOT NULL,
    id_user     INTEGER NOT NULL REFERENCES user (id) ON DELETE CASCADE,
    guid        TEXT    NOT NULL,
    title       TEXT    NOT NULL,
    author      TEXT    NOT NULL DEFAULT '',
    content     TEXT    NOT NULL DEFAULT '',
    link        TEXT    NOT NULL,
    date        INTEGER NOT NULL DEFAULT 0,
    lastSeen    INTEGER NOT NULL DEFAULT 0,
    hash        BLOB,
    is_read     INTEGER NOT NULL DEFAULT 0,
    is_favorite INTEGER NOT NULL DEFAULT 0,
    id_feed     INTEGER NOT NULL REFERENCES feed (id) ON DELETE CASCADE,
    tags        TEXT    NOT NULL DEFAULT '',
    attributes  TEXT,
    UNIQUE (id_feed, guid)
);
CREATE INDEX entrytmp_date_index ON entrytmp (id_user, date, id);

CREATE TABLE tag (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    id_user    INTEGER NOT NULL REFERENCES user (id) ON DELETE CASCADE,
    name       TEXT    NOT NULL,
    attributes TEXT,
    UNIQUE (id_user, name)
);

CREATE TABLE entrytag (
    id_tag   INTEGER NOT NULL REFERENCES tag (id) ON DELETE CASCADE,
    id_entry INTEGER NOT NULL REFERENCES entry (id) ON DELETE CASCADE,
    PRIMARY KEY (id_tag, id_entry)
);
CREATE INDEX entrytag_id_entry_index ON entrytag (id_entry);

CREATE TABLE user_query (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    id_user    INTEGER NOT NULL REFERENCES user (id) ON DELETE CASCADE,
    name       TEXT    NOT NULL,
    search     TEXT    NOT NULL DEFAULT '',
    get_type   TEXT    NOT NULL DEFAULT 'a',
    get_id     INTEGER,
    state      INTEGER NOT NULL DEFAULT 3,
    sort_order TEXT    NOT NULL DEFAULT 'id:desc',
    position   INTEGER NOT NULL DEFAULT 0,
    attributes TEXT,
    UNIQUE (id_user, name)
);

-- Migration bookkeeping lives in `_sqlx_migrations`, which sqlx creates and owns; it is not here.
-- A database written by a build from before the driver was sqlx has no such table, looks untouched
-- to sqlx, and would fail on the `CREATE TABLE`s above. There is no bridge: recreate it.

INSERT INTO meta (key, value) VALUES ('text_encoding', 'utf8-plain');
