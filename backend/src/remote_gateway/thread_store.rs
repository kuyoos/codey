//! 首页会话列表的数据来源：桌面端的 state 库。
//!
//! 上游 `thread/list` 只返回桌面进程当下正在使用的一小部分会话，刚完成的会话也可能不在
//! 其中，单靠它首页会几乎空白。桌面侧边栏展示的是 `state_5.sqlite` 的 `threads` 表，
//! 这里用同一来源补齐列表：只读打开、按库中实际存在的列兼容查询，运行状态等实时字段仍
//! 由上游列表补充。

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OpenFlags, params_from_iter};

use crate::sqlite_util::table_columns;

/// 首页一次最多返回多少条会话；三天窗口内远小于这个量，这只是防止库异常时整库塞进网页。
pub(super) const HOME_LIMIT: usize = 80;
/// 搜索要覆盖很久以前的会话，但同样限制返回量。
pub(super) const SEARCH_LIMIT: usize = 40;

/// 会话列表查询条件；`cutoff` 为 `None` 表示不按时间裁剪（搜索与首页兜底）。
pub(super) struct Query {
    pub(super) search: Option<String>,
    pub(super) cutoff: Option<i64>,
    pub(super) limit: usize,
}

/// 从 state 库读到的一行会话；时间统一为秒。
pub(super) struct ThreadRow {
    pub(super) id: String,
    pub(super) name: Option<String>,
    pub(super) preview: Option<String>,
    pub(super) cwd: Option<String>,
    pub(super) model: Option<String>,
    pub(super) created_at: Option<i64>,
    pub(super) updated_at: i64,
}

/// 按条件读会话，按最后活动时间倒序；多个库同时存在时按 id 去重，保留时间更新的一行。
pub(super) fn home_threads(home: &Path, query: &Query) -> Vec<ThreadRow> {
    let mut merged: HashMap<String, ThreadRow> = HashMap::new();
    for path in codey_runtime_core::codex_sqlite::codex_session_db_paths_from_home(home) {
        let Some(rows) = read_database(&path, query) else {
            continue;
        };
        for row in rows {
            if let Some(existing) = merged.get(&row.id)
                && existing.updated_at >= row.updated_at
            {
                continue;
            }
            merged.insert(row.id.clone(), row);
        }
    }
    let mut rows = merged.into_values().collect::<Vec<_>>();
    rows.sort_by_key(|row| std::cmp::Reverse(row.updated_at));
    rows.truncate(query.limit);
    rows
}

fn read_database(path: &Path, query: &Query) -> Option<Vec<ThreadRow>> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    // 桌面端可能正在写库，让一步：读不到就当作这个库暂不可用。
    let _ = connection.busy_timeout(Duration::from_millis(250));
    let columns = table_columns(&connection, "threads").ok()?;
    if !columns.contains("id") {
        return Some(Vec::new());
    }
    let activity = activity_expression(&columns)?;
    let name = column_or_null(&columns, "name");
    let title = column_or_null(&columns, "title");
    let preview = column_or_null(&columns, "preview");
    let cwd = column_or_null(&columns, "cwd");
    let model = column_or_null(&columns, "model");
    let created = if columns.contains("created_at") {
        "NULLIF(CAST(created_at AS INTEGER), 0)"
    } else {
        "NULL"
    };

    let mut conditions = vec![format!("({activity}) > 0")];
    let mut bindings: Vec<SqlValue> = Vec::new();
    if columns.contains("archived") {
        conditions.push("COALESCE(archived, 0) = 0".to_owned());
    }
    if columns.contains("thread_source") {
        conditions
            .push("COALESCE(thread_source, '') NOT IN ('subagent', 'guardian_review')".to_owned());
    }
    if columns.contains("source") {
        conditions.push("COALESCE(source, '') NOT LIKE '{\"subagent\"%'".to_owned());
    }
    if let Some(cutoff) = query.cutoff {
        conditions.push(format!("({activity}) >= ?"));
        bindings.push(SqlValue::Integer(cutoff));
    }
    if let Some(search) = query.search.as_deref() {
        let mut matchers = Vec::new();
        if columns.contains("name") {
            matchers.push("COALESCE(name, '') LIKE ? ESCAPE '\\'");
        }
        if columns.contains("title") {
            matchers.push("COALESCE(title, '') LIKE ? ESCAPE '\\'");
        }
        if matchers.is_empty() {
            return Some(Vec::new());
        }
        conditions.push(format!("({})", matchers.join(" OR ")));
        let pattern = SqlValue::Text(format!("%{}%", escape_like(search)));
        for _ in &matchers {
            bindings.push(pattern.clone());
        }
    }
    bindings.push(SqlValue::Integer(query.limit as i64));

    let sql = format!(
        "SELECT id, {name}, {title}, {preview}, {cwd}, {model}, {created}, ({activity}) AS activity \
         FROM threads WHERE {} ORDER BY activity DESC LIMIT ?",
        conditions.join(" AND ")
    );
    let mut statement = connection.prepare(&sql).ok()?;
    let rows = statement
        .query_map(params_from_iter(bindings), |row| {
            let name = row.get::<_, Option<String>>(1)?;
            let title = row.get::<_, Option<String>>(2)?;
            let cwd = row.get::<_, Option<String>>(4)?;
            Ok(ThreadRow {
                id: row.get(0)?,
                name: non_empty(name).or_else(|| non_empty(title)),
                preview: non_empty(row.get::<_, Option<String>>(3)?),
                cwd: non_empty(cwd).map(|cwd| normalize_cwd(&cwd)),
                model: non_empty(row.get::<_, Option<String>>(5)?),
                created_at: row.get::<_, Option<i64>>(6)?,
                updated_at: row.get::<_, i64>(7)?.max(0),
            })
        })
        .ok()?;
    rows.collect::<rusqlite::Result<Vec<_>>>().ok()
}

/// 会话活动时间的表达式（秒）：新版列优先，缺列或值为 0 时退回更旧的列。
fn activity_expression(columns: &HashSet<String>) -> Option<String> {
    let mut parts = Vec::new();
    for (column, expression) in [
        ("updated_at", "NULLIF(CAST(updated_at AS INTEGER), 0)"),
        (
            "updated_at_ms",
            "NULLIF(CAST(updated_at_ms AS INTEGER) / 1000, 0)",
        ),
        ("recency_at", "NULLIF(CAST(recency_at AS INTEGER), 0)"),
        (
            "recency_at_ms",
            "NULLIF(CAST(recency_at_ms AS INTEGER) / 1000, 0)",
        ),
        ("created_at", "NULLIF(CAST(created_at AS INTEGER), 0)"),
        (
            "created_at_ms",
            "NULLIF(CAST(created_at_ms AS INTEGER) / 1000, 0)",
        ),
    ] {
        if columns.contains(column) {
            parts.push(expression);
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(format!("COALESCE({})", parts.join(", ")))
}

fn column_or_null<'a>(columns: &HashSet<String>, column: &'a str) -> &'a str {
    if columns.contains(column) {
        column
    } else {
        "NULL"
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|text| !text.trim().is_empty())
}

/// 搜索词按字面匹配：用户输入里的 LIKE 通配符先转义，避免 `%` 放宽成任意内容。
fn escape_like(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(character, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

/// Windows 的 verbatim 前缀（`\\?\C:\...`）只影响展示与分组，读出来就去掉；
/// UNC 形式（`\\?\UNC\...`）没有盘符，保持原样交给前端。
pub(super) fn normalize_cwd(cwd: &str) -> String {
    let trimmed = cwd.trim();
    let Some(rest) = trimmed.strip_prefix(r"\\?\") else {
        return trimmed.to_owned();
    };
    if rest.len() >= 3 && rest.as_bytes().get(1) == Some(&b':') {
        rest.to_owned()
    } else {
        trimmed.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use tempfile::tempdir;

    fn seed_full_schema(path: &Path) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads (
                    id TEXT PRIMARY KEY, name TEXT, title TEXT, preview TEXT, cwd TEXT,
                    model TEXT, created_at INTEGER, updated_at INTEGER, archived INTEGER,
                    thread_source TEXT, source TEXT
                )",
            )
            .unwrap();
        for (id, name, cwd, created, updated, archived, thread_source) in [
            (
                "recent",
                Some("最近会话"),
                r"\\?\D:\proj\app",
                100,
                2_000,
                0,
                "user",
            ),
            ("stale", Some("旧会话"), r"D:\proj\old", 100, 500, 0, "user"),
            (
                "subagent",
                Some("子代理"),
                r"D:\proj\app",
                100,
                1_900,
                0,
                "subagent",
            ),
            (
                "archived",
                Some("归档会话"),
                r"D:\proj\app",
                100,
                1_950,
                1,
                "user",
            ),
            (
                "guardian",
                Some("守护会话"),
                r"D:\proj\app",
                100,
                1_960,
                0,
                "guardian_review",
            ),
            (
                "percent",
                Some("50% 会话"),
                r"D:\proj\app",
                100,
                1_700,
                0,
                "user",
            ),
        ] {
            connection
                .execute(
                    "INSERT INTO threads (id, name, preview, cwd, created_at, updated_at, archived, thread_source, source) \
                     VALUES (?1, ?2, '', ?3, ?4, ?5, ?6, ?7, 'vscode')",
                    params![id, name, cwd, created, updated, archived, thread_source],
                )
                .unwrap();
        }
    }

    #[test]
    fn home_list_returns_recent_user_threads_in_order() {
        let home = tempdir().unwrap();
        seed_full_schema(&home.path().join("state_5.sqlite"));

        let rows = home_threads(
            home.path(),
            &Query {
                search: None,
                cutoff: Some(1_000),
                limit: HOME_LIMIT,
            },
        );
        let ids = rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>();
        // 归档、子代理与守护会话都被过滤，只留三天里的用户会话，并按时间倒序。
        assert_eq!(ids, vec!["recent", "percent"]);
        assert_eq!(rows[0].updated_at, 2_000);
        // verbatim 前缀只影响展示，读出来就去掉。
        assert_eq!(rows[0].cwd.as_deref(), Some(r"D:\proj\app"));

        // 不带窗口（首页兜底）时更早的会话也能返回。
        let rows = home_threads(
            home.path(),
            &Query {
                search: None,
                cutoff: None,
                limit: HOME_LIMIT,
            },
        );
        let ids = rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>();
        assert_eq!(ids, vec!["recent", "percent", "stale"]);

        // limit 生效，同样是最近的几条。
        let rows = home_threads(
            home.path(),
            &Query {
                search: None,
                cutoff: None,
                limit: 1,
            },
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "recent");
    }

    #[test]
    fn search_matches_name_and_title_without_wildcards() {
        let home = tempdir().unwrap();
        let path = home.path().join("state_5.sqlite");
        seed_full_schema(&path);

        let query = |term: &str| Query {
            search: Some(term.to_owned()),
            cutoff: None,
            limit: SEARCH_LIMIT,
        };
        let rows = home_threads(home.path(), &query("旧"));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "stale");

        // title 列也要参与匹配（老版本只有 title 没有 name）。
        let connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "UPDATE threads SET name='', title='标题含关键词' WHERE id='stale'",
                [],
            )
            .unwrap();
        let rows = home_threads(home.path(), &query("关键词"));
        assert_eq!(rows[0].id, "stale");

        // 搜索词里的 LIKE 通配符按字面匹配：`50%` 不能放宽成任意内容。
        let rows = home_threads(home.path(), &query("50%"));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "percent");
    }

    #[test]
    fn legacy_schema_without_optional_columns_still_lists_threads() {
        let home = tempdir().unwrap();
        let connection = Connection::open(home.path().join("state_5.sqlite")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, title TEXT, cwd TEXT, created_at INTEGER, updated_at INTEGER);
                 INSERT INTO threads VALUES ('legacy', '旧版会话', 'D:\\proj', 10, 900);",
            )
            .unwrap();

        let rows = home_threads(
            home.path(),
            &Query {
                search: None,
                cutoff: Some(500),
                limit: HOME_LIMIT,
            },
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "legacy");
        assert_eq!(rows[0].name.as_deref(), Some("旧版会话"));
        assert_eq!(rows[0].created_at, Some(10));
        assert_eq!(rows[0].updated_at, 900);
    }

    #[test]
    fn normalizes_windows_verbatim_paths() {
        assert_eq!(normalize_cwd(r"\\?\D:\Desktop\codey"), r"D:\Desktop\codey");
        assert_eq!(normalize_cwd(r"D:\Desktop\codey"), r"D:\Desktop\codey");
        assert_eq!(normalize_cwd(r"\\?\UNC\srv\share"), r"\\?\UNC\srv\share");
        assert_eq!(normalize_cwd("  "), "");
    }
}
