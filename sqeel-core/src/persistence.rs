use crate::state::QueryResult;
use std::path::PathBuf;

/// Process-wide override for the data dir, set by `--sandbox` so
/// dev-mode runs don't touch the user's real
/// `~/.local/share/sqeel/`. `None` (the default) falls back to
/// `dirs::data_dir()`.
static DATA_DIR_OVERRIDE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Install a sandbox data dir. Idempotent — first call wins.
pub fn set_data_dir_override(path: PathBuf) {
    let _ = DATA_DIR_OVERRIDE.set(path);
}

pub fn data_dir() -> Option<PathBuf> {
    if let Some(p) = DATA_DIR_OVERRIDE.get() {
        return Some(p.clone());
    }
    dirs::data_dir().map(|d| d.join("sqeel"))
}

/// Scratch queries are connection-agnostic — `queries/` lives directly
/// under the data dir so a buffer the user opened against one
/// connection can be re-run against any other. Results stay
/// per-connection (see [`results_dir_for`]) since they're tied to a
/// concrete query execution.
pub fn queries_dir() -> Option<PathBuf> {
    data_dir().map(|d| d.join("queries"))
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

/// Returns next available `scratch_NNN.sql` name in the global
/// queries dir. Scratch buffers are connection-agnostic.
pub fn next_scratch_name() -> anyhow::Result<String> {
    let dir = queries_dir().ok_or_else(|| anyhow::anyhow!("cannot determine data dir"))?;
    ensure_dir(&dir)?;
    for i in 1..=999u32 {
        let name = format!("scratch_{:03}.sql", i);
        if !dir.join(&name).exists() {
            return Ok(name);
        }
    }
    Ok("scratch_overflow.sql".into())
}

/// Save a SQL buffer to the queries dir.
pub fn save_query(name: &str, content: &str) -> anyhow::Result<()> {
    let dir = queries_dir().ok_or_else(|| anyhow::anyhow!("cannot determine data dir"))?;
    ensure_dir(&dir)?;
    std::fs::write(dir.join(name), content)?;
    Ok(())
}

/// Delete a saved SQL buffer. No-op if the file doesn't exist.
pub fn delete_query(name: &str) -> anyhow::Result<()> {
    let dir = queries_dir().ok_or_else(|| anyhow::anyhow!("cannot determine data dir"))?;
    let path = dir.join(name);
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

/// Rename a saved SQL buffer. Fails if the destination already exists.
pub fn rename_query(old: &str, new: &str) -> anyhow::Result<()> {
    let dir = queries_dir().ok_or_else(|| anyhow::anyhow!("cannot determine data dir"))?;
    let from = dir.join(old);
    let to = dir.join(new);
    if to.exists() {
        anyhow::bail!("A buffer named {new} already exists");
    }
    std::fs::rename(from, to)?;
    Ok(())
}

/// Load a SQL buffer from the queries dir.
pub fn load_query(name: &str) -> anyhow::Result<String> {
    let dir = queries_dir().ok_or_else(|| anyhow::anyhow!("cannot determine data dir"))?;
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
/// Returns the filename the result was saved as.
pub fn save_result(conn_slug: &str, query: &str, result: &QueryResult) -> anyhow::Result<String> {
    let dir =
        results_dir_for(conn_slug).ok_or_else(|| anyhow::anyhow!("cannot determine data dir"))?;
    ensure_dir(&dir)?;

    let ts = unix_timestamp();
    let hash = fnv_hash8(query);
    let filename = format!("{}_{}.json", ts, hash);

    let json = serde_json::to_string_pretty(result)?;
    std::fs::write(dir.join(&filename), json)?;

    evict_old_results_dir(&dir);
    Ok(filename)
}

/// Load a saved result by filename from a specific connection's results subdir.
pub fn load_result_for(conn_slug: &str, name: &str) -> anyhow::Result<QueryResult> {
    let dir =
        results_dir_for(conn_slug).ok_or_else(|| anyhow::anyhow!("cannot determine data dir"))?;
    let content = std::fs::read_to_string(dir.join(name))?;
    let mut result: QueryResult = serde_json::from_str(&content)?;
    result.compute_col_widths();
    Ok(result)
}

const RESULT_MAX_AGE_SECS: u64 = 30 * 24 * 60 * 60; // 30 days

/// Delete result files older than 30 days for the given connection.
pub fn evict_old_results(conn_slug: &str) {
    let Some(dir) = results_dir_for(conn_slug) else {
        return;
    };
    evict_old_results_dir(&dir);
}

fn evict_old_results_dir(dir: &std::path::Path) {
    let cutoff = unix_timestamp().saturating_sub(RESULT_MAX_AGE_SECS);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !name_str.ends_with(".json") {
            continue;
        }
        if let Some(ts_str) = name_str.split('_').next()
            && let Ok(ts) = ts_str.parse::<u64>()
            && ts < cutoff
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
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
    let mut result: QueryResult = serde_json::from_str(&content)?;
    result.compute_col_widths();
    Ok(result)
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
    fn results_evict_older_than_30_days() {
        let tmp = temp_data_dir();
        let dir = tmp.path().to_path_buf();
        let result = QueryResult {
            columns: vec!["id".into()],
            rows: vec![vec!["1".into()]],
            col_widths: vec![],
        };
        let r = dir.join("results");
        fs::create_dir_all(&r).unwrap();

        let now = unix_timestamp();
        let thirty_one_days_ago = now - 31 * 24 * 60 * 60;
        let yesterday = now - 24 * 60 * 60;

        // Write 5 old files (should be evicted)
        for i in 0..5u64 {
            let filename = format!("{}_{}.json", thirty_one_days_ago + i, i);
            let json = serde_json::to_string_pretty(&result).unwrap();
            fs::write(r.join(&filename), json).unwrap();
        }
        // Write 3 recent files (should survive)
        for i in 0..3u64 {
            let filename = format!("{}_{}.json", yesterday + i, i + 100);
            let json = serde_json::to_string_pretty(&result).unwrap();
            fs::write(r.join(&filename), json).unwrap();
        }

        evict_old_results_dir(&r);
        assert_eq!(count_results(&dir), 3);
    }

    #[test]
    fn errors_not_stored() {
        // Errors are never passed to save_result — this is enforced at call site.
        // Test that QueryResult (success) serializes and deserializes correctly.
        let result = QueryResult {
            columns: vec!["col".into()],
            rows: vec![vec!["val".into()]],
            col_widths: vec![],
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
            col_widths: vec![],
        };
        let csv = export_csv(&result);
        assert_eq!(csv, "id,name\n1,Alice\n2,Bob\n");
    }

    #[test]
    fn export_csv_escapes_commas_and_quotes() {
        let result = QueryResult {
            columns: vec!["val".into()],
            rows: vec![vec!["hello, world".into()], vec!["say \"hi\"".into()]],
            col_widths: vec![],
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
            col_widths: vec![],
        };
        let json = export_json(&result).unwrap();
        let loaded: QueryResult = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.columns, result.columns);
        assert_eq!(loaded.rows, result.rows);
    }
}
