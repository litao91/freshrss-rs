-- Demo subscriptions and articles, for the browser check and for manual verification.
--
-- The refresh pipeline is what normally fills these tables, so until it exists this is how a running
-- server gets something to show. It assumes one registered user with `id = 1` — register through the
-- sign-in screen or `POST /api/v1/auth/register` first.
--
--     sqlite3 data/freshrss.sqlite < scripts/seed-demo.sql
--
-- Then **restart the server**. The accent-folded search columns are left empty on purpose: filling
-- them is `db::fold`'s backfill, which runs at startup, so seeding this way exercises that path
-- instead of quietly depending on it having already run.

PRAGMA foreign_keys = ON;

-- A second folder, so the nav tree has something to collapse.
INSERT INTO category (id, id_user, name, kind, lastUpdate, error, position)
VALUES (2, 1, 'Engineering', 0, 1791800000, 0, 1);

INSERT INTO feed (id, id_user, url, kind, category, name, website, description,
                  lastUpdate, priority, error, ttl)
VALUES
    -- The ordinary case.
    (1, 1, 'https://blog.example/feed.xml', 0, 1, 'Example Blog', 'https://blog.example/',
     'A demo feed with a long-ish description so the folder row has something to truncate.',
     1791800000, 10, 0, 0),
    -- Important, accented name, and a per-feed TTL.
    (2, 1, 'https://news.example/rss', 0, 2, 'Café News', 'https://news.example/',
     'Accents in the feed name and in its article titles.', 1791800000, 20, 0, 900),
    -- Muted: a negative TTL is the muting flag, and a muted feed is a problem-dialog entry.
    (3, 1, 'https://quiet.example/atom.xml', 0, 2, 'Quiet feed', 'https://quiet.example/',
     '', 1791800000, 10, 0, -3600),
    -- Failing: `error` holds the unix time of the last failed refresh.
    (4, 1, 'https://broken.example/feed', 0, 1, 'Broken feed', 'https://broken.example/',
     '', 1791700000, 10, 1791800000, 0);

-- 40 articles on the ordinary feed. The SPA asks for 40 at a time and the default view is unread
-- only, so the *unread* count has to clear one page for the infinite-scroll sentinel to have
-- anywhere to go: 40 here plus the other feeds leaves roughly 47 unread.
WITH RECURSIVE seq (n) AS (
    SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 40
)
INSERT INTO entry (id, id_user, guid, title, author, content, link, date, lastSeen,
                   is_read, is_favorite, id_feed, tags)
SELECT
    1791000000000000 + n * 1000000,
    1,
    'blog-' || n,
    -- Titles that sort into a visible order and that the search box can find.
    CASE n % 4
        WHEN 0 THEN 'Rust ' || printf('%02d', n) || ': a release note'
        WHEN 1 THEN 'Rust ' || printf('%02d', n) || ': what changed'
        WHEN 2 THEN 'Notes ' || printf('%02d', n) || ' on databases'
        ELSE 'Post ' || printf('%02d', n)
    END,
    CASE n % 3 WHEN 0 THEN 'Alice Example' WHEN 1 THEN 'Bob Sample' ELSE '' END,
    '<p>Body of article ' || n || '.</p>'
        || '<p>It has more than one paragraph, a <a href="https://blog.example/' || n
        || '">link</a>, and some <code>inline code</code>.</p>'
        || CASE WHEN n % 6 = 0 THEN
               '<blockquote><p>A pull quote, to check that prose styling reaches block elements.</p></blockquote>'
               || '<pre><code>fn main() { println!("hello"); }</code></pre>'
               || '<img src="https://blog.example/img/' || n || '.png" alt="A figure">'
           ELSE '' END,
    'https://blog.example/' || n,
    1791800000 - n * 3600,
    1791800000 - n * 3600,
    -- Every third article already read, so the Unread/All segment switch has both to show.
    CASE WHEN n % 3 = 0 THEN 1 ELSE 0 END,
    CASE WHEN n % 11 = 0 THEN 1 ELSE 0 END,
    1,
    CASE n % 5
        WHEN 0 THEN '#rust #release'
        WHEN 1 THEN '#rust'
        WHEN 2 THEN '#database'
        ELSE ''
    END
FROM seq;

-- 15 on the accented feed: `intitle:ecole` has to match "École" through the folded columns, which
-- is the whole reason the fold exists.
WITH RECURSIVE seq (n) AS (
    SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 15
)
INSERT INTO entry (id, id_user, guid, title, author, content, link, date, lastSeen,
                   is_read, is_favorite, id_feed, tags)
SELECT
    1791100000000000 + n * 1000000,
    1,
    'news-' || n,
    CASE n % 3
        WHEN 0 THEN 'École ' || printf('%02d', n) || ' — ünïcödé title'
        WHEN 1 THEN 'Café ' || printf('%02d', n) || ' and a naïve résumé'
        ELSE 'Plain ' || printf('%02d', n)
    END,
    'Zoë Reporter',
    '<p>Article ' || n || ' from the accented feed, with a café and a résumé in the body too.</p>',
    'https://news.example/' || n,
    1791790000 - n * 3600,
    1791790000 - n * 3600,
    0,
    CASE WHEN n = 1 THEN 1 ELSE 0 END,
    2,
    CASE WHEN n % 4 = 0 THEN '#café' ELSE '' END
FROM seq;

-- 6 on the muted feed and 6 on the failing one.
WITH RECURSIVE seq (n) AS (
    SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 6
)
INSERT INTO entry (id, id_user, guid, title, author, content, link, date, lastSeen,
                   is_read, is_favorite, id_feed, tags)
SELECT 1791200000000000 + n * 1000000, 1, 'quiet-' || n,
       'Quiet ' || printf('%02d', n), '', '<p>From the muted feed.</p>',
       'https://quiet.example/' || n, 1791780000 - n * 3600, 1791780000 - n * 3600,
       0, 0, 3, ''
FROM seq;

WITH RECURSIVE seq (n) AS (
    SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 6
)
INSERT INTO entry (id, id_user, guid, title, author, content, link, date, lastSeen,
                   is_read, is_favorite, id_feed, tags)
SELECT 1791300000000000 + n * 1000000, 1, 'broken-' || n,
       'Last good ' || printf('%02d', n), '', '<p>From the feed that now fails.</p>',
       'https://broken.example/' || n, 1791770000 - n * 3600, 1791770000 - n * 3600,
       1, 0, 4, ''
FROM seq;

-- Labels, which live in their own tables rather than in the `tags` column: a feed keyword and a
-- user label are different things that happen to look alike in the sidebar.
INSERT INTO tag (id, id_user, name) VALUES (1, 1, 'read-later'), (2, 1, 'important');
INSERT INTO entrytag (id_tag, id_entry)
VALUES (1, 1791000001000000), (1, 1791000002000000), (2, 1791100001000000);

-- One saved search, so the queries endpoint and the sidebar have a row.
INSERT INTO user_query (id, id_user, name, search, get_type, get_id, state, sort_order, position)
VALUES (1, 1, 'Rust releases', 'intitle:rust #release', '', 0, 0, '', 0);

-- The counters the nav tree reads instead of recounting on every request.
UPDATE feed SET cache_nbEntries = (SELECT COUNT(*) FROM entry WHERE entry.id_feed = feed.id),
            cache_nbUnreads = (SELECT COUNT(*) FROM entry
                               WHERE entry.id_feed = feed.id AND entry.is_read = 0);

-- The folded search columns are filled by a backfill that runs at startup, and that backfill is
-- gated on a version key so it does not rescan the whole table on every boot. Rows written behind
-- the server's back are therefore never folded, and a search would silently find nothing in them.
-- Dropping the gate makes the next start fold them.
UPDATE meta SET value = '0' WHERE key = 'fold_version';
