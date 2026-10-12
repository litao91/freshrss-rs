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

use sqlx::SqliteConnection;

use crate::db::dao::meta;
use crate::error::Result;

/// Bump whenever [`freshrss_search::fold`] changes meaning.
///
/// Not a schema version: the columns stay exactly as they are, only their contents are recomputed.
pub const FOLD_VERSION: &str = "1";

const META_KEY: &str = "fold_version";

/// How many rows one statement touches. Folding a full article body is the expensive part, so the
/// pass runs in bounded chunks rather than one statement that holds the write lock for minutes.
///
/// No transaction wraps the whole pass, deliberately: `fold_version` is only advanced at the end, so
/// a crash part-way leaves some rows folded and the gate closed, and the next start simply folds the
/// rest. Wrapping it would trade that for a rollback of work already done.
const CHUNK: i64 = 500;

/// One row whose folds are still at the default, read back with the values they mirror.
#[derive(Debug, sqlx::FromRow)]
struct Unfolded {
    id: i64,
    title: String,
    content: String,
    author: String,
    link: String,
    tags: String,
}

/// Rewrites every folded column whose row predates the current fold.
///
/// Returns the number of rows touched, for the startup log. Cheap when nothing needs doing: one
/// `meta` read and, if the version matches, nothing else.
pub async fn ensure(conn: &mut SqliteConnection) -> Result<usize> {
    if meta::get(conn, META_KEY).await?.as_deref() == Some(FOLD_VERSION) {
        return Ok(0);
    }
    let touched = backfill_entries(conn).await? + backfill_tags(conn).await?;
    meta::set(conn, META_KEY, FOLD_VERSION).await?;
    Ok(touched)
}

async fn backfill_entries(conn: &mut SqliteConnection) -> Result<usize> {
    let mut touched = 0;
    loop {
        // Rows still on the default are exactly the ones that need work, so the pass converges and a
        // second run is a no-op even if the version write was lost.
        let rows: Vec<Unfolded> = sqlx::query_as(
            "SELECT id, title, content, author, link, tags FROM entry
              WHERE title_folded = '' AND title <> ''
              ORDER BY id LIMIT ?",
        )
        .bind(CHUNK)
        .fetch_all(&mut *conn)
        .await?;
        if rows.is_empty() {
            // Titles can legitimately be empty, and their fold is empty too, so the loop above would
            // skip them forever; pick those up by content instead.
            let rest: Vec<Unfolded> = sqlx::query_as(
                "SELECT id, title, content, author, link, tags FROM entry
                  WHERE title_folded = '' AND content_folded = '' AND (title = '' OR content <> '')
                  ORDER BY id LIMIT ?",
            )
            .bind(CHUNK)
            .fetch_all(&mut *conn)
            .await?;
            if rest.is_empty() {
                return Ok(touched);
            }
            touched += update_entries(conn, &rest).await?;
            continue;
        }
        touched += update_entries(conn, &rows).await?;
    }
}

async fn update_entries(conn: &mut SqliteConnection, rows: &[Unfolded]) -> Result<usize> {
    for row in rows {
        // sqlx caches prepared statements per connection, so this is one `sqlite3_prepare` for the
        // whole chunk rather than one per row.
        sqlx::query(
            "UPDATE entry SET
                 title_folded = ?2, content_folded = ?3, author_folded = ?4,
                 link_folded = ?5, tags_folded = ?6
             WHERE id = ?1",
        )
        .bind(row.id)
        .bind(freshrss_search::fold(&row.title))
        .bind(freshrss_search::fold(&row.content))
        .bind(freshrss_search::fold(&row.author))
        .bind(freshrss_search::fold(&row.link))
        .bind(freshrss_search::fold(&row.tags))
        .execute(&mut *conn)
        .await?;
    }
    Ok(rows.len())
}

async fn backfill_tags(conn: &mut SqliteConnection) -> Result<usize> {
    let rows: Vec<(i64, String)> = sqlx::query("SELECT id, name FROM tag WHERE name_folded = ''")
        .map(|row: sqlx::sqlite::SqliteRow| {
            use sqlx::Row as _;
            (row.get::<i64, _>("id"), row.get::<String, _>("name"))
        })
        .fetch_all(&mut *conn)
        .await?;
    for (id, name) in &rows {
        sqlx::query("UPDATE tag SET name_folded = ?2 WHERE id = ?1")
            .bind(*id)
            .bind(freshrss_search::fold(name))
            .execute(&mut *conn)
            .await?;
    }
    Ok(rows.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::dao::user as user_dao;
    use crate::db::support::{db, scalar};
    use crate::db::Db;

    async fn user(db: &Db) -> i64 {
        db.with(|mut conn| async move {
            user_dao::insert(
                &mut conn,
                &user_dao::NewUser {
                    username: "alice",
                    email: None,
                    password_hash: "argon2id$hash",
                    is_admin: false,
                },
            )
            .await
        })
        .await
        .expect("user")
    }

    /// Runs a pass as though the stored folds predate the current one.
    ///
    /// `Db::in_memory` already ran one at startup and recorded the version, so a bare `ensure` here
    /// would correctly report "nothing to do" and test nothing.
    async fn rerun(db: &Db) -> usize {
        db.with(|mut conn| async move {
            meta::set(&mut conn, META_KEY, "0").await?;
            ensure(&mut conn).await
        })
        .await
        .expect("backfill")
    }

    async fn feed(db: &Db, id_user: i64) {
        db.with(|mut conn| async move {
            Ok(sqlx::query(
                "INSERT INTO feed (id, id_user, url, kind, name, lastUpdate, priority, error, ttl,
                                    cache_nbEntries, cache_nbUnreads)
                 VALUES (1, ?1, 'https://e.example/f.xml', 0, 'F', 0, 10, 0, 0, 0, 0)",
            )
            .bind(id_user)
            .execute(&mut *conn)
            .await?)
        })
        .await
        .expect("feed");
    }

    /// Inserts a row the way a pre-migration database would have it: folds left at the default.
    async fn insert_unfolded(db: &Db, id_user: i64, id: i64, title: &str, tags: &str) {
        let title = title.to_owned();
        let tags = tags.to_owned();
        db.with(|mut conn| async move {
            Ok(sqlx::query(
                "INSERT INTO entry
                   (id, id_user, guid, title, author, content, link, date, lastSeen, hash,
                    is_read, is_favorite, id_feed, tags)
                 VALUES (?1,?2,?6,?3,'Ada Lovelace','<p>École</p>','https://e.example/1',0,0,?4,0,0,1,?5)",
            )
            .bind(id)
            .bind(id_user)
            .bind(title)
            .bind(vec![7u8; 16])
            .bind(tags)
            .bind(format!("g-{id}"))
            .execute(&mut *conn)
            .await?)
        })
        .await
        .expect("insert");
    }

    #[tokio::test]
    async fn a_current_database_is_untouched() {
        let store = db().await;
        let n = store
            .with(|mut conn| async move { ensure(&mut conn).await })
            .await
            .expect("ensure");
        assert_eq!(n, 0, "a fresh database has nothing to backfill");
        // And the version is recorded, so the next startup is one meta read.
        let recorded: Option<String> =
            scalar(&store, "SELECT value FROM meta WHERE key = 'fold_version'").await;
        assert_eq!(recorded.as_deref(), Some(FOLD_VERSION));
    }

    #[tokio::test]
    async fn rows_without_folds_get_them() {
        let store = db().await;
        let id_user = user(&store).await;
        feed(&store, id_user).await;
        insert_unfolded(&store, id_user, 100, "l'École des chiens", "#Étiquette").await;
        insert_unfolded(&store, id_user, 101, "", "#rust").await;

        let touched = rerun(&store).await;
        assert_eq!(touched, 2);

        let (title, tags): (String, String) = store
            .with(|mut conn| async move {
                let row = sqlx::query("SELECT title_folded, tags_folded FROM entry WHERE id = 100")
                    .fetch_one(&mut *conn)
                    .await?;
                use sqlx::Row as _;
                Ok((
                    row.get::<String, _>("title_folded"),
                    row.get::<String, _>("tags_folded"),
                ))
            })
            .await
            .expect("folds");
        assert_eq!(title, "l'ecole des chiens");
        assert_eq!(tags, "#etiquette");

        // The empty-title row is folded too, not skipped by the convergence loop.
        let empty: String = scalar(&store, "SELECT title_folded FROM entry WHERE id = 101").await;
        assert_eq!(empty, "");
    }

    #[tokio::test]
    async fn a_second_pass_is_a_noop() {
        let store = db().await;
        let id_user = user(&store).await;
        feed(&store, id_user).await;
        insert_unfolded(&store, id_user, 1, "Café", "#rust").await;
        assert!(rerun(&store).await > 0);
        assert_eq!(
            rerun(&store).await,
            0,
            "the second pass finds nothing left at the default"
        );
    }

    #[tokio::test]
    async fn a_changed_fold_version_rewrites_everything() {
        let store = db().await;
        let id_user = user(&store).await;
        feed(&store, id_user).await;
        insert_unfolded(&store, id_user, 1, "Café", "#rust").await;
        rerun(&store).await;

        // Pretend an older build wrote a different fold. The version gate alone would not catch a
        // row whose fold is merely wrong, but a row left at the default is what the gate is for; a
        // wrong-but-present fold is rewritten only by a full pass, which is what bumping
        // FOLD_VERSION triggers in a real deployment.
        store
            .with(|mut conn| async move {
                meta::set(&mut conn, META_KEY, "0").await?;
                Ok(
                    sqlx::query("UPDATE entry SET title_folded = '' WHERE id = 1")
                        .execute(&mut *conn)
                        .await?,
                )
            })
            .await
            .expect("age it");
        assert!(rerun(&store).await > 0);

        let title: String = scalar(&store, "SELECT title_folded FROM entry WHERE id = 1").await;
        assert_eq!(title, "cafe");
    }

    #[tokio::test]
    async fn labels_are_folded_too() {
        let store = db().await;
        let id_user = user(&store).await;
        store
            .with(|mut conn| async move {
                sqlx::query(
                    "INSERT INTO tag (id_user, name, name_folded) VALUES (?1, 'Étiquette', '')",
                )
                .bind(id_user)
                .execute(&mut *conn)
                .await?;
                // `Db::in_memory` already ran a pass at startup and recorded the version, so move
                // it back to stand in for "this row predates the current fold".
                meta::set(&mut conn, META_KEY, "0").await?;
                ensure(&mut conn).await
            })
            .await
            .expect("labels");

        let folded: String = scalar(&store, "SELECT name_folded FROM tag").await;
        assert_eq!(folded, "etiquette");
    }

    #[tokio::test]
    async fn the_dao_write_path_folds_without_a_backfill() {
        // The backfill is for rows that predate the columns; anything written through the DAOs from
        // here on carries its fold, and a test that needed the backfill to pass would be hiding a
        // missing write.
        let store = db().await;
        let id_user = user(&store).await;
        store
            .with(|mut conn| async move {
                crate::db::dao::tag::insert(&mut conn, id_user, "Étiquette").await
            })
            .await
            .expect("dao fold");

        let folded: String = scalar(&store, "SELECT name_folded FROM tag").await;
        assert_eq!(folded, "etiquette");
    }
}
