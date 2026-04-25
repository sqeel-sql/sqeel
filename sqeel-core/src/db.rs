use crate::schema::SchemaNode;
use crate::state::QueryResult;
use sqlx::{
    Column, Row, TypeInfo,
    mysql::MySqlPool,
    postgres::PgPool,
    sqlite::{SqliteConnectOptions, SqlitePool},
};
use std::str::FromStr;

/// Outcome of `DbConnection::execute`. Row-returning queries (SELECT,
/// SHOW, EXPLAIN, …) produce a `Rows` result; statements that don't
/// produce a result set (INSERT/UPDATE/DELETE, CREATE/DROP/ALTER, …)
/// produce a `NonQuery` summary the UI can render as a status line
/// instead of an empty table.
pub enum ExecOutcome {
    Rows(QueryResult),
    NonQuery { verb: String, rows_affected: u64 },
}

/// Per-engine connection pool. Sqeel dispatches typed queries through the
/// matching variant so each engine can decode its native column types
/// (DATETIME, DECIMAL, JSON, BYTEA, UUID, …) without going through `sqlx::Any`.
pub enum Pool {
    MySql(MySqlPool),
    Pg(PgPool),
    Sqlite(SqlitePool),
}

pub struct DbConnection {
    pool: Pool,
    pub url: String,
}

impl DbConnection {
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        let pool = if url.starts_with("mysql://") || url.starts_with("mariadb://") {
            Pool::MySql(MySqlPool::connect(url).await?)
        } else if url.starts_with("postgres://") || url.starts_with("postgresql://") {
            Pool::Pg(PgPool::connect(url).await?)
        } else if url.starts_with("sqlite://") || url.starts_with("sqlite:") {
            // Match what every other SQL client does for sqlite: create
            // the DB file if it doesn't exist yet. Stops `--sandbox`
            // and "open my new project DB" both from failing on
            // first launch with a confusing "file not found" error.
            // Users who want strict "must exist" semantics can pass
            // `?mode=ro` or `?mode=rw` in the URL to override.
            let opts = SqliteConnectOptions::from_str(url)?.create_if_missing(true);
            Pool::Sqlite(SqlitePool::connect_with(opts).await?)
        } else {
            anyhow::bail!("Unsupported URL scheme: {url}");
        };
        Ok(Self {
            pool,
            url: url.to_string(),
        })
    }

    pub fn is_sqlite(&self) -> bool {
        matches!(self.pool, Pool::Sqlite(_))
    }

    /// Load just the database/schema names as collapsed nodes with no tables.
    /// This is fast and lets the UI show the structure before tables are loaded.
    pub async fn load_schema_databases(&self) -> anyhow::Result<Vec<SchemaNode>> {
        if self.is_sqlite() {
            return Ok(vec![SchemaNode::Database {
                name: "main".into(),
                expanded: true,
                tables: vec![],
                tables_loaded_at: None,
            }]);
        }
        let databases = self.list_databases().await?;
        Ok(databases
            .into_iter()
            .map(|name| SchemaNode::Database {
                name,
                expanded: false,
                tables: vec![],
                tables_loaded_at: None,
            })
            .collect())
    }

    pub async fn execute(&self, query: &str) -> anyhow::Result<ExecOutcome> {
        // Non-row statements (INSERT/UPDATE/DELETE/CREATE/DROP/…) go
        // through sqlx's `execute()` so we can surface rows_affected
        // in a dedicated results pane instead of pretending the empty
        // result set means "nothing happened".
        if let Some(verb) = non_query_verb(query) {
            let rows_affected = match &self.pool {
                Pool::MySql(p) => sqlx::query(query).execute(p).await?.rows_affected(),
                Pool::Pg(p) => sqlx::query(query).execute(p).await?.rows_affected(),
                Pool::Sqlite(p) => sqlx::query(query).execute(p).await?.rows_affected(),
            };
            return Ok(ExecOutcome::NonQuery {
                verb,
                rows_affected,
            });
        }

        let owned;
        let query = match apply_default_limit(query, DEFAULT_ROW_LIMIT) {
            Some(q) => {
                owned = q;
                owned.as_str()
            }
            None => query,
        };
        let (columns, rows) = match &self.pool {
            Pool::MySql(p) => {
                let rs = sqlx::query(query).fetch_all(p).await?;
                let cols = rs
                    .first()
                    .map(|r| r.columns().iter().map(|c| c.name().to_string()).collect())
                    .unwrap_or_default();
                let data = rs
                    .iter()
                    .map(|r| (0..r.columns().len()).map(|i| decode_mysql(r, i)).collect())
                    .collect();
                (cols, data)
            }
            Pool::Pg(p) => {
                let rs = sqlx::query(query).fetch_all(p).await?;
                let cols = rs
                    .first()
                    .map(|r| r.columns().iter().map(|c| c.name().to_string()).collect())
                    .unwrap_or_default();
                let data = rs
                    .iter()
                    .map(|r| (0..r.columns().len()).map(|i| decode_pg(r, i)).collect())
                    .collect();
                (cols, data)
            }
            Pool::Sqlite(p) => {
                let rs = sqlx::query(query).fetch_all(p).await?;
                let cols = rs
                    .first()
                    .map(|r| r.columns().iter().map(|c| c.name().to_string()).collect())
                    .unwrap_or_default();
                let data = rs
                    .iter()
                    .map(|r| {
                        (0..r.columns().len())
                            .map(|i| decode_sqlite(r, i))
                            .collect()
                    })
                    .collect();
                (cols, data)
            }
        };

        Ok(ExecOutcome::Rows(QueryResult {
            columns,
            rows,
            col_widths: vec![],
        }))
    }

    pub async fn list_databases(&self) -> anyhow::Result<Vec<String>> {
        match &self.pool {
            Pool::Sqlite(p) => {
                let rows = sqlx::query("PRAGMA database_list").fetch_all(p).await?;
                Ok(rows
                    .iter()
                    .map(|r| r.try_get::<String, _>(1).unwrap_or_else(|_| "main".into()))
                    .collect())
            }
            Pool::MySql(p) => {
                let rows = sqlx::query("SHOW DATABASES").fetch_all(p).await?;
                Ok(rows.iter().map(|r| mysql_string(r, 0)).collect())
            }
            Pool::Pg(p) => {
                let rows =
                    sqlx::query("SELECT datname FROM pg_database WHERE datistemplate = false")
                        .fetch_all(p)
                        .await?;
                Ok(rows
                    .iter()
                    .map(|r| r.try_get::<String, _>(0).unwrap_or_default())
                    .collect())
            }
        }
    }

    pub async fn list_tables(&self, database: &str) -> anyhow::Result<Vec<String>> {
        match &self.pool {
            Pool::MySql(p) => {
                let rows = sqlx::query(&format!("SHOW TABLES FROM `{database}`"))
                    .fetch_all(p)
                    .await?;
                Ok(rows.iter().map(|r| mysql_string(r, 0)).collect())
            }
            Pool::Sqlite(p) => {
                let rows =
                    sqlx::query("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
                        .fetch_all(p)
                        .await?;
                Ok(rows
                    .iter()
                    .map(|r| r.try_get::<String, _>(0).unwrap_or_default())
                    .collect())
            }
            Pool::Pg(p) => {
                let rows = sqlx::query(
                    "SELECT tablename FROM pg_tables WHERE schemaname = $1 ORDER BY tablename",
                )
                .bind(database)
                .fetch_all(p)
                .await?;
                Ok(rows
                    .iter()
                    .map(|r| r.try_get::<String, _>(0).unwrap_or_default())
                    .collect())
            }
        }
    }

    pub async fn list_columns(
        &self,
        database: &str,
        table: &str,
    ) -> anyhow::Result<Vec<ColumnInfo>> {
        match &self.pool {
            Pool::MySql(p) => {
                let rows = sqlx::query(
                    "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_KEY \
                     FROM information_schema.COLUMNS \
                     WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? \
                     ORDER BY ORDINAL_POSITION",
                )
                .bind(database)
                .bind(table)
                .fetch_all(p)
                .await?;
                Ok(rows
                    .iter()
                    .map(|r| ColumnInfo {
                        name: mysql_string(r, 0),
                        type_name: mysql_string(r, 1),
                        nullable: mysql_string(r, 2) == "YES",
                        is_pk: mysql_string(r, 3) == "PRI",
                    })
                    .collect())
            }
            Pool::Sqlite(p) => {
                let rows = sqlx::query(&format!("PRAGMA table_info({table})"))
                    .fetch_all(p)
                    .await?;
                Ok(rows
                    .iter()
                    .map(|r| ColumnInfo {
                        name: r.try_get::<String, _>(1).unwrap_or_default(),
                        type_name: r.try_get::<String, _>(2).unwrap_or_default(),
                        nullable: r.try_get::<i64, _>(3).unwrap_or(0) == 0,
                        is_pk: r.try_get::<i64, _>(5).unwrap_or(0) != 0,
                    })
                    .collect())
            }
            Pool::Pg(p) => {
                let rows = sqlx::query(
                    "SELECT c.column_name, c.data_type, c.is_nullable, \
                     COALESCE((SELECT 1 FROM information_schema.table_constraints tc \
                       JOIN information_schema.key_column_usage kcu \
                         ON tc.constraint_name = kcu.constraint_name \
                       WHERE tc.table_schema = $1 AND tc.table_name = $2 \
                         AND kcu.column_name = c.column_name \
                         AND tc.constraint_type = 'PRIMARY KEY' LIMIT 1), 0) AS is_pk \
                     FROM information_schema.columns c \
                     WHERE c.table_schema = $1 AND c.table_name = $2 \
                     ORDER BY c.ordinal_position",
                )
                .bind(database)
                .bind(table)
                .fetch_all(p)
                .await?;
                Ok(rows
                    .iter()
                    .map(|r| ColumnInfo {
                        name: r.try_get::<String, _>(0).unwrap_or_default(),
                        type_name: r.try_get::<String, _>(1).unwrap_or_default(),
                        nullable: r.try_get::<String, _>(2).unwrap_or_default() == "YES",
                        is_pk: r.try_get::<i32, _>(3).unwrap_or(0) != 0,
                    })
                    .collect())
            }
        }
    }

    /// Load the schema tree: databases + tables only (no columns — too slow to
    /// load eagerly for large schemas). Columns can be loaded on demand later.
    pub async fn load_schema(&self) -> anyhow::Result<Vec<SchemaNode>> {
        if self.is_sqlite() {
            let tables = self.list_tables("main").await.unwrap_or_default();
            let table_nodes = tables
                .into_iter()
                .map(|t| SchemaNode::Table {
                    name: t,
                    expanded: false,
                    columns: vec![],
                    columns_loaded_at: None,
                })
                .collect();
            return Ok(vec![SchemaNode::Database {
                name: "main".into(),
                expanded: true,
                tables: table_nodes,
                tables_loaded_at: Some(std::time::Instant::now()),
            }]);
        }

        let databases = self.list_databases().await?;
        let mut nodes = Vec::new();
        for db in databases {
            let tables = self.list_tables(&db).await.unwrap_or_default();
            let table_nodes = tables
                .into_iter()
                .map(|t| SchemaNode::Table {
                    name: t,
                    expanded: false,
                    columns: vec![],
                    columns_loaded_at: None,
                })
                .collect();
            nodes.push(SchemaNode::Database {
                name: db,
                expanded: false,
                tables: table_nodes,
                tables_loaded_at: Some(std::time::Instant::now()),
            });
        }
        Ok(nodes)
    }
}

#[derive(Debug, Clone)]
pub struct ColumnInfo {
    pub name: String,
    pub type_name: String,
    pub nullable: bool,
    pub is_pk: bool,
}

macro_rules! raw_is_null {
    ($row:expr, $idx:expr) => {{
        use sqlx::ValueRef;
        $row.try_get_raw($idx).map(|v| v.is_null()).unwrap_or(false)
    }};
}

/// Decode a MySQL column as a String, falling back to raw bytes (utf8) for
/// columns returned as binary (e.g. `SHOW DATABASES`/`SHOW TABLES` on some
/// servers return VARBINARY).
fn mysql_string(row: &sqlx::mysql::MySqlRow, idx: usize) -> String {
    if let Ok(s) = row.try_get::<String, _>(idx) {
        return s;
    }
    if let Ok(b) = row.try_get::<Vec<u8>, _>(idx) {
        return bytes_to_display(&b);
    }
    String::new()
}

fn bytes_to_display(v: &[u8]) -> String {
    match std::str::from_utf8(v) {
        Ok(s) => s.to_string(),
        Err(_) => v.iter().map(|b| format!("{b:02x}")).collect(),
    }
}

fn decode_mysql(row: &sqlx::mysql::MySqlRow, idx: usize) -> String {
    if raw_is_null!(row, idx) {
        return "NULL".into();
    }
    let ty = row.columns()[idx].type_info().name().to_ascii_uppercase();
    match ty.as_str() {
        "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "BIGINT" => {
            if let Ok(v) = row.try_get::<i64, _>(idx) {
                return v.to_string();
            }
        }
        "TINYINT UNSIGNED" | "SMALLINT UNSIGNED" | "MEDIUMINT UNSIGNED" | "INT UNSIGNED"
        | "BIGINT UNSIGNED" => {
            if let Ok(v) = row.try_get::<u64, _>(idx) {
                return v.to_string();
            }
        }
        "BOOLEAN" => {
            if let Ok(v) = row.try_get::<bool, _>(idx) {
                return v.to_string();
            }
        }
        "FLOAT" | "DOUBLE" => {
            if let Ok(v) = row.try_get::<f64, _>(idx) {
                return v.to_string();
            }
        }
        "DECIMAL" | "NUMERIC" => {
            if let Ok(v) = row.try_get::<bigdecimal::BigDecimal, _>(idx) {
                return v.to_string();
            }
        }
        "DATE" => {
            if let Ok(v) = row.try_get::<chrono::NaiveDate, _>(idx) {
                return v.to_string();
            }
        }
        "TIME" => {
            if let Ok(v) = row.try_get::<chrono::NaiveTime, _>(idx) {
                return v.to_string();
            }
        }
        "DATETIME" => {
            if let Ok(v) = row.try_get::<chrono::NaiveDateTime, _>(idx) {
                return v.to_string();
            }
        }
        "TIMESTAMP" => {
            if let Ok(v) = row.try_get::<chrono::DateTime<chrono::Utc>, _>(idx) {
                return v.to_rfc3339();
            }
            if let Ok(v) = row.try_get::<chrono::NaiveDateTime, _>(idx) {
                return v.to_string();
            }
        }
        "JSON" => {
            if let Ok(v) = row.try_get::<serde_json::Value, _>(idx) {
                return v.to_string();
            }
        }
        "BLOB" | "TINYBLOB" | "MEDIUMBLOB" | "LONGBLOB" | "BINARY" | "VARBINARY" => {
            if let Ok(v) = row.try_get::<Vec<u8>, _>(idx) {
                return bytes_to_display(&v);
            }
        }
        "CHAR" | "VARCHAR" | "TEXT" | "TINYTEXT" | "MEDIUMTEXT" | "LONGTEXT" | "ENUM" | "SET" => {
            if let Ok(v) = row.try_get::<String, _>(idx) {
                return v;
            }
        }
        _ => {}
    }
    // Fallback probe ladder — bool moved after numerics so integer columns
    // with unknown type names don't get stringified as true/false.
    if let Ok(v) = row.try_get::<String, _>(idx) {
        return v;
    }
    if let Ok(v) = row.try_get::<i64, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<u64, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<f64, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<bigdecimal::BigDecimal, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<chrono::NaiveDateTime, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<chrono::NaiveDate, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<chrono::NaiveTime, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<chrono::DateTime<chrono::Utc>, _>(idx) {
        return v.to_rfc3339();
    }
    if let Ok(v) = row.try_get::<serde_json::Value, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<bool, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<Vec<u8>, _>(idx) {
        return bytes_to_display(&v);
    }
    "?".into()
}

fn decode_pg(row: &sqlx::postgres::PgRow, idx: usize) -> String {
    if raw_is_null!(row, idx) {
        return "NULL".into();
    }
    let ty = row.columns()[idx].type_info().name().to_ascii_uppercase();
    match ty.as_str() {
        "BOOL" => {
            if let Ok(v) = row.try_get::<bool, _>(idx) {
                return v.to_string();
            }
        }
        "INT2" => {
            if let Ok(v) = row.try_get::<i16, _>(idx) {
                return v.to_string();
            }
        }
        "INT4" => {
            if let Ok(v) = row.try_get::<i32, _>(idx) {
                return v.to_string();
            }
        }
        "INT8" => {
            if let Ok(v) = row.try_get::<i64, _>(idx) {
                return v.to_string();
            }
        }
        "FLOAT4" => {
            if let Ok(v) = row.try_get::<f32, _>(idx) {
                return v.to_string();
            }
        }
        "FLOAT8" => {
            if let Ok(v) = row.try_get::<f64, _>(idx) {
                return v.to_string();
            }
        }
        "NUMERIC" => {
            if let Ok(v) = row.try_get::<bigdecimal::BigDecimal, _>(idx) {
                return v.to_string();
            }
        }
        "UUID" => {
            if let Ok(v) = row.try_get::<uuid::Uuid, _>(idx) {
                return v.to_string();
            }
        }
        "DATE" => {
            if let Ok(v) = row.try_get::<chrono::NaiveDate, _>(idx) {
                return v.to_string();
            }
        }
        "TIME" => {
            if let Ok(v) = row.try_get::<chrono::NaiveTime, _>(idx) {
                return v.to_string();
            }
        }
        "TIMESTAMP" => {
            if let Ok(v) = row.try_get::<chrono::NaiveDateTime, _>(idx) {
                return v.to_string();
            }
        }
        "TIMESTAMPTZ" => {
            if let Ok(v) = row.try_get::<chrono::DateTime<chrono::Utc>, _>(idx) {
                return v.to_rfc3339();
            }
        }
        "JSON" | "JSONB" => {
            if let Ok(v) = row.try_get::<serde_json::Value, _>(idx) {
                return v.to_string();
            }
        }
        "BYTEA" => {
            if let Ok(v) = row.try_get::<Vec<u8>, _>(idx) {
                return bytes_to_display(&v);
            }
        }
        "TEXT" | "VARCHAR" | "BPCHAR" | "NAME" | "CITEXT" => {
            if let Ok(v) = row.try_get::<String, _>(idx) {
                return v;
            }
        }
        _ => {}
    }
    // Fallback probe ladder — bool moved after numerics.
    if let Ok(v) = row.try_get::<String, _>(idx) {
        return v;
    }
    if let Ok(v) = row.try_get::<i64, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<i32, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<i16, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<f64, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<f32, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<bigdecimal::BigDecimal, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<uuid::Uuid, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<chrono::NaiveDateTime, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<chrono::NaiveDate, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<chrono::NaiveTime, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<chrono::DateTime<chrono::Utc>, _>(idx) {
        return v.to_rfc3339();
    }
    if let Ok(v) = row.try_get::<serde_json::Value, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<bool, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<Vec<u8>, _>(idx) {
        return bytes_to_display(&v);
    }
    "?".into()
}

fn decode_sqlite(row: &sqlx::sqlite::SqliteRow, idx: usize) -> String {
    if raw_is_null!(row, idx) {
        return "NULL".into();
    }
    let ty = row.columns()[idx].type_info().name().to_ascii_uppercase();
    match ty.as_str() {
        "INTEGER" => {
            if let Ok(v) = row.try_get::<i64, _>(idx) {
                return v.to_string();
            }
        }
        "REAL" => {
            if let Ok(v) = row.try_get::<f64, _>(idx) {
                return v.to_string();
            }
        }
        "TEXT" => {
            if let Ok(v) = row.try_get::<String, _>(idx) {
                return v;
            }
        }
        "BLOB" => {
            if let Ok(v) = row.try_get::<Vec<u8>, _>(idx) {
                return bytes_to_display(&v);
            }
        }
        "BOOLEAN" => {
            if let Ok(v) = row.try_get::<bool, _>(idx) {
                return v.to_string();
            }
        }
        _ => {}
    }
    if let Ok(v) = row.try_get::<String, _>(idx) {
        return v;
    }
    if let Ok(v) = row.try_get::<i64, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<f64, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<bool, _>(idx) {
        return v.to_string();
    }
    if let Ok(v) = row.try_get::<Vec<u8>, _>(idx) {
        return bytes_to_display(&v);
    }
    "?".into()
}

/// Rows added automatically when a SELECT/WITH query has no LIMIT clause.
pub const DEFAULT_ROW_LIMIT: usize = 100;

/// Returns the leading uppercase keyword if `query` is a non-row
/// statement (DML / DDL / transaction control / etc), else `None`.
/// Used by `execute` to dispatch to sqlx's `execute()` and surface
/// rows_affected instead of an empty result set.
///
/// Row-returning verbs we leave for the fetch_all path:
/// SELECT, WITH, VALUES, SHOW, EXPLAIN, DESC[RIBE], TABLE, PRAGMA.
/// Anything else with a recognisable verb is treated as non-row.
/// An unrecognisable / empty query falls through to fetch_all so
/// sqlx surfaces its own parse error.
fn non_query_verb(query: &str) -> Option<String> {
    let stripped = skip_leading_whitespace_and_comments(query.trim_start());
    let kw = leading_keyword(stripped)?.to_ascii_uppercase();
    let row_returning = matches!(
        kw.as_str(),
        "SELECT"
            | "WITH"
            | "VALUES"
            | "SHOW"
            | "EXPLAIN"
            | "DESC"
            | "DESCRIBE"
            | "TABLE"
            | "PRAGMA"
    );
    if row_returning { None } else { Some(kw) }
}

/// If `query` is a top-level SELECT or WITH statement with no LIMIT clause,
/// return a rewritten query with ` LIMIT <limit>` appended. Returns `None`
/// when the query already limits itself or isn't a row-producing statement.
pub fn apply_default_limit(query: &str, limit: usize) -> Option<String> {
    let trimmed = strip_trailing_semicolons(query).trim();
    if trimmed.is_empty() {
        return None;
    }
    let after_comments = skip_leading_whitespace_and_comments(trimmed);
    let first_kw = leading_keyword(after_comments)?.to_ascii_uppercase();
    if first_kw != "SELECT" && first_kw != "WITH" {
        return None;
    }
    if has_top_level_keyword(trimmed, "LIMIT") {
        return None;
    }
    Some(format!("{trimmed} LIMIT {limit}"))
}

fn strip_trailing_semicolons(q: &str) -> &str {
    q.trim_end().trim_end_matches(';').trim_end()
}

fn skip_leading_whitespace_and_comments(mut s: &str) -> &str {
    loop {
        let before = s;
        s = s.trim_start();
        if let Some(rest) = s.strip_prefix("--") {
            s = rest.split_once('\n').map(|(_, r)| r).unwrap_or("");
        } else if let Some(rest) = s.strip_prefix("/*") {
            s = rest.split_once("*/").map(|(_, r)| r).unwrap_or("");
        }
        if s == before {
            return s;
        }
    }
}

fn leading_keyword(s: &str) -> Option<&str> {
    let end = s
        .char_indices()
        .find(|(_, c)| !c.is_ascii_alphabetic())
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    if end == 0 { None } else { Some(&s[..end]) }
}

/// Scan `q` for `needle` (case-insensitive, whole word) appearing at
/// paren-depth 0 and outside of string/identifier literals and comments.
fn has_top_level_keyword(q: &str, needle: &str) -> bool {
    let bytes = q.as_bytes();
    let n = bytes.len();
    let mut i = 0;
    let mut depth: i32 = 0;
    while i < n {
        let b = bytes[i];
        match b {
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth -= 1;
                i += 1;
            }
            b'\'' | b'"' | b'`' => {
                let quote = b;
                i += 1;
                while i < n {
                    if bytes[i] == b'\\' && i + 1 < n {
                        i += 2;
                        continue;
                    }
                    if bytes[i] == quote {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            b'-' if i + 1 < n && bytes[i + 1] == b'-' => {
                while i < n && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < n && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < n && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(n);
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let start = i;
                while i < n && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                if depth == 0 && q[start..i].eq_ignore_ascii_case(needle) {
                    return true;
                }
            }
            _ => i += 1,
        }
    }
    false
}

#[cfg(test)]
mod limit_tests {
    use super::*;

    fn apply(q: &str) -> Option<String> {
        apply_default_limit(q, 100)
    }

    #[test]
    fn appends_to_bare_select() {
        assert_eq!(
            apply("SELECT * FROM t"),
            Some("SELECT * FROM t LIMIT 100".into())
        );
    }

    #[test]
    fn strips_trailing_semicolon_before_appending() {
        assert_eq!(
            apply("select id from users;"),
            Some("select id from users LIMIT 100".into())
        );
    }

    #[test]
    fn leaves_query_that_already_limits() {
        assert_eq!(apply("SELECT * FROM t LIMIT 5"), None);
        assert_eq!(apply("select * from t limit 5 offset 10"), None);
    }

    #[test]
    fn ignores_limit_inside_subquery_paren() {
        let q = "SELECT * FROM (SELECT id FROM t LIMIT 5) x";
        assert_eq!(
            apply(q),
            Some("SELECT * FROM (SELECT id FROM t LIMIT 5) x LIMIT 100".into())
        );
    }

    #[test]
    fn ignores_limit_inside_string_literal() {
        assert!(apply("SELECT 'has LIMIT in string' AS x").is_some());
    }

    #[test]
    fn handles_with_cte() {
        let q = "WITH x AS (SELECT 1) SELECT * FROM x";
        assert_eq!(
            apply(q),
            Some("WITH x AS (SELECT 1) SELECT * FROM x LIMIT 100".into())
        );
    }

    #[test]
    fn skips_non_select() {
        assert_eq!(apply("INSERT INTO t VALUES (1)"), None);
        assert_eq!(apply("UPDATE t SET x = 1"), None);
        assert_eq!(apply("DELETE FROM t"), None);
        assert_eq!(apply("EXPLAIN SELECT * FROM t"), None);
    }

    #[test]
    fn skips_leading_comments() {
        let q = "-- fetch users\nSELECT * FROM users";
        let out = apply(q).unwrap();
        assert!(out.ends_with(" LIMIT 100"));
        assert!(out.contains("SELECT * FROM users"));
    }
}
