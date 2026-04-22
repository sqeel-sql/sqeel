use crate::schema::SchemaNode;
use crate::state::QueryResult;
use sqlx::{Column, Row, mysql::MySqlPool, postgres::PgPool, sqlite::SqlitePool};

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
            Pool::Sqlite(SqlitePool::connect(url).await?)
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
            }]);
        }
        let databases = self.list_databases().await?;
        Ok(databases
            .into_iter()
            .map(|name| SchemaNode::Database {
                name,
                expanded: false,
                tables: vec![],
            })
            .collect())
    }

    pub async fn execute(&self, query: &str) -> anyhow::Result<QueryResult> {
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

        Ok(QueryResult {
            columns,
            rows,
            col_widths: vec![],
        })
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
                        name: r.try_get::<String, _>(0).unwrap_or_default(),
                        type_name: r.try_get::<String, _>(1).unwrap_or_default(),
                        nullable: r.try_get::<String, _>(2).unwrap_or_default() == "YES",
                        is_pk: r.try_get::<String, _>(3).unwrap_or_default() == "PRI",
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
                })
                .collect();
            return Ok(vec![SchemaNode::Database {
                name: "main".into(),
                expanded: true,
                tables: table_nodes,
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
                })
                .collect();
            nodes.push(SchemaNode::Database {
                name: db,
                expanded: false,
                tables: table_nodes,
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
    if let Ok(v) = row.try_get::<String, _>(idx) {
        return v;
    }
    if let Ok(v) = row.try_get::<bool, _>(idx) {
        return v.to_string();
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
    if let Ok(v) = row.try_get::<Vec<u8>, _>(idx) {
        return bytes_to_display(&v);
    }
    "?".into()
}

fn decode_pg(row: &sqlx::postgres::PgRow, idx: usize) -> String {
    if raw_is_null!(row, idx) {
        return "NULL".into();
    }
    if let Ok(v) = row.try_get::<String, _>(idx) {
        return v;
    }
    if let Ok(v) = row.try_get::<bool, _>(idx) {
        return v.to_string();
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
    if let Ok(v) = row.try_get::<Vec<u8>, _>(idx) {
        return bytes_to_display(&v);
    }
    "?".into()
}

fn decode_sqlite(row: &sqlx::sqlite::SqliteRow, idx: usize) -> String {
    if raw_is_null!(row, idx) {
        return "NULL".into();
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
