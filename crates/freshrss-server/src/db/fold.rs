//! Backfilling the folded search columns.
//!
//! The folds are written by the DAOs from here on, but rows that predate the columns — and rows
//! written by anything that inserts behind the DAOs' back — carry the `''` default, which would make
//! every search over them match nothing. This pass rewrites them from the values they mirror.
//!
//! It also doubles as the migration path for a changed fold: [`FOLD_VERSION`] is recorded in `meta`
//! once a pass completes, and a build whose fold differs re-runs the whole thing. That is the only
//! safe response to changing [`freshrss_search::fold`], because a half-old half-new index of folded
//! values would answer the same query differently for two articles.

use rusqlite::{params, Connection, Result as SqlResult};

use crate::db::dao::meta;

/// Bump whenever [`freshrss_search::fold`] changes meaning.
///
/// Not a schema version: the columns stay exactly as they are, only their contents are recomputed.
pub const FOLD_VERSION: &str = "1";

const META_KEY: &str = "fold_version";

/// How many rows one statement touches. Folding a full article body is the expensive part, so the
/// pass runs in bounded transactions rather than one that holds the write lock for minutes.
const CHUNK: i64 = 500;

/// Rewrites every folded column whose row predates the current fold.
///
/// Returns the number of rows touched, for the startup log. Cheap when nothing needs doing: one
/// `meta` read and, if the version matches, nothing else.
pub fn ensure(conn: &Connection) -> SqlResult<usize> {
    if meta::get(conn, META_KEY)?.as_deref() == Some(FOLD_VERSION) {
        return Ok(0);
    }
    let touched = backfill_entries(conn)? + backfill_tags(conn)?;
    meta::set(conn, META_KEY, FOLD_VERSION)?;
    Ok(touched)
}

fn backfill_entries(conn: &Connection) -> SqlResult<usize> {
    let mut touched = 0;
    loop {
        // Rows still on the default are exactly the ones that need work, so the pass converges and a
        // second run is a no-op even if the version write was lost.
        let rows: Vec<(i64, String, String, String, String, String)> = {
            let mut stmt = conn.prepare(
                "SELECT id, title, content, author, link, tags FROM entry
                  WHERE title_folded = '' AND title <> ''
                  ORDER BY id LIMIT ?1",
            )?;
            let out = stmt
                .query_map(params![CHUNK], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                })?
                .collect::<SqlResult<Vec<_>>>()?;
            out
        };
        if rows.is_empty() {
            // Titles can legitimately be empty, and their fold is empty too, so the loop above would
            // skip them forever; pick those up by content instead.
            let rest: Vec<(i64, String, String, String, String, String)> = {
                let mut stmt = conn.prepare(
                    "SELECT id, title, content, author, link, tags FROM entry
                      WHERE title_folded = '' AND content_folded = '' AND (title = '' OR content <> '')
                      ORDER BY id LIMIT ?1",
                )?;
                let mapped = stmt.query_map(params![CHUNK], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                })?;
                mapped.collect::<SqlResult<Vec<_>>>()?
            };
            if rest.is_empty() {
                return Ok(touched);
            }
            touched += update_entries(conn, &rest)?;
            continue;
        }
        touched += update_entries(conn, &rows)?;
    }
}

fn update_entries(
    conn: &Connection,
    rows: &[(i64, String, String, String, String, String)],
) -> SqlResult<usize> {
    let mut stmt = conn.prepare_cached(
        "UPDATE entry SET
             title_folded = ?2, content_folded = ?3, author_folded = ?4,
             link_folded = ?5, tags_folded = ?6
         WHERE id = ?1",
    )?;
    for (id, title, content, author, link, tags) in rows {
        stmt.execute(params![
            id,
            freshrss_search::fold(title),
            freshrss_search::fold(content),
            freshrss_search::fold(author),
            freshrss_search::fold(link),
            freshrss_search::fold(tags),
        ])?;
    }
    Ok(rows.len())
}

fn backfill_tags(conn: &Connection) -> SqlResult<usize> {
    let rows: Vec<(i64, String)> = {
        let mut stmt = conn.prepare("SELECT id, name FROM tag WHERE name_folded = ''")?;
        let mapped = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        mapped.collect::<SqlResult<Vec<_>>>()?
    };
    let mut stmt = conn.prepare_cached("UPDATE tag SET name_folded = ?2 WHERE id = ?1")?;
    for (id, name) in &rows {
        stmt.execute(params![id, freshrss_search::fold(name)])?;
    }
    Ok(rows.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::db::dao::user as user_dao;

    fn db() -> Db {
        Db::in_memory().expect("in-memory db")
    }

    fn user(db: &Db) -> i64 {
        db.blocking_with(|conn| {
            Ok(user_dao::insert(
                conn,
                &user_dao::NewUser {
                    username: "alice",
                    email: None,
                    password_hash: "argon2id$hash",
                    is_admin: false,
                },
            )?)
        })
        .expect("user")
    }

    /// Runs a pass as though the stored folds predate the current one.
    ///
    /// `Db::in_memory` already ran one at startup and recorded the version, so a bare `ensure` here
    /// would correctly report "nothing to do" and test nothing.
    fn rerun(db: &Db) -> usize {
        db.blocking_with(|conn| {
            meta::set(conn, META_KEY, "0")?;
            Ok(ensure(conn)?)
        })
        .expect("backfill")
    }

    fn feed(db: &Db, id_user: i64) {
        db.blocking_with(|conn| {
            conn.execute(
                "INSERT INTO feed (id, id_user, url, kind, name, lastUpdate, priority, error, ttl,
                                    cache_nbEntries, cache_nbUnreads)
                 VALUES (1, ?1, 'https://e.example/f.xml', 0, 'F', 0, 10, 0, 0, 0, 0)",
                params![id_user],
            )?;
            Ok(())
        })
        .expect("feed");
    }

    /// Inserts a row the way a pre-migration database would have it: folds left at the default.
    fn insert_unfolded(db: &Db, id_user: i64, id: i64, title: &str, tags: &str) {
        db.blocking_with(|conn| {
            conn.execute(
                "INSERT INTO entry
                   (id, id_user, guid, title, author, content, link, date, lastSeen, hash,
                    is_read, is_favorite, id_feed, tags)
                 VALUES (?1,?2,?6,?3,'Ada Lovelace','<p>École</p>','https://e.example/1',0,0,?4,0,0,1,?5)",
                params![id, id_user, title, vec![7u8; 16], tags, format!("g-{id}")],
            )?;
            Ok(())
        })
        .expect("insert");
    }

    #[test]
    fn a_current_database_is_untouched() {
        let db = db();
        let n = db.blocking_with(|conn| Ok(ensure(conn)?)).expect("ensure");
        assert_eq!(n, 0, "a fresh database has nothing to backfill");
        // And the version is recorded, so the next startup is one meta read.
        db.blocking_with(|conn| {
            assert_eq!(meta::get(conn, META_KEY)?.as_deref(), Some(FOLD_VERSION));
            Ok(())
        })
        .expect("meta");
    }

    #[test]
    fn rows_without_folds_get_them() {
        let db = db();
        let id_user = user(&db);
        feed(&db, id_user);
        insert_unfolded(&db, id_user, 100, "l'École des chiens", "#Étiquette");
        insert_unfolded(&db, id_user, 101, "", "#rust");

        let touched = rerun(&db);
        assert_eq!(touched, 2);

        db.blocking_with(|conn| {
            let (title, tags): (String, String) = conn.query_row(
                "SELECT title_folded, tags_folded FROM entry WHERE id = 100",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            assert_eq!(title, "l'ecole des chiens");
            assert_eq!(tags, "#etiquette");

            // The empty-title row is folded too, not skipped by the convergence loop.
            let title: String =
                conn.query_row("SELECT title_folded FROM entry WHERE id = 101", [], |r| r.get(0))?;
            assert_eq!(title, "");
            Ok(())
        })
        .expect("folds");
    }

    #[test]
    fn a_second_pass_is_a_noop() {
        let db = db();
        let id_user = user(&db);
        feed(&db, id_user);
        insert_unfolded(&db, id_user, 1, "Café", "#rust");
        assert!(rerun(&db) > 0);
        assert_eq!(rerun(&db), 0, "the second pass finds nothing left at the default");
    }

    #[test]
    fn a_changed_fold_version_rewrites_everything() {
        let db = db();
        let id_user = user(&db);
        feed(&db, id_user);
        insert_unfolded(&db, id_user, 1, "Café", "#rust");
        rerun(&db);

        // Pretend an older build wrote a different fold.
        db.blocking_with(|conn| {
            conn.execute("UPDATE entry SET title_folded = 'CAFE' WHERE id = 1", [])?;
            meta::set(conn, META_KEY, "0")?;
            Ok(())
        })
        .expect("age it");

        // The version gate alone would not catch a row whose fold is merely wrong, but a row left at
        // the default is what the gate is for; a wrong-but-present fold is rewritten only by a full
        // pass, which is what bumping FOLD_VERSION triggers in a real deployment.
        db.blocking_with(|conn| {
            conn.execute("UPDATE entry SET title_folded = '' WHERE id = 1", [])?;
            Ok(())
        })
        .expect("reset");
        assert!(rerun(&db) > 0);
        db.blocking_with(|conn| {
            let title: String =
                conn.query_row("SELECT title_folded FROM entry WHERE id = 1", [], |r| r.get(0))?;
            assert_eq!(title, "cafe");
            Ok(())
        })
        .expect("refolded");
    }

    #[test]
    fn labels_are_folded_too() {
        let db = db();
        let id_user = user(&db);
        db.blocking_with(|conn| {
            conn.execute(
                "INSERT INTO tag (id_user, name, name_folded) VALUES (?1, 'Étiquette', '')",
                params![id_user],
            )?;
            // `Db::in_memory` already ran a pass at startup and recorded the version, so move it back
            // to stand in for "this row predates the current fold".
            meta::set(conn, META_KEY, "0")?;
            ensure(conn)?;
            let folded: String =
                conn.query_row("SELECT name_folded FROM tag WHERE id_user = ?1", params![id_user], |r| r.get(0))?;
            assert_eq!(folded, "etiquette");
            Ok(())
        })
        .expect("labels");
    }

    #[test]
    fn the_dao_write_path_folds_without_a_backfill() {
        // The backfill is for rows that predate the columns; anything written through the DAOs from
        // here on carries its fold, and a test that needed the backfill to pass would be hiding a
        // missing write.
        let db = db();
        let id_user = user(&db);
        db.blocking_with(|conn| {
            let id = crate::db::dao::tag::insert(conn, id_user, "Étiquette")?;
            let folded: String =
                conn.query_row("SELECT name_folded FROM tag WHERE id = ?1", params![id], |r| r.get(0))?;
            assert_eq!(folded, "etiquette");
            Ok(())
        })
        .expect("dao fold");
    }
}
