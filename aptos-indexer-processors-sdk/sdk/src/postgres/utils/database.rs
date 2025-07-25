// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

//! Database-related functions
#![allow(clippy::extra_unused_lifetimes)]

use crate::utils::{convert::remove_null_bytes, errors::ProcessorError};
use ahash::AHashMap;
use diesel::{query_builder::QueryFragment, ConnectionResult, QueryResult};
use diesel_async::{
    pooled_connection::{
        bb8::{Pool, PooledConnection},
        AsyncDieselConnectionManager, ManagerConfig, PoolError,
    },
    AsyncPgConnection, RunQueryDsl,
};
use diesel_migrations::{EmbeddedMigrations, MigrationHarness};
use futures_util::{future::BoxFuture, FutureExt};
use std::sync::Arc;
use tracing::{info, warn};

pub type Backend = diesel::pg::Pg;

pub type MyDbConnection = AsyncPgConnection;
pub type DbPool = Pool<MyDbConnection>;
pub type ArcDbPool = Arc<DbPool>;
pub type DbPoolConnection<'a> = PooledConnection<'a, MyDbConnection>;

pub const DEFAULT_MAX_POOL_SIZE: u32 = 150;

// the max is actually u16::MAX but we see that when the size is too big we get an overflow error so reducing it a bit
pub const MAX_DIESEL_PARAM_SIZE: usize = (u16::MAX / 2) as usize;

/// This function will clean the data for postgres. Currently it has support for removing
/// null bytes from strings but in the future we will add more functionality.
pub fn clean_data_for_db<T: serde::Serialize + for<'de> serde::Deserialize<'de>>(
    items: Vec<T>,
    should_remove_null_bytes: bool,
) -> Vec<T> {
    if should_remove_null_bytes {
        items.iter().map(remove_null_bytes).collect()
    } else {
        items
    }
}

fn establish_connection(database_url: &str) -> BoxFuture<ConnectionResult<AsyncPgConnection>> {
    use native_tls::{Certificate, TlsConnector};
    use postgres_native_tls::MakeTlsConnector;

    (async move {
        let (url, cert_path) = parse_and_clean_db_url(database_url);
        let cert = std::fs::read(cert_path.unwrap()).expect("Could not read certificate");

        let cert = Certificate::from_pem(&cert).expect("Could not parse certificate");
        let connector = TlsConnector::builder()
            .danger_accept_invalid_certs(true)
            .add_root_certificate(cert)
            .build()
            .expect("Could not build TLS connector");
        let connector = MakeTlsConnector::new(connector);

        let (client, connection) = tokio_postgres::connect(&url, connector)
            .await
            .expect("Could not connect to database");
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                eprintln!("connection error: {e}");
            }
        });
        AsyncPgConnection::try_from(client).await
    })
    .boxed()
}

fn parse_and_clean_db_url(url: &str) -> (String, Option<String>) {
    let mut db_url = url::Url::parse(url).expect("Could not parse database url");
    let mut cert_path = None;

    let mut query = "".to_string();
    db_url.query_pairs().for_each(|(k, v)| {
        if k == "sslrootcert" {
            cert_path = Some(v.parse().unwrap());
        } else {
            query.push_str(&format!("{k}={v}&"));
        }
    });
    db_url.set_query(Some(&query));

    (db_url.to_string(), cert_path)
}

pub async fn new_db_pool(
    database_url: &str,
    max_pool_size: Option<u32>,
) -> Result<ArcDbPool, PoolError> {
    let (_url, cert_path) = parse_and_clean_db_url(database_url);

    let config = if cert_path.is_some() {
        let mut config = ManagerConfig::<MyDbConnection>::default();
        config.custom_setup = Box::new(|conn| Box::pin(establish_connection(conn)));
        AsyncDieselConnectionManager::<MyDbConnection>::new_with_config(database_url, config)
    } else {
        AsyncDieselConnectionManager::<MyDbConnection>::new(database_url)
    };
    let pool = Pool::builder()
        .max_size(max_pool_size.unwrap_or(DEFAULT_MAX_POOL_SIZE))
        .build(config)
        .await?;
    Ok(Arc::new(pool))
}

pub async fn execute_in_chunks<U, T>(
    conn: ArcDbPool,
    build_query: fn(Vec<T>) -> U,
    items_to_insert: &[T],
    chunk_size: usize,
) -> Result<(), ProcessorError>
where
    U: QueryFragment<Backend> + diesel::query_builder::QueryId + Send + 'static,
    T: serde::Serialize + for<'de> serde::Deserialize<'de> + Clone + Send + 'static,
{
    let tasks = items_to_insert
        .chunks(chunk_size)
        .map(|chunk| {
            let conn = conn.clone();
            let items = chunk.to_vec();
            tokio::spawn(async move {
                let query = build_query(items.clone());
                execute_or_retry_cleaned(conn, build_query, items, query).await
            })
        })
        .collect::<Vec<_>>();

    let results = futures_util::future::try_join_all(tasks)
        .await
        .expect("Task panicked executing in chunks");
    for res in results {
        res?
    }

    Ok(())
}

/// Returns the entry for the config hashmap, or the default field count for the insert.
///
/// Given diesel has a limit of how many parameters can be inserted in a single operation (u16::MAX),
/// we default to chunk an array of items based on how many columns are in the table.
pub fn get_config_table_chunk_size<T: field_count::FieldCount>(
    table_name: &str,
    per_table_chunk_sizes: &AHashMap<String, usize>,
) -> usize {
    let chunk_size = per_table_chunk_sizes.get(table_name).copied();
    chunk_size.unwrap_or_else(|| MAX_DIESEL_PARAM_SIZE / T::field_count())
}

pub async fn execute_with_better_error<U>(
    pool: ArcDbPool,
    query: U,
) -> Result<usize, ProcessorError>
where
    U: QueryFragment<Backend> + diesel::query_builder::QueryId + Send,
{
    let debug_string = diesel::debug_query::<Backend, _>(&query).to_string();
    let conn = &mut pool.get().await.map_err(|e| {
        warn!("Error getting connection from pool: {:?}", e);
        ProcessorError::DBStoreError {
            message: format!("{e:#}"),
            query: Some(debug_string.clone()),
        }
    })?;
    query
        .execute(conn)
        .await
        .inspect_err(|e| {
            warn!("Error running query: {:?}\n{:?}", e, debug_string);
        })
        .map_err(|e| ProcessorError::DBStoreError {
            message: format!("{e:#}"),
            query: Some(debug_string),
        })
}

pub async fn execute_with_better_error_conn<U>(
    conn: &mut MyDbConnection,
    query: U,
) -> QueryResult<usize>
where
    U: QueryFragment<Backend> + diesel::query_builder::QueryId + Send,
{
    let debug_string = diesel::debug_query::<Backend, _>(&query).to_string();
    tracing::debug!("Executing query: {:?}", debug_string);
    let res = query.execute(conn).await;
    if let Err(ref e) = res {
        tracing::warn!("Error running query: {:?}\n{:?}", e, debug_string);
    }
    res
}

async fn execute_or_retry_cleaned<U, T>(
    conn: ArcDbPool,
    build_query: fn(Vec<T>) -> U,
    items: Vec<T>,
    query: U,
) -> Result<(), ProcessorError>
where
    U: QueryFragment<Backend> + diesel::query_builder::QueryId + Send,
    T: serde::Serialize + for<'de> serde::Deserialize<'de> + Clone,
{
    match execute_with_better_error(conn.clone(), query).await {
        Ok(_) => {},
        Err(_) => {
            let cleaned_items = clean_data_for_db(items, true);
            let cleaned_query = build_query(cleaned_items);
            match execute_with_better_error(conn.clone(), cleaned_query).await {
                Ok(_) => {},
                Err(e) => {
                    return Err(e);
                },
            }
        },
    }
    Ok(())
}

pub fn run_pending_migrations<DB: diesel::backend::Backend>(
    conn: &mut impl MigrationHarness<DB>,
    migrations: EmbeddedMigrations,
) {
    conn.run_pending_migrations(migrations)
        .expect("[Parser] Migrations failed!");
}

// For the normal processor build we just use standard Diesel with the postgres
// feature enabled (which uses libpq under the hood, hence why we named the feature
// this way).
#[cfg(feature = "postgres_full")]
pub async fn run_migrations(
    postgres_connection_string: String,
    _conn_pool: ArcDbPool,
    migrations: EmbeddedMigrations,
) {
    use diesel::{Connection, PgConnection};

    info!("Running migrations: {:?}", postgres_connection_string);
    let migration_time = std::time::Instant::now();
    let mut conn =
        PgConnection::establish(&postgres_connection_string).expect("migrations failed!");
    run_pending_migrations(&mut conn, migrations);
    info!(
        duration_in_secs = migration_time.elapsed().as_secs_f64(),
        "[Parser] Finished migrations"
    );
}

// If the postgres_full feature isn't enabled, we use diesel async instead. This is used by
// the CLI for the local testnet, where we cannot tolerate the libpq dependency.
#[cfg(not(feature = "postgres_full"))]
pub async fn run_migrations(
    postgres_connection_string: String,
    conn_pool: ArcDbPool,
    migrations: EmbeddedMigrations,
) {
    use diesel_async::async_connection_wrapper::AsyncConnectionWrapper;

    info!("Running migrations: {:?}", postgres_connection_string);
    let conn = conn_pool
        // We need to use this since AsyncConnectionWrapper doesn't know how to
        // work with a pooled connection.
        .dedicated_connection()
        .await
        .expect("[Parser] Failed to get connection");
    // We use spawn_blocking since run_pending_migrations is a blocking function.
    tokio::task::spawn_blocking(move || {
        // This lets us use the connection like a normal diesel connection. See more:
        // https://docs.rs/diesel-async/latest/diesel_async/async_connection_wrapper/type.AsyncConnectionWrapper.html
        let mut conn: AsyncConnectionWrapper<diesel_async::AsyncPgConnection> =
            AsyncConnectionWrapper::from(conn);
        run_pending_migrations(&mut conn, migrations);
    })
    .await
    .expect("[Parser] Failed to run migrations");
}

pub struct DbContext<'a> {
    pub conn: DbPoolConnection<'a>,
    pub query_retries: u32,
    pub query_retry_delay_ms: u64,
}
