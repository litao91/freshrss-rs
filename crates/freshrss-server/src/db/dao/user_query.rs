//! `user_query` access — the reader's saved searches.
//!
//! Every lookup is scoped by `id_user`. That is not defensive redundancy here: `S:` and `search:`
//! resolve against these rows, so an unscoped lookup would let one reader's query pull in another's
//! saved search by guessing its id.

use std::collections::HashSet;

use rusqlite::{params, Connection, OptionalExtension, Result as SqlResult};

use crate::db::dao::row_attributes;
use crate::db::sql::{chunks, in_clause};
use crate::domain::list_type::ListType;
use crate::domain::state::EntryState;
use crate::domain::user_query::SavedQuery;

pub const COLUMNS: &str =
    "id, id_user, name, search, get_type, get_id, state, sort_order, position, attributes";

fn from_row(row: &rusqlite::Row<'_>) -> SqlResult<SavedQuery> {
    let get_type: String = row.get(4)?;
    let state_bits: i64 = row.get(6)?;
    let sort_order: String = row.get(7)?;
    let (sort, order) = SavedQuery::sort_order_from_db(&sort_order);
    Ok(SavedQuery {
        id: row.get(0)?,
        id_user: row.get(1)?,
        name: row.get(2)?,
        search: row.get(3)?,
        // An unrecognised scope degrades to the main stream rather than failing the sidebar.
        scope: ListType::parse(&get_type).unwrap_or_default(),
        scope_id: row.get(5)?,
        state: EntryState::from_bits(state_bits as i32),
        sort,
        order,
        secondary_sort: Default::default(),
        secondary_order: Default::default(),
        position: row.get(8)?,
        attributes: row_attributes(row, 9)?,
    })
}

pub fn list(conn: &Connection, id_user: i64) -> SqlResult<Vec<SavedQuery>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM user_query WHERE id_user = ?1 ORDER BY position, name COLLATE NOCASE"
    ))?;
    let rows = stmt
        .query_map(params![id_user], from_row)?
        .collect::<SqlResult<Vec<_>>>()?;
    Ok(rows)
}

pub fn find_by_id(conn: &Connection, id: i64, id_user: i64) -> SqlResult<Option<SavedQuery>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM user_query WHERE id = ?1 AND id_user = ?2"),
        params![id, id_user],
        from_row,
    )
    .optional()
}

pub fn find_by_name(
    conn: &Connection,
    id_user: i64,
    name: &str,
) -> SqlResult<Option<SavedQuery>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM user_query WHERE id_user = ?1 AND name = ?2"),
        params![id_user, name],
        from_row,
    )
    .optional()
}

pub fn insert(conn: &Connection, query: &SavedQuery) -> SqlResult<i64> {
    conn.execute(
        "INSERT INTO user_query
           (id_user, name, search, get_type, get_id, state, sort_order, position, attributes)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![
            query.id_user,
            query.name,
            query.search,
            query.scope.as_char(),
            query.scope_id,
            query.state.to_bits(),
            query.sort_order_to_db(),
            query.position,
            query.attributes.to_db(),
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Updates everything a reader can edit. `position` is set by [`set_positions`], which has to move
/// several rows at once to avoid colliding with the ones it has not reached yet.
pub fn update(conn: &Connection, query: &SavedQuery) -> SqlResult<bool> {
    Ok(conn.execute(
        "UPDATE user_query
            SET name = ?3, search = ?4, get_type = ?5, get_id = ?6, state = ?7,
                sort_order = ?8, attributes = ?9
          WHERE id = ?1 AND id_user = ?2",
        params![
            query.id,
            query.id_user,
            query.name,
            query.search,
            query.scope.as_char(),
            query.scope_id,
            query.state.to_bits(),
            query.sort_order_to_db(),
            query.attributes.to_db(),
        ],
    )? > 0)
}

pub fn delete(conn: &Connection, id: i64, id_user: i64) -> SqlResult<bool> {
    Ok(conn.execute(
        "DELETE FROM user_query WHERE id = ?1 AND id_user = ?2",
        params![id, id_user],
    )? > 0)
}

/// Assigns positions from the slice index, so the caller sends the complete desired order.
///
/// A partial list would leave the omitted rows wherever they were, and two rows could end up equal —
/// which makes the sidebar order depend on the tie-break rather than on what the reader arranged.
///
/// Ids that do not belong to this user are skipped rather than rejected: a stale client holding a row
/// that has since been deleted is normal, and an error would tell the caller nothing they could act on.
pub fn set_positions(conn: &Connection, id_user: i64, ids: &[i64]) -> SqlResult<usize> {
    // One lookup of what this user actually owns, so the updates below cannot touch another user's row
    // however the id list was assembled.
    let mut owned = HashSet::new();
    for chunk in chunks(ids) {
        let placeholders = in_clause(chunk.len());
        let sql = format!("SELECT id FROM user_query WHERE id_user = ?1 AND id IN {placeholders}");
        let mut stmt = conn.prepare(&sql)?;
        let mut values: Vec<&dyn rusqlite::ToSql> = vec![&id_user];
        values.extend(chunk.iter().map(|v| v as &dyn rusqlite::ToSql));
        let mut rows = stmt.query(values.as_slice())?;
        while let Some(row) = rows.next()? {
            owned.insert(row.get::<_, i64>(0)?);
        }
    }

    let autocommit = conn.is_autocommit();
    if autocommit {
        // A half-applied reorder is worse than either outcome, so the moves are one transaction.
        conn.execute_batch("BEGIN")?;
    }
    let result = (|| -> SqlResult<usize> {
        let mut moved = 0;
        for (position, id) in ids.iter().enumerate() {
            if !owned.contains(id) {
                continue;
            }
            conn.execute(
                "UPDATE user_query SET position = ?3 WHERE id = ?1 AND id_user = ?2",
                params![id, id_user, position as i64],
            )?;
            moved += 1;
        }
        Ok(moved)
    })();
    if autocommit {
        conn.execute_batch(if result.is_ok() { "COMMIT" } else { "ROLLBACK" })?;
    }
    result
}

pub fn delete_all_for_user(conn: &Connection, id_user: i64) -> SqlResult<usize> {
    conn.execute(
        "DELETE FROM user_query WHERE id_user = ?1",
        params![id_user],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::db::dao::user as user_dao;
    use crate::domain::sort::{Order, Sort};

    fn setup() -> (Db, i64) {
        let db = Db::in_memory().expect("in-memory db");
        let id_user = db
            .blocking_with(|conn| {
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
            .expect("create user");
        (db, id_user)
    }

    fn saved(id_user: i64, name: &str, search: &str) -> SavedQuery {
        SavedQuery::new(id_user, name, search)
    }

    #[test]
    fn a_saved_query_round_trips_every_editable_field() {
        let (db, id_user) = setup();
        db.blocking_with(|conn| {
            let mut q = saved(id_user, "unread rust", "intitle:rust -author:bot");
            q.scope = ListType::Category;
            q.scope_id = Some(4);
            q.state = EntryState::unread();
            q.sort = Sort::Date;
            q.order = Order::Asc;
            q.id = insert(conn, &q)?;

            let back = find_by_id(conn, q.id, id_user)?.expect("exists");
            assert_eq!(back.name, q.name);
            assert_eq!(back.search, q.search);
            assert_eq!(back.scope, ListType::Category);
            assert_eq!(back.scope_id, Some(4));
            assert_eq!(back.state, EntryState::unread());
            assert_eq!(back.sort, Sort::Date);
            assert_eq!(back.order, Order::Asc);
            Ok(())
        })
        .expect("round trip");
    }

    #[test]
    fn the_state_survives_as_a_bitmask() {
        // The column is an integer, so a state that only the structural form can express would be lost.
        let (db, id_user) = setup();
        db.blocking_with(|conn| {
            let mut q = saved(id_user, "s", "x");
            q.state = EntryState::unread_or_starred();
            q.id = insert(conn, &q)?;
            assert_eq!(
                find_by_id(conn, q.id, id_user)?.expect("exists").state,
                EntryState::unread_or_starred()
            );
            Ok(())
        })
        .expect("round trip");
    }

    #[test]
    fn names_are_unique_per_user_but_not_across_users() {
        let (db, id_user) = setup();
        let other = db
            .blocking_with(|conn| {
                Ok(user_dao::insert(
                    conn,
                    &user_dao::NewUser {
                        username: "bob",
                        email: None,
                        password_hash: "argon2id$hash",
                        is_admin: false,
                    },
                )?)
            })
            .expect("create user");

        db.blocking_with(|conn| {
            let q = saved(id_user, "todo", "labels:todo");
            insert(conn, &q)?;
            // The same name for another user is a different saved search.
            insert(conn, &saved(other, "todo", "labels:other"))?;

            assert_eq!(
                find_by_name(conn, id_user, "todo")?.expect("exists").search,
                "labels:todo"
            );
            assert_eq!(
                find_by_name(conn, other, "todo")?.expect("exists").search,
                "labels:other"
            );
            assert!(find_by_name(conn, id_user, "nope")?.is_none());

            // A second `todo` for the same user is a constraint violation, not a silent overwrite.
            assert!(insert(conn, &saved(id_user, "todo", "x")).is_err());
            Ok(())
        })
        .expect("lookups");
    }

    #[test]
    fn no_lookup_reaches_another_users_saved_search() {
        // `S:` resolves by id, so an unscoped lookup would be a read primitive over other readers'
        // queries.
        let (db, id_user) = setup();
        let other = db
            .blocking_with(|conn| {
                Ok(user_dao::insert(
                    conn,
                    &user_dao::NewUser {
                        username: "bob",
                        email: None,
                        password_hash: "argon2id$hash",
                        is_admin: false,
                    },
                )?)
            })
            .expect("create user");

        db.blocking_with(|conn| {
            let mut q = saved(other, "private", "intitle:secret");
            q.id = insert(conn, &q)?;

            assert!(find_by_id(conn, q.id, id_user)?.is_none());
            assert!(find_by_name(conn, id_user, "private")?.is_none());
            assert!(list(conn, id_user)?.is_empty());
            assert_eq!(list(conn, other)?.len(), 1);
            assert!(!delete(conn, q.id, id_user)?, "deleting someone else's row does nothing");
            assert!(!update(conn, &SavedQuery { id_user, ..q.clone() })?);
            assert_eq!(
                find_by_id(conn, q.id, other)?.expect("still there").search,
                "intitle:secret"
            );
            Ok(())
        })
        .expect("isolation");
    }

    #[test]
    fn listing_is_ordered_by_position_then_name() {
        let (db, id_user) = setup();
        db.blocking_with(|conn| {
            for (name, position) in [("zulu", 1), ("alpha", 1), ("first", 0)] {
                let mut q = saved(id_user, name, "x");
                q.position = position;
                insert(conn, &q)?;
            }
            let names: Vec<String> = list(conn, id_user)?.into_iter().map(|q| q.name).collect();
            assert_eq!(names, vec!["first", "alpha", "zulu"]);
            Ok(())
        })
        .expect("listing");
    }

    #[test]
    fn set_positions_assigns_from_the_slice_index() {
        let (db, id_user) = setup();
        db.blocking_with(|conn| {
            let mut ids = Vec::new();
            for name in ["a", "b", "c"] {
                let q = saved(id_user, name, "x");
                ids.push(insert(conn, &q)?);
            }
            ids.reverse();
            assert_eq!(set_positions(conn, id_user, &ids)?, 3);
            let names: Vec<String> = list(conn, id_user)?.into_iter().map(|q| q.name).collect();
            assert_eq!(names, vec!["c", "b", "a"]);

            // An id belonging to someone else is skipped rather than moved.
            let other = user_dao::insert(
                conn,
                &user_dao::NewUser {
                    username: "bob",
                    email: None,
                    password_hash: "argon2id$hash",
                    is_admin: false,
                },
            )?;
            let foreign = insert(conn, &saved(other, "intruder", "x"))?;
            let moved = set_positions(conn, id_user, &[foreign])?;
            assert_eq!(moved, 0, "another user's row is not this user's to reorder");
            // And it kept its own position.
            assert_eq!(find_by_id(conn, foreign, other)?.expect("exists").position, 0);
            Ok(())
        })
        .expect("reorder");
    }

    #[test]
    fn deleting_a_user_removes_their_saved_searches() {
        let (db, id_user) = setup();
        db.blocking_with(|conn| {
            insert(conn, &saved(id_user, "a", "x"))?;
            assert_eq!(delete_all_for_user(conn, id_user)?, 1);
            assert!(list(conn, id_user)?.is_empty());
            Ok(())
        })
        .expect("cascade");
    }

    #[test]
    fn a_corrupt_scope_degrades_rather_than_failing() {
        // The sidebar has to load even if a row was written by a build with a different scope set.
        let (db, id_user) = setup();
        db.blocking_with(|conn| {
            let q = saved(id_user, "a", "x");
            let id = insert(conn, &q)?;
            conn.execute(
                "UPDATE user_query SET get_type = 'QQ' WHERE id = ?1",
                params![id],
            )?;
            assert_eq!(
                find_by_id(conn, id, id_user)?.expect("exists").scope,
                ListType::MainStream
            );
            Ok(())
        })
        .expect("degrades");
    }
}
