//! Lightweight DDL parser that identifies schema mutations so the
//! schema cache can be invalidated at the right granularity after a
//! query finishes.
//!
//! This is a heuristic over tokens — it is not a full SQL parser. It
//! handles the common shapes (`CREATE TABLE`, `DROP TABLE`,
//! `TRUNCATE [TABLE]`, `RENAME TABLE`, `ALTER TABLE`,
//! `CREATE/DROP DATABASE|SCHEMA`) including optional `IF [NOT] EXISTS`
//! clauses and `db.table` qualifiers (with MySQL-style backticks and
//! standard double-quote identifier quoting).

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DdlEffect {
    /// Database list changed (CREATE/DROP DATABASE/SCHEMA).
    Databases,
    /// Table list in `db` changed (CREATE/DROP/TRUNCATE/RENAME TABLE).
    /// `db == None` means the query was unqualified — invalidate across
    /// every known database.
    Tables { db: Option<String> },
    /// Columns of `table` changed (ALTER TABLE). `db == None` means
    /// unqualified — invalidate every database that has a table by this
    /// name.
    Columns { db: Option<String>, table: String },
}

/// Parse `query` and return the DDL effect, if any. Returns `None` for
/// non-DDL queries (SELECT, INSERT, UPDATE, DELETE, etc.) and for DDL
/// shapes we don't recognise.
pub fn parse_ddl(query: &str) -> Option<DdlEffect> {
    let toks = tokenize(query);
    if toks.is_empty() {
        return None;
    }
    let upper: Vec<String> = toks.iter().map(|t| t.to_ascii_uppercase()).collect();
    let kw = |i: usize| upper.get(i).map(String::as_str);

    match (kw(0), kw(1)) {
        (Some("CREATE"), Some("DATABASE" | "SCHEMA"))
        | (Some("DROP"), Some("DATABASE" | "SCHEMA")) => return Some(DdlEffect::Databases),
        _ => {}
    }

    // CREATE/DROP/TRUNCATE/RENAME TABLE ...
    let tbl_verb_end = match (kw(0), kw(1)) {
        (Some("CREATE" | "DROP" | "RENAME"), Some("TABLE")) => Some(2),
        (Some("TRUNCATE"), Some("TABLE")) => Some(2),
        // `TRUNCATE foo` is valid in some dialects (MySQL/Postgres).
        (Some("TRUNCATE"), Some(_)) => Some(1),
        _ => None,
    };
    if let Some(start) = tbl_verb_end {
        let i = skip_if_exists(&upper, start);
        let (db, _) = parse_qualified(&toks, &upper, i);
        return Some(DdlEffect::Tables { db });
    }

    // ALTER TABLE ...
    if kw(0) == Some("ALTER") && kw(1) == Some("TABLE") {
        let i = skip_if_exists(&upper, 2);
        let (db, table) = parse_qualified(&toks, &upper, i);
        if let Some(table) = table {
            return Some(DdlEffect::Columns { db, table });
        }
    }

    None
}

/// Advance past an optional `IF EXISTS` or `IF NOT EXISTS` clause.
fn skip_if_exists(upper: &[String], start: usize) -> usize {
    let mut i = start;
    if upper.get(i).map(String::as_str) != Some("IF") {
        return i;
    }
    i += 1;
    if upper.get(i).map(String::as_str) == Some("NOT") {
        i += 1;
    }
    if upper.get(i).map(String::as_str) == Some("EXISTS") {
        i += 1;
    }
    i
}

/// Read an optionally-qualified identifier starting at `start`. Returns
/// `(db, table)` where `db` is `Some` only if a `db.table` form was
/// present.
fn parse_qualified(
    toks: &[String],
    upper: &[String],
    start: usize,
) -> (Option<String>, Option<String>) {
    let Some(first) = toks.get(start) else {
        return (None, None);
    };
    if upper.get(start + 1).map(String::as_str) == Some(".")
        && let Some(second) = toks.get(start + 2)
    {
        return (Some(first.clone()), Some(second.clone()));
    }
    (None, Some(first.clone()))
}

/// Tokenize a SQL query into identifier words and bare `.` separators,
/// preserving casing. Strips `--` line comments, `/* */` block comments,
/// and single-quoted string literals. Identifier quoting via backticks or
/// double quotes is unwrapped so the inner name survives.
fn tokenize(query: &str) -> Vec<String> {
    let mut cleaned = String::with_capacity(query.len());
    let mut chars = query.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                // Skip string literal.
                for ch in chars.by_ref() {
                    if ch == '\'' {
                        break;
                    }
                }
            }
            '-' if chars.peek() == Some(&'-') => {
                for ch in chars.by_ref() {
                    if ch == '\n' {
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = ' ';
                for ch in chars.by_ref() {
                    if prev == '*' && ch == '/' {
                        break;
                    }
                    prev = ch;
                }
            }
            '`' | '"' => {
                let close = c;
                for ch in chars.by_ref() {
                    if ch == close {
                        break;
                    }
                    cleaned.push(ch);
                }
                // Preserve a separator so adjacent tokens don't merge.
                cleaned.push(' ');
            }
            _ => cleaned.push(c),
        }
    }

    let mut out = Vec::new();
    let mut cur = String::new();
    for c in cleaned.chars() {
        if c.is_alphanumeric() || c == '_' {
            cur.push(c);
        } else {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            if c == '.' {
                out.push(".".into());
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_none_for_select() {
        assert!(parse_ddl("select * from users").is_none());
    }

    #[test]
    fn returns_none_for_dml() {
        assert!(parse_ddl("insert into users values (1)").is_none());
        assert!(parse_ddl("update users set x = 1").is_none());
        assert!(parse_ddl("delete from users").is_none());
    }

    #[test]
    fn create_database() {
        assert_eq!(parse_ddl("CREATE DATABASE foo"), Some(DdlEffect::Databases));
        assert_eq!(parse_ddl("create schema bar"), Some(DdlEffect::Databases));
    }

    #[test]
    fn drop_database() {
        assert_eq!(parse_ddl("drop database foo"), Some(DdlEffect::Databases));
        assert_eq!(
            parse_ddl("DROP SCHEMA IF EXISTS bar"),
            Some(DdlEffect::Databases)
        );
    }

    #[test]
    fn create_table_unqualified() {
        assert_eq!(
            parse_ddl("CREATE TABLE users (id INT)"),
            Some(DdlEffect::Tables { db: None })
        );
    }

    #[test]
    fn create_table_qualified() {
        assert_eq!(
            parse_ddl("CREATE TABLE mydb.users (id INT)"),
            Some(DdlEffect::Tables {
                db: Some("mydb".into())
            })
        );
    }

    #[test]
    fn create_table_if_not_exists() {
        assert_eq!(
            parse_ddl("create table if not exists mydb.users (id int)"),
            Some(DdlEffect::Tables {
                db: Some("mydb".into())
            })
        );
    }

    #[test]
    fn drop_table_handles_backticks() {
        assert_eq!(
            parse_ddl("DROP TABLE `mydb`.`users`"),
            Some(DdlEffect::Tables {
                db: Some("mydb".into())
            })
        );
    }

    #[test]
    fn drop_table_handles_double_quotes() {
        assert_eq!(
            parse_ddl("drop table \"mydb\".\"users\""),
            Some(DdlEffect::Tables {
                db: Some("mydb".into())
            })
        );
    }

    #[test]
    fn truncate_with_and_without_table_keyword() {
        assert_eq!(
            parse_ddl("truncate table users"),
            Some(DdlEffect::Tables { db: None })
        );
        assert_eq!(
            parse_ddl("truncate users"),
            Some(DdlEffect::Tables { db: None })
        );
    }

    #[test]
    fn rename_table() {
        assert_eq!(
            parse_ddl("RENAME TABLE mydb.old TO mydb.new"),
            Some(DdlEffect::Tables {
                db: Some("mydb".into())
            })
        );
    }

    #[test]
    fn alter_table_unqualified() {
        assert_eq!(
            parse_ddl("ALTER TABLE users ADD COLUMN x INT"),
            Some(DdlEffect::Columns {
                db: None,
                table: "users".into()
            })
        );
    }

    #[test]
    fn alter_table_qualified() {
        assert_eq!(
            parse_ddl("alter table mydb.users drop column x"),
            Some(DdlEffect::Columns {
                db: Some("mydb".into()),
                table: "users".into()
            })
        );
    }

    #[test]
    fn alter_table_if_exists() {
        assert_eq!(
            parse_ddl("ALTER TABLE IF EXISTS users ADD x INT"),
            Some(DdlEffect::Columns {
                db: None,
                table: "users".into()
            })
        );
    }

    #[test]
    fn ignores_leading_comments() {
        assert_eq!(
            parse_ddl("-- a comment\n/* block */ drop database foo"),
            Some(DdlEffect::Databases)
        );
    }

    #[test]
    fn string_literal_with_keyword_is_ignored() {
        // A string literal containing "DROP DATABASE" must not trigger DDL.
        assert!(parse_ddl("select 'drop database hack'").is_none());
    }
}
