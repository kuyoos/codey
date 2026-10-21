//! “运行中”会话探测：上游 `thread/list` 的运行状态只覆盖桌面进程自己执行的会话，
//! 其他客户端（其他 Codey 实例、桌面官方应用等）正在执行的 turn 只有磁盘上能看到。
//!
//! 数据来源是桌面侧的投影库 `thread_history_*.sqlite`：`thread_turns` 里状态为
//! `inProgress` 的线程表示有未完结的 turn；再用该线程最近一次 item 写入时间或
//! rollout 文件更新时间当心跳，排除进程崩溃遗留的僵尸行。turn 正常结束时状态会
//! 立即变为 completed，所以只有崩溃的会话会在心跳窗口内多显示一段时间。
//!
//! 一条会话的 rollout 可能按续接分成多个文件，文件名形如
//! `rollout-<时间>-<根会话 id>_<分片 id>.jsonl`，turn 记录挂在分片 id 下；
//! 因此把同一文件名里的各个 id 关联起来，任一分段存活即整条会话视为运行中。

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags};

/// 心跳窗口：最近一次写入早于这个时长的 `inProgress` 行按崩溃残留处理。
const STALE_TTL: Duration = Duration::from_secs(300);

/// 磁盘上正在运行的会话 id 集合（含续接分片与其根会话）。
pub(super) fn active_thread_ids(home: &Path) -> HashSet<String> {
    // 空闲时没有候选，直接跳过目录扫描。
    let candidates = in_progress_threads(home);
    if candidates.is_empty() {
        return HashSet::new();
    }
    let now = SystemTime::now();
    let rollouts = rollout_files(home);
    let heartbeats = heartbeat_map(&rollouts);
    let mut active = HashSet::new();
    for (database, thread_id) in candidates {
        let item_fresh = thread_items_fresh(&database, &thread_id, now);
        let file_fresh = heartbeats
            .get(thread_id.as_str())
            .is_some_and(|mtime| within_ttl(*mtime, now));
        if item_fresh || file_fresh {
            active.insert(thread_id);
        }
    }
    // 续接分片与根会话共享同一批 rollout 文件：任一 id 存活则两边都算运行中。
    for (ids, mtime) in &rollouts {
        if within_ttl(*mtime, now) && ids.iter().any(|id| active.contains(id)) {
            active.extend(ids.iter().cloned());
        }
    }
    active
}

/// 投影库里所有未完结的 turn 所属线程；库缺失或表缺失时跳过。
fn in_progress_threads(home: &Path) -> Vec<(PathBuf, String)> {
    let mut threads = Vec::new();
    for path in history_db_paths(home) {
        let Some(connection) = open_readonly(&path) else {
            continue;
        };
        let Ok(mut statement) = connection
            .prepare("SELECT DISTINCT thread_id FROM thread_turns WHERE status = 'inProgress'")
        else {
            continue;
        };
        let Ok(rows) = statement.query_map([], |row| row.get::<_, String>(0)) else {
            continue;
        };
        for thread_id in rows.flatten() {
            threads.push((path.clone(), thread_id));
        }
    }
    threads
}

/// 线程最近一次 item 写入是否还在心跳窗口内；读不到按不新鲜处理。
fn thread_items_fresh(database: &Path, thread_id: &str, now: SystemTime) -> bool {
    let Some(connection) = open_readonly(database) else {
        return false;
    };
    connection
        .query_row(
            "SELECT MAX(created_at_ms) FROM thread_items WHERE thread_id = ?",
            [thread_id],
            |row| row.get::<_, Option<i64>>(0),
        )
        .ok()
        .flatten()
        .is_some_and(|milliseconds| within_ttl_ms(milliseconds, now))
}

/// 桌面投影库位于 home 根目录，命名 `thread_history_<版本>.sqlite`；版本号未知时全部读取。
fn history_db_paths(home: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(home) else {
        return Vec::new();
    };
    let mut paths = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("thread_history") && name.ends_with(".sqlite"))
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

fn open_readonly(path: &Path) -> Option<Connection> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    // 桌面端可能正在写库，让一步：读不到就放弃本轮。
    let _ = connection.busy_timeout(Duration::from_millis(250));
    Some(connection)
}

/// 扫描桌面会话目录，收集每个 rollout 文件里的会话 id 与更新时间。
fn rollout_files(home: &Path) -> Vec<(Vec<String>, SystemTime)> {
    let mut files = Vec::new();
    collect_rollouts(&home.join("sessions"), &mut files);
    files
}

fn collect_rollouts(directory: &Path, files: &mut Vec<(Vec<String>, SystemTime)>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_rollouts(&entry.path(), files);
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let ids = parse_rollout_ids(&name);
        if ids.is_empty() {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(mtime) = metadata.modified() else {
            continue;
        };
        files.push((ids, mtime));
    }
}

/// 每个 id 对应它所在 rollout 文件的最新更新时间（同一会话可能有多个分片文件）。
fn heartbeat_map(rollouts: &[(Vec<String>, SystemTime)]) -> HashMap<&str, SystemTime> {
    let mut map = HashMap::new();
    for (ids, mtime) in rollouts {
        for id in ids {
            map.entry(id.as_str())
                .and_modify(|existing: &mut SystemTime| *existing = (*existing).max(*mtime))
                .or_insert(*mtime);
        }
    }
    map
}

/// 从 `rollout-…jsonl` 文件名解析会话 id（根会话与续接分片）；其他文件返回空。
fn parse_rollout_ids(file_name: &str) -> Vec<String> {
    let Some(rest) = file_name
        .strip_prefix("rollout-")
        .and_then(|rest| rest.strip_suffix(".jsonl"))
    else {
        return Vec::new();
    };
    let bytes = rest.as_bytes();
    let mut ids = Vec::new();
    let mut index = 0;
    while index + UUID_LEN <= bytes.len() {
        if is_uuid_at(bytes, index) {
            ids.push(rest[index..index + UUID_LEN].to_owned());
            index += UUID_LEN;
        } else {
            index += 1;
        }
    }
    ids
}

const UUID_LEN: usize = 36;

/// 标准 UUID 形状：8-4-4-4-12 的十六进制、连字符位置固定；日期时间片段不满足该形状。
fn is_uuid_at(bytes: &[u8], start: usize) -> bool {
    (0..UUID_LEN).all(|offset| {
        let byte = bytes[start + offset];
        if matches!(offset, 8 | 13 | 18 | 23) {
            byte == b'-'
        } else {
            byte.is_ascii_hexdigit()
        }
    })
}

fn within_ttl(mtime: SystemTime, now: SystemTime) -> bool {
    // 时间戳在未来（时钟偏差）时保守视为仍在更新。
    now.duration_since(mtime)
        .map(|age| age <= STALE_TTL)
        .unwrap_or(true)
}

fn within_ttl_ms(milliseconds: i64, now: SystemTime) -> bool {
    let now_ms = now
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default();
    now_ms - milliseconds <= STALE_TTL.as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollout_ids_parse_root_and_segment() {
        let ids = parse_rollout_ids(
            "rollout-2026-10-05T09-27-08-01a10721-2cf5-7271-b22f-544b877b192e_\
             01a109ab-fa6d-7ad1-ad34-0afa27f448e6.jsonl",
        );
        assert_eq!(
            ids,
            vec![
                "01a10721-2cf5-7271-b22f-544b877b192e".to_owned(),
                "01a109ab-fa6d-7ad1-ad34-0afa27f448e6".to_owned(),
            ]
        );
    }

    #[test]
    fn rollout_ids_parse_single_session_and_ignore_other_files() {
        assert_eq!(
            parse_rollout_ids(
                "rollout-2026-10-04T21-49-55-01a1072d-a835-7722-9100-fbf1d24083af.jsonl"
            ),
            vec!["01a1072d-a835-7722-9100-fbf1d24083af".to_owned()]
        );
        assert!(parse_rollout_ids("state_5.sqlite").is_empty());
        assert!(parse_rollout_ids("rollout-2026-10-04T21-49-55.jsonl").is_empty());
        assert!(parse_rollout_ids("rollout-broken.jsonl").is_empty());
    }

    #[test]
    fn activity_window_prefers_recent_writes() {
        let now = UNIX_EPOCH + Duration::from_millis(10_000_000);
        assert!(within_ttl(now - Duration::from_secs(1), now));
        assert!(!within_ttl(now - STALE_TTL - Duration::from_secs(1), now));
        // 时间戳在未来（时钟偏差）时保守视为仍在更新。
        assert!(within_ttl(now + Duration::from_secs(60), now));
        assert!(within_ttl_ms(10_000_000 - 1000, now));
        assert!(!within_ttl_ms(
            10_000_000 - STALE_TTL.as_millis() as i64 - 1,
            now
        ));
    }

    #[test]
    fn active_ids_keep_fresh_in_progress_only() {
        let home = std::env::temp_dir().join(format!("codey-activity-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("临时目录");
        let database = home.join("thread_history_1.sqlite");
        {
            let connection = Connection::open(&database).expect("建库");
            connection
                .execute_batch(
                    "CREATE TABLE thread_turns (thread_id TEXT, status TEXT);\
                     CREATE TABLE thread_items (thread_id TEXT, created_at_ms INTEGER);",
                )
                .expect("建表");
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("系统时间")
                .as_millis() as i64;
            connection
                .execute(
                    "INSERT INTO thread_turns VALUES ('fresh', 'inProgress')",
                    [],
                )
                .expect("插入");
            connection
                .execute(
                    "INSERT INTO thread_items VALUES ('fresh', ?1)",
                    [now_ms - 1000],
                )
                .expect("插入");
            connection
                .execute(
                    "INSERT INTO thread_turns VALUES ('stale', 'inProgress')",
                    [],
                )
                .expect("插入");
            connection
                .execute(
                    "INSERT INTO thread_items VALUES ('stale', ?1)",
                    [now_ms - 3_600_000],
                )
                .expect("插入");
            // 已完结的 turn 即使写入很新也不算运行中。
            connection
                .execute("INSERT INTO thread_turns VALUES ('done', 'completed')", [])
                .expect("插入");
            connection
                .execute("INSERT INTO thread_items VALUES ('done', ?1)", [now_ms])
                .expect("插入");
        }
        let active = active_thread_ids(&home);
        assert!(active.contains("fresh"), "{active:?}");
        assert!(!active.contains("stale"));
        assert!(!active.contains("done"));
        let _ = std::fs::remove_dir_all(&home);
    }
}
