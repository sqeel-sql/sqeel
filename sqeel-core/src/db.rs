use crate::schema::SchemaNode;
use crate::state::QueryResult;
use sqlx::{AnyPool, Column, Row};

pub struct DbConnection {
    pool: AnyPool,
    pub url: String,
}

impl DbConnection {
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        sqlx::any::install_default_drivers();
        let pool = AnyPool::connect(url).await?;
        Ok(Self {
            pool,
            url: url.to_string(),
        })
    }

    fn is_mysql(&self) -> bool {
        self.url.starts_with("mysql://") || self.url.starts_with("mariadb://")
    }

    pub fn is_sqlite(&self) -> bool {
        self.url.starts_with("sqlite://") || self.url.starts_with("sqlite:")
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
        let rows = sqlx::query(query).fetch_all(&self.pool).await?;

        if rows.is_empty() {
            return Ok(QueryResult {
                columns: vec![],
                rows: vec![],
                col_widths: vec![],
            });
        }

        let columns: Vec<String> = rows[0]
            .columns()
            .iter()
            .map(|c| c.name().to_string())
            .collect();

        let result_rows: Vec<Vec<String>> = rows
            .iter()
            .map(|row| {
                (0..row.columns().len())
                    .map(|i| decode_cell_any(row, i))
                    .collect()
            })
            .collect();

        Ok(QueryResult {
            columns,
            rows: result_rows,
            col_widths: vec![],
        })
    }

    pub async fn list_databases(&self) -> anyhow::Result<Vec<String>> {
        if self.is_sqlite() {
            // PRAGMA database_list: seq(i64) | name(str) | file(str)
            let rows = sqlx::query("PRAGMA database_list")
                .fetch_all(&self.pool)
                .await?;
            return Ok(rows
                .iter()
                .map(|r| r.try_get::<String, _>(1).unwrap_or_else(|_| "main".into()))
                .collect());
        }
        let query = if self.is_mysql() {
            "SHOW DATABASES"
        } else {
            "SELECT datname FROM pg_database WHERE datistemplate = false"
        };
        let rows = sqlx::query(query).fetch_all(&self.pool).await?;
        Ok(rows
            .iter()
            .map(|r| r.try_get::<String, _>(0).unwrap_or_default())
            .collect())
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

    pub async fn list_tables(&self, database: &str) -> anyhow::Result<Vec<String>> {
        let query = if self.is_mysql() {
            format!("SHOW TABLES FROM `{database}`")
        } else if self.is_sqlite() {
            "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name".into()
        } else {
            format!(
                "SELECT tablename FROM pg_tables WHERE schemaname='public' AND tableowner='{database}'"
            )
        };
        let rows = sqlx::query(&query).fetch_all(&self.pool).await?;
        Ok(rows
            .iter()
            .map(|r| r.try_get::<String, _>(0).unwrap_or_default())
            .collect())
    }

    pub async fn list_columns(
        &self,
        database: &str,
        table: &str,
    ) -> anyhow::Result<Vec<ColumnInfo>> {
        if self.is_mysql() {
            let rows = sqlx::query(&format!(
                "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_KEY \
                 FROM information_schema.COLUMNS \
                 WHERE TABLE_SCHEMA = '{database}' AND TABLE_NAME = '{table}' \
                 ORDER BY ORDINAL_POSITION"
            ))
            .fetch_all(&self.pool)
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
        } else if self.is_sqlite() {
            let rows = sqlx::query(&format!("PRAGMA table_info({table})"))
                .fetch_all(&self.pool)
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
        } else {
            let rows = sqlx::query(&format!(
                "SELECT column_name, data_type, is_nullable, \
                 (SELECT COUNT(*) FROM information_schema.table_constraints tc \
                  JOIN information_schema.key_column_usage kcu \
                  ON tc.constraint_name = kcu.constraint_name \
                  WHERE tc.table_name = '{table}' AND kcu.column_name = c.column_name \
                  AND tc.constraint_type = 'PRIMARY KEY') AS is_pk \
                 FROM information_schema.columns c \
                 WHERE table_name = '{table}' ORDER BY ordinal_position"
            ))
            .fetch_all(&self.pool)
            .await?;
            Ok(rows
                .iter()
                .map(|r| ColumnInfo {
                    name: r.try_get::<String, _>(0).unwrap_or_default(),
                    type_name: r.try_get::<String, _>(1).unwrap_or_default(),
                    nullable: r.try_get::<String, _>(2).unwrap_or_default() == "YES",
                    is_pk: r.try_get::<i64, _>(3).unwrap_or(0) != 0,
                })
                .collect())
        }
    }
}

#[derive(Debug, Clone)]
pub struct ColumnInfo {
    pub name: String,
    pub type_name: String,
    pub nullable: bool,
    pub is_pk: bool,
}

fn decode_cell_any(row: &sqlx::any::AnyRow, idx: usize) -> String {
    row.try_get::<String, _>(idx)
        .or_else(|_| row.try_get::<i64, _>(idx).map(|v| v.to_string()))
        .or_else(|_| row.try_get::<f64, _>(idx).map(|v| v.to_string()))
        .or_else(|_| row.try_get::<bool, _>(idx).map(|v| v.to_string()))
        .or_else(|_| {
            row.try_get::<Option<String>, _>(idx)
                .map(|v| v.unwrap_or_else(|| "NULL".into()))
        })
        .unwrap_or_else(|_| "?".into())
}
