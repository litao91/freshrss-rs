-- Feed icon cache.
--
-- The refresh pipeline records each feed's icon *address* in the feed's `feedIconUrl` attribute,
-- but the bytes themselves are fetched on their own schedule: the sidebar references the icon
-- endpoint on every render, far more often than a feed is refreshed, and re-downloading a favicon
-- per page load would be both slow and rude to the origin. Caching the body here, stamped with the
-- time it was fetched, lets the endpoint answer from storage and re-fetch only once the entry goes
-- stale.
--
-- Keyed by feed rather than by URL so `ON DELETE CASCADE` reclaims the blob when a subscription is
-- removed, and so a feed whose icon address changes simply overwrites its one row. The stored `url`
-- is what detects that change: a row whose address no longer matches the feed is treated as a miss
-- even inside the TTL window.

CREATE TABLE favicon (
    id_feed      INTEGER PRIMARY KEY REFERENCES feed (id) ON DELETE CASCADE,
    url          TEXT    NOT NULL,
    content_type TEXT    NOT NULL DEFAULT '',
    body         BLOB    NOT NULL,
    fetched_at   INTEGER NOT NULL DEFAULT 0
);
