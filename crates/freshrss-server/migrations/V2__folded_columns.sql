-- Accent-folded copies of the columns the search grammar compares against.
--
-- SQLite's LIKE is case-insensitive for ASCII only and not accent-insensitive at all, and the
-- driver (sqlx) cannot register a user-defined function to do better. So the fold happens once, at
-- write time, into a parallel column, and a search compares folded to folded with instr(). The
-- in-memory matcher folds the same way, which is what keeps the two backends in agreement.
--
-- NOT NULL DEFAULT '' rather than nullable: a fold of anything is a string, and a NULL here would
-- make every instr() NULL and silently match nothing.
ALTER TABLE entry ADD COLUMN title_folded   TEXT NOT NULL DEFAULT '';
ALTER TABLE entry ADD COLUMN content_folded TEXT NOT NULL DEFAULT '';
ALTER TABLE entry ADD COLUMN author_folded  TEXT NOT NULL DEFAULT '';
ALTER TABLE entry ADD COLUMN link_folded    TEXT NOT NULL DEFAULT '';
ALTER TABLE entry ADD COLUMN tags_folded    TEXT NOT NULL DEFAULT '';

ALTER TABLE entrytmp ADD COLUMN title_folded   TEXT NOT NULL DEFAULT '';
ALTER TABLE entrytmp ADD COLUMN content_folded TEXT NOT NULL DEFAULT '';
ALTER TABLE entrytmp ADD COLUMN author_folded  TEXT NOT NULL DEFAULT '';
ALTER TABLE entrytmp ADD COLUMN link_folded    TEXT NOT NULL DEFAULT '';
ALTER TABLE entrytmp ADD COLUMN tags_folded    TEXT NOT NULL DEFAULT '';

ALTER TABLE tag ADD COLUMN name_folded TEXT NOT NULL DEFAULT '';

-- Rows written before this migration carry the default. The application backfills them at startup
-- and records which fold produced the stored values, so changing the fold re-runs the backfill.
INSERT INTO meta (key, value) VALUES ('fold_version', '0');
