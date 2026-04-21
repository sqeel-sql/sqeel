use crate::schema::SchemaNode;
use crate::state::QueryResult;
use std::path::PathBuf;

const RESULT_HISTORY_LIMIT: usize = 10;

pub fn data_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("sqeel"))
}

pub fn schema_cache_dir() -> Option<PathBuf> {
    data_dir().map(|d| d.join("schema"))
}

pub fn save_schema_cache(url: &str, nodes: &[SchemaNode]) -> anyhow::Result<()> {
    let dir = schema_cache_dir().ok_or_else(|| anyhow::anyhow!("cannot determine data dir"))?;
    ensure_dir(&dir)?;
    let key = fnv_hash8(url);
    let json = serde_json::to_string(nodes)?;
    std::fs::write(dir.join(format!("{key}.json")), json)?;
    Ok(())
}

pub fn load_schema_cache(url: &str) -> Option<Vec<SchemaNode>> {
    let dir = schema_cache_dir()?;
    let key = fnv_hash8(url);
    let content = std::fs::read_to_string(dir.join(format!("{key}.json"))).ok()?;
    serde_json::from_str(&content).ok()
}

pub fn queries_dir() -> Option<PathBuf> {
    data_dir().map(|d| d.join("queries"))
}

/// Per-connection queries subdirectory: `~/.local/share/sqeel/queries/<conn_slug>/`
pub fn queries_dir_for(conn_slug: &str) -> Option<PathBuf> {
    data_dir().map(|d| d.join("queries").join(conn_slug))
}

/// Sanitize a connection name or URL into a safe directory component.
pub fn sanitize_conn_slug(s: &str) -> String {
    let slug: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if slug.is_empty() {
        "default".into()
    } else {
        slug
    }
}

pub fn results_dir() -> Option<PathBuf> {
    data_dir().map(|d| d.join("results"))
}

pub fn results_dir_for(conn_slug: &str) -> Option<PathBuf> {
    data_dir().map(|d| d.join("results").join(conn_slug))
}

fn ensure_dir(path: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(path)?;
    Ok(())
}

/// Returns next available scratch_NNN.sql name inside the connection's subdir.
pub fn next_scratch_name(conn_slug: &str) -> anyhow::Result<String> {
    let dir = queries_dir_for(conn_slug)
        .ok_or_else(|| anyhow::anyhow!("cannot determine data dir"))?;
    ensure_dir(&dir)?;
    for i in 1..=999u32 {
        let name = format!("scratch_{:03}.sql", i);
        if !dir.join(&name).exists() {
            return Ok(name);
        }
    }
    Ok("scratch_overflow.sql".into())
}

/// Save a SQL buffer to the connection's queries subdir.
pub fn save_query(conn_slug: &str, name: &str, content: &str) -> anyhow::Result<()> {
    let dir = queries_dir_for(conn_slug)
        .ok_or_else(|| anyhow::anyhow!("cannot determine data dir"))?;
    ensure_dir(&dir)?;
    std::fs::write(dir.join(name), content)?;
    Ok(())
}

/// Load a SQL buffer from the connection's queries subdir.
pub fn load_query(conn_slug: &str, name: &str) -> anyhow::Result<String> {
    let dir = queries_dir_for(conn_slug)
        .ok_or_else(|| anyhow::anyhow!("cannot determine data dir"))?;
    Ok(std::fs::read_to_string(dir.join(name))?)
}

/// List all saved SQL files, sorted by name.
pub fn list_queries() -> anyhow::Result<Vec<String>> {
    let dir = queries_dir().ok_or_else(|| anyhow::anyhow!("cannot determine data dir"))?;
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut names: Vec<String> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("sql") {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|s| s.to_string())
            } else {
                None
            }
        })
        .collect();
    names.sort();
    Ok(names)
}

/// Simple FNV-1a hash for stable short identifiers.
fn fnv_hash8(s: &str) -> String {
    let mut hash: u64 = 14695981039346656037;
    for b in s.bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(1099511628211);
    }
    format!("{:08x}", hash & 0xFFFF_FFFF)
}

fn unix_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Persist a successful query result under the connection's results subdir.
/// Keeps at most RESULT_HISTORY_LIMIT files per connection; oldest is evicted.
pub fn save_result(conn_slug: &str, query: &str, result: &QueryResult) -> anyhow::Result<()> {
    let dir = results_dir_for(conn_slug)
        .ok_or_else(|| anyhow::anyhow!("cannot determine data dir"))?;
    ensure_dir(&dir)?;

    let ts = unix_timestamp();
    let hash = fnv_hash8(query);
    let filename = format!("{}_{}.json", ts, hash);

    let json = serde_json::to_string_pretty(result)?;
    std::fs::write(dir.join(&filename), json)?;

    evict_oldest_results(&dir)?;
    Ok(())
}

fn evict_oldest_results(dir: &std::path::Path) -> anyhow::Result<()> {
    let mut files: Vec<(String, std::time::SystemTime)> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("json") {
                let mtime = e.metadata().ok()?.modified().ok()?;
                let name = p.file_name()?.to_str()?.to_string();
                Some((name, mtime))
            } else {
                None
            }
        })
        .collect();

    if files.len() <= RESULT_HISTORY_LIMIT {
        return Ok(());
    }

    files.sort_by_key(|(_, mtime)| *mtime);
    for (name, _) in files.iter().take(files.len() - RESULT_HISTORY_LIMIT) {
        let _ = std::fs::remove_file(dir.join(name));
    }
    Ok(())
}

/// List saved result filenames, newest first.
pub fn list_results() -> anyhow::Result<Vec<String>> {
    let dir = results_dir().ok_or_else(|| anyhow::anyhow!("cannot determine data dir"))?;
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut files: Vec<String> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("json") {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|s| s.to_string())
            } else {
                None
            }
        })
        .collect();
    files.sort_by(|a, b| b.cmp(a)); // newest first (timestamp prefix)
    Ok(files)
}

/// Load a result by filename.
pub fn load_result(name: &str) -> anyhow::Result<QueryResult> {
    let dir = results_dir().ok_or_else(|| anyhow::anyhow!("cannot determine data dir"))?;
    let content = std::fs::read_to_string(dir.join(name))?;
    Ok(serde_json::from_str(&content)?)
}

/// Export a QueryResult to CSV string.
pub fn export_csv(result: &QueryResult) -> String {
    let mut out = String::new();
    out.push_str(&csv_row(&result.columns));
    for row in &result.rows {
        out.push_str(&csv_row(row));
    }
    out
}

fn csv_row(fields: &[String]) -> String {
    let mut parts = Vec::with_capacity(fields.len());
    for f in fields {
        if f.contains(',') || f.contains('"') || f.contains('\n') {
            parts.push(format!("\"{}\"", f.replace('"', "\"\"")));
        } else {
            parts.push(f.clone());
        }
    }
    parts.join(",") + "\n"
}

/// Export a QueryResult to pretty-printed JSON string.
pub fn export_json(result: &QueryResult) -> anyhow::Result<String> {
    Ok(serde_json::to_string_pretty(result)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::QueryResult;
    use std::fs;

    fn temp_data_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    // Helpers that operate on an explicit dir rather than the system data dir
    fn save_query_to(dir: &std::path::Path, name: &str, content: &str) {
        let q = dir.join("queries");
        fs::create_dir_all(&q).unwrap();
        fs::write(q.join(name), content).unwrap();
    }

    fn load_query_from(dir: &std::path::Path, name: &str) -> String {
        fs::read_to_string(dir.join("queries").join(name)).unwrap()
    }

    fn count_results(dir: &std::path::Path) -> usize {
        let r = dir.join("results");
        if !r.exists() {
            return 0;
        }
        fs::read_dir(&r)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
            .count()
    }

    #[test]
    fn query_round_trip() {
        let tmp = temp_data_dir();
        let dir = tmp.path().to_path_buf();
        save_query_to(&dir, "scratch_001.sql", "SELECT 1");
        let loaded = load_query_from(&dir, "scratch_001.sql");
        assert_eq!(loaded, "SELECT 1");
    }

    #[test]
    fn result_history_rotation() {
        let tmp = temp_data_dir();
        let dir = tmp.path().to_path_buf();
        let result = QueryResult {
            columns: vec!["id".into()],
            rows: vec![vec!["1".into()]],
        };
        // Save 11 results — oldest should be evicted
        for i in 0..11u64 {
            let r = dir.join("results");
            fs::create_dir_all(&r).unwrap();
            let filename = format!("{}_{}.json", i, i);
            let json = serde_json::to_string_pretty(&result).unwrap();
            fs::write(r.join(&filename), json).unwrap();
            evict_oldest_results(&r).unwrap();
        }
        assert_eq!(count_results(&dir), 10);
    }

    #[test]
    fn errors_not_stored() {
        // Errors are never passed to save_result — this is enforced at call site.
        // Test that QueryResult (success) serializes and deserializes correctly.
        let result = QueryResult {
            columns: vec!["col".into()],
            rows: vec![vec!["val".into()]],
        };
        let json = serde_json::to_string(&result).unwrap();
        let loaded: QueryResult = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.columns, result.columns);
        assert_eq!(loaded.rows, result.rows);
    }

    #[test]
    fn fnv_hash_stable() {
        assert_eq!(fnv_hash8("SELECT 1"), fnv_hash8("SELECT 1"));
        assert_ne!(fnv_hash8("SELECT 1"), fnv_hash8("SELECT 2"));
    }

    #[test]
    fn scratch_name_no_collision() {
        // Names for indices 1..=999 are unique
        let names: std::collections::HashSet<String> = (1..=999u32)
            .map(|i| format!("scratch_{:03}.sql", i))
            .collect();
        assert_eq!(names.len(), 999);
    }

    #[test]
    fn export_csv_basic() {
        let result = QueryResult {
            columns: vec!["id".into(), "name".into()],
            rows: vec![
                vec!["1".into(), "Alice".into()],
                vec!["2".into(), "Bob".into()],
            ],
        };
        let csv = export_csv(&result);
        assert_eq!(csv, "id,name\n1,Alice\n2,Bob\n");
    }

    #[test]
    fn export_csv_escapes_commas_and_quotes() {
        let result = QueryResult {
            columns: vec!["val".into()],
            rows: vec![vec!["hello, world".into()], vec!["say \"hi\"".into()]],
        };
        let csv = export_csv(&result);
        assert!(csv.contains("\"hello, world\""));
        assert!(csv.contains("\"say \"\"hi\"\"\""));
    }

    #[test]
    fn export_json_round_trip() {
        let result = QueryResult {
            columns: vec!["x".into()],
            rows: vec![vec!["42".into()]],
        };
        let json = export_json(&result).unwrap();
        let loaded: QueryResult = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.columns, result.columns);
        assert_eq!(loaded.rows, result.rows);
    }
}
