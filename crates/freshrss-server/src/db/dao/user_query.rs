//! `user_query` access — the reader's saved searches.
//!
//! Every lookup is scoped by `id_user`. That is not defensive redundancy here: `S:` and `search:`
//! resolve against these rows, so an unscoped lookup would let one reader's query pull in another's
//! saved search by guessing its id.

use std::collections::HashSet;

use sqlx::sqlite::SqliteRow;
use sqlx::{Connection as _, QueryBuilder, Row as _, Sqlite, SqliteConnection};

use crate::db::dao::row_attributes;
use crate::db::dao::{chunks, push_in_list};
use crate::domain::list_type::ListType;
use crate::domain::state::EntryState;
use crate::domain::user_query::SavedQuery;
use crate::error::Result;

pub const COLUMNS: &str =
    "id, id_user, name, search, get_type, get_id, state, sort_order, position, attributes";

fn from_row(row: SqliteRow) -> std::result::Result<SavedQuery, sqlx::Error> {
    let get_type: String = row.try_get("get_type")?;
    let state_bits: i64 = row.try_get("state")?;
    let sort_order: String = row.try_get("sort_order")?;
    let (sort, order) = SavedQuery::sort_order_from_db(&sort_order);
    Ok(SavedQuery {
        id: row.try_get("id")?,
        id_user: row.try_get("id_user")?,
        name: row.try_get("name")?,
        search: row.try_get("search")?,
        // An unrecognised scope degrades to the main stream rather than failing the sidebar.
        scope: ListType::parse(&get_type).unwrap_or_default(),
        scope_id: row.try_get("get_id")?,
        state: EntryState::from_bits(state_bits as i32),
        sort,
        order,
        secondary_sort: Default::default(),
        secondary_order: Default::default(),
        position: row.try_get("position")?,
        attributes: row_attributes(&row, "attributes")?,
    })
}

pub async fn list(conn: &mut SqliteConnection, id_user: i64) -> Result<Vec<SavedQuery>> {
    let mut builder = QueryBuilder::<Sqlite>::new("SELECT ");
    builder
        .push(COLUMNS)
        .push(" FROM user_query WHERE id_user = ")
        .push_bind(id_user)
        .push(" ORDER BY position, name COLLATE NOCASE");
    Ok(builder
        .build()
        .try_map(from_row)
        .fetch_all(&mut *conn)
        .await?)
}

pub async fn find_by_id(
    conn: &mut SqliteConnection,
    id: i64,
    id_user: i64,
) -> Result<Option<SavedQuery>> {
    let mut builder = QueryBuilder::<Sqlite>::new("SELECT ");
    builder
        .push(COLUMNS)
        .push(" FROM user_query WHERE id = ")
        .push_bind(id)
        .push(" AND id_user = ")
        .push_bind(id_user);
    Ok(builder
        .build()
        .try_map(from_row)
        .fetch_optional(&mut *conn)
        .await?)
}

pub async fn find_by_name(
    conn: &mut SqliteConnection,
    id_user: i64,
    name: &str,
) -> Result<Option<SavedQuery>> {
    let mut builder = QueryBuilder::<Sqlite>::new("SELECT ");
    builder
        .push(COLUMNS)
        .push(" FROM user_query WHERE id_user = ")
        .push_bind(id_user)
        .push(" AND name = ")
        .push_bind(name);
    Ok(builder
        .build()
        .try_map(from_row)
        .fetch_optional(&mut *conn)
        .await?)
}

pub async fn insert(conn: &mut SqliteConnection, query: &SavedQuery) -> Result<i64> {
    let id = sqlx::query(
        "INSERT INTO user_query
           (id_user, name, search, get_type, get_id, state, sort_order, position, attributes)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
    )
    .bind(query.id_user)
    .bind(&query.name)
    .bind(&query.search)
    .bind(query.scope.as_char())
    .bind(query.scope_id)
    .bind(query.state.to_bits())
    .bind(query.sort_order_to_db())
    .bind(query.position)
    .bind(query.attributes.to_db())
    .execute(&mut *conn)
    .await?
    .last_insert_rowid();
    Ok(id)
}

/// Updates everything a reader can edit. `position` is set by [`set_positions`], which has to move
/// several rows at once to avoid colliding with the ones it has not reached yet.
pub async fn update(conn: &mut SqliteConnection, query: &SavedQuery) -> Result<bool> {
    Ok(sqlx::query(
        "UPDATE user_query
            SET name = ?3, search = ?4, get_type = ?5, get_id = ?6, state = ?7,
                sort_order = ?8, attributes = ?9
          WHERE id = ?1 AND id_user = ?2",
    )
    .bind(query.id)
    .bind(query.id_user)
    .bind(&query.name)
    .bind(&query.search)
    .bind(query.scope.as_char())
    .bind(query.scope_id)
    .bind(query.state.to_bits())
    .bind(query.sort_order_to_db())
    .bind(query.attributes.to_db())
    .execute(&mut *conn)
    .await?
    .rows_affected()
        > 0)
}

pub async fn delete(conn: &mut SqliteConnection, id: i64, id_user: i64) -> Result<bool> {
    Ok(
        sqlx::query("DELETE FROM user_query WHERE id = ?1 AND id_user = ?2")
            .bind(id)
            .bind(id_user)
            .execute(&mut *conn)
            .await?
            .rows_affected()
            > 0,
    )
}

/// Assigns positions from the slice index, so the caller sends the complete desired order.
///
/// A partial list would leave the omitted rows wherever they were, and two rows could end up equal —
/// which makes the sidebar order depend on the tie-break rather than on what the reader arranged.
///
/// Ids that do not belong to this user are skipped rather than rejected: a stale client holding a row
/// that has since been deleted is normal, and an error would tell the caller nothing they could act on.
pub async fn set_positions(
    conn: &mut SqliteConnection,
    id_user: i64,
    ids: &[i64],
) -> Result<usize> {
    // One lookup of what this user actually owns, so the updates below cannot touch another user's row
    // however the id list was assembled.
    let mut owned = HashSet::new();
    for chunk in chunks(ids, 1) {
        let mut builder = QueryBuilder::<Sqlite>::new("SELECT id FROM user_query WHERE id_user = ");
        builder.push_bind(id_user).push(" AND id ");
        push_in_list(&mut builder, chunk.iter().copied());
        for row in builder.build().fetch_all(&mut *conn).await? {
            owned.insert(row.get::<i64, _>("id"));
        }
    }

    // A half-applied reorder is worse than either outcome, so the moves are one transaction.
    // sqlx turns that into a savepoint when the caller already holds one, so this composes
    // instead of failing with "cannot start a transaction within a transaction".
    let mut tx = conn.begin().await?;
    let mut moved = 0;
    for (position, id) in ids.iter().enumerate() {
        if !owned.contains(id) {
            continue;
        }
        sqlx::query("UPDATE user_query SET position = ?3 WHERE id = ?1 AND id_user = ?2")
            .bind(*id)
            .bind(id_user)
            .bind(position as i64)
            .execute(&mut *tx)
            .await?;
        moved += 1;
    }
    tx.commit().await?;
    Ok(moved)
}

pub async fn delete_all_for_user(conn: &mut SqliteConnection, id_user: i64) -> Result<usize> {
    Ok(sqlx::query("DELETE FROM user_query WHERE id_user = ?1")
        .bind(id_user)
        .execute(&mut *conn)
        .await?
        .rows_affected() as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::dao::user as user_dao;
    use crate::db::support::{db, scalar};
    use crate::db::Db;
    use crate::domain::sort::{Order, Sort};

    async fn setup() -> (Db, i64) {
        let store = db().await;
        let id_user = user(&store, "alice").await;
        (store, id_user)
    }

    async fn user(db: &Db, username: &str) -> i64 {
        let username = username.to_owned();
        db.with(|mut conn| async move {
            user_dao::insert(
                &mut conn,
                &user_dao::NewUser {
                    username: &username,
                    email: None,
                    password_hash: "argon2id$hash",
                    is_admin: false,
                },
            )
            .await
        })
        .await
        .expect("create user")
    }

    fn saved(id_user: i64, name: &str, search: &str) -> SavedQuery {
        SavedQuery::new(id_user, name, search)
    }

    #[tokio::test]
    async fn a_saved_query_round_trips_every_editable_field() {
        let (store, id_user) = setup().await;
        store
            .with(|mut conn| async move {
                let mut q = saved(id_user, "unread rust", "intitle:rust -author:bot");
                q.scope = ListType::Category;
                q.scope_id = Some(4);
                q.state = EntryState::unread();
                q.sort = Sort::Date;
                q.order = Order::Asc;
                q.id = insert(&mut conn, &q).await?;

                let back = find_by_id(&mut conn, q.id, id_user).await?.expect("exists");
                assert_eq!(back.name, q.name);
                assert_eq!(back.search, q.search);
                assert_eq!(back.scope, ListType::Category);
                assert_eq!(back.scope_id, Some(4));
                assert_eq!(back.state, EntryState::unread());
                assert_eq!(back.sort, Sort::Date);
                assert_eq!(back.order, Order::Asc);
                Ok(())
            })
            .await
            .expect("round trip");
    }

    #[tokio::test]
    async fn the_state_survives_as_a_bitmask() {
        // The column is an integer, so a state that only the structural form can express would be lost.
        let (store, id_user) = setup().await;
        store
            .with(|mut conn| async move {
                let mut q = saved(id_user, "s", "x");
                q.state = EntryState::unread_or_starred();
                q.id = insert(&mut conn, &q).await?;
                assert_eq!(
                    find_by_id(&mut conn, q.id, id_user)
                        .await?
                        .expect("exists")
                        .state,
                    EntryState::unread_or_starred()
                );
                Ok(())
            })
            .await
            .expect("round trip");
    }

    #[tokio::test]
    async fn names_are_unique_per_user_but_not_across_users() {
        let (store, id_user) = setup().await;
        let other = user(&store, "bob").await;

        store
            .with(|mut conn| async move {
                let q = saved(id_user, "todo", "labels:todo");
                insert(&mut conn, &q).await?;
                // The same name for another user is a different saved search.
                insert(&mut conn, &saved(other, "todo", "labels:other")).await?;

                assert_eq!(
                    find_by_name(&mut conn, id_user, "todo")
                        .await?
                        .expect("exists")
                        .search,
                    "labels:todo"
                );
                assert_eq!(
                    find_by_name(&mut conn, other, "todo")
                        .await?
                        .expect("exists")
                        .search,
                    "labels:other"
                );
                assert!(find_by_name(&mut conn, id_user, "nope").await?.is_none());

                // A second `todo` for the same user is a constraint violation, not a silent overwrite.
                assert!(insert(&mut conn, &saved(id_user, "todo", "x"))
                    .await
                    .is_err());
                Ok(())
            })
            .await
            .expect("lookups");
    }

    #[tokio::test]
    async fn no_lookup_reaches_another_users_saved_search() {
        // `S:` resolves by id, so an unscoped lookup would be a read primitive over other readers'
        // queries.
        let (store, id_user) = setup().await;
        let other = user(&store, "bob").await;

        store
            .with(|mut conn| async move {
                let mut q = saved(other, "private", "intitle:secret");
                q.id = insert(&mut conn, &q).await?;

                assert!(find_by_id(&mut conn, q.id, id_user).await?.is_none());
                assert!(find_by_name(&mut conn, id_user, "private").await?.is_none());
                assert!(list(&mut conn, id_user).await?.is_empty());
                assert_eq!(list(&mut conn, other).await?.len(), 1);
                assert!(
                    !delete(&mut conn, q.id, id_user).await?,
                    "deleting someone else's row does nothing"
                );
                assert!(
                    !update(
                        &mut conn,
                        &SavedQuery {
                            id_user,
                            ..q.clone()
                        }
                    )
                    .await?
                );
                assert_eq!(
                    find_by_id(&mut conn, q.id, other)
                        .await?
                        .expect("still there")
                        .search,
                    "intitle:secret"
                );
                Ok(())
            })
            .await
            .expect("isolation");
    }

    #[tokio::test]
    async fn listing_is_ordered_by_position_then_name() {
        let (store, id_user) = setup().await;
        store
            .with(|mut conn| async move {
                for (name, position) in [("zulu", 1), ("alpha", 1), ("first", 0)] {
                    let mut q = saved(id_user, name, "x");
                    q.position = position;
                    insert(&mut conn, &q).await?;
                }
                let names: Vec<String> = list(&mut conn, id_user)
                    .await?
                    .into_iter()
                    .map(|q| q.name)
                    .collect();
                assert_eq!(names, vec!["first", "alpha", "zulu"]);
                Ok(())
            })
            .await
            .expect("listing");
    }

    #[tokio::test]
    async fn set_positions_assigns_from_the_slice_index() {
        let (store, id_user) = setup().await;
        store
            .with(|mut conn| async move {
                let mut ids = Vec::new();
                for name in ["a", "b", "c"] {
                    let q = saved(id_user, name, "x");
                    ids.push(insert(&mut conn, &q).await?);
                }
                ids.reverse();
                assert_eq!(set_positions(&mut conn, id_user, &ids).await?, 3);
                let names: Vec<String> = list(&mut conn, id_user)
                    .await?
                    .into_iter()
                    .map(|q| q.name)
                    .collect();
                assert_eq!(names, vec!["c", "b", "a"]);

                // An id belonging to someone else is skipped rather than moved.
                let other = user_dao::insert(
                    &mut conn,
                    &user_dao::NewUser {
                        username: "bob",
                        email: None,
                        password_hash: "argon2id$hash",
                        is_admin: false,
                    },
                )
                .await?;
                let foreign = insert(&mut conn, &saved(other, "intruder", "x")).await?;
                let moved = set_positions(&mut conn, id_user, &[foreign]).await?;
                assert_eq!(moved, 0, "another user's row is not this user's to reorder");
                // And it kept its own position.
                assert_eq!(
                    find_by_id(&mut conn, foreign, other)
                        .await?
                        .expect("exists")
                        .position,
                    0
                );
                Ok(())
            })
            .await
            .expect("reorder");
    }

    #[tokio::test]
    async fn deleting_a_user_removes_their_saved_searches() {
        let (store, id_user) = setup().await;
        store
            .with(|mut conn| async move {
                insert(&mut conn, &saved(id_user, "a", "x")).await?;
                assert_eq!(delete_all_for_user(&mut conn, id_user).await?, 1);
                assert!(list(&mut conn, id_user).await?.is_empty());
                Ok(())
            })
            .await
            .expect("cascade");
    }

    #[tokio::test]
    async fn a_corrupt_scope_degrades_rather_than_failing() {
        // The sidebar has to load even if a row was written by a build with a different scope set.
        let (store, id_user) = setup().await;
        store
            .with(|mut conn| async move {
                let q = saved(id_user, "a", "x");
                let id = insert(&mut conn, &q).await?;
                sqlx::query("UPDATE user_query SET get_type = 'QQ' WHERE id = ?1")
                    .bind(id)
                    .execute(&mut *conn)
                    .await?;
                assert_eq!(
                    find_by_id(&mut conn, id, id_user)
                        .await?
                        .expect("exists")
                        .scope,
                    ListType::MainStream
                );
                Ok(())
            })
            .await
            .expect("degrades");
    }

    #[tokio::test]
    async fn set_positions_joins_a_transaction_the_caller_already_holds() {
        // The reorder runs inside a savepoint, so a caller that opened a transaction still gets
        // all-or-nothing behaviour rather than a "cannot start a transaction within a transaction".
        let (store, id_user) = setup().await;
        store
            .with(|mut conn| async move {
                let mut ids = Vec::new();
                for name in ["a", "b"] {
                    ids.push(insert(&mut conn, &saved(id_user, name, "x")).await?);
                }
                ids.reverse();
                let mut tx = conn.begin().await?;
                assert_eq!(set_positions(&mut tx, id_user, &ids).await?, 2);
                tx.commit().await?;
                Ok(())
            })
            .await
            .expect("nested");
        let first: String = scalar(
            &store,
            "SELECT name FROM user_query ORDER BY position LIMIT 1",
        )
        .await;
        assert_eq!(first, "b");
    }
}
