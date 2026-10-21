use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use codey_runtime_core::codex_sqlite::CodexSessionDbDiscoveryCache;
use rusqlite::{Connection, OpenFlags, params};
use serde::Serialize;

mod parser;

use parser::RolloutParseState;

const RECENT_SESSION_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);
pub(crate) const MAX_RECENT_SESSIONS: usize = 64;
pub(crate) const MAX_CACHED_TERMINAL_TURNS_PER_ROLLOUT: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApproval {
    pub session_id: String,
    pub turn_id: String,
    pub waiting_id: String,
    pub duration_ms: u128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartedTurn {
    pub session_id: String,
    pub turn_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbortedTurn {
    pub session_id: String,
    pub turn_id: String,
    pub is_snapshot_replay: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnConfiguration {
    pub model: String,
    pub reasoning_effort: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedTurn {
    pub session_id: String,
    pub turn_id: String,
    pub duration_ms: u128,
    pub completed_at: Option<i64>,
    pub error: Option<String>,
    pub is_snapshot_replay: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionLifecycleStatus {
    #[default]
    Idle,
    Running,
    Error,
    Waiting,
}

#[derive(Debug, Clone, Default)]
pub struct RecentSessionEvents {
    pub pending_approvals: Vec<PendingApproval>,
    pub started_turns: Arc<Vec<StartedTurn>>,
    pub aborted_turns: Arc<Vec<AbortedTurn>>,
    pub completed_turns: Arc<Vec<CompletedTurn>>,
    pub session_statuses: Arc<HashMap<String, SessionLifecycleStatus>>,
    pub turn_configurations: Arc<HashMap<String, HashMap<String, TurnConfiguration>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RolloutSignature {
    len: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl RolloutSignature {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;

        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
        }
    }
}

const ROLLOUT_FINGERPRINT_LEN: usize = 64;

#[derive(Debug, Clone)]
struct RolloutFingerprint {
    bytes: [u8; ROLLOUT_FINGERPRINT_LEN],
    len: usize,
}

impl Default for RolloutFingerprint {
    fn default() -> Self {
        Self {
            bytes: [0; ROLLOUT_FINGERPRINT_LEN],
            len: 0,
        }
    }
}

impl RolloutFingerprint {
    fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn len(&self) -> usize {
        self.len
    }

    fn set_prefix(&mut self, source: &[u8]) {
        self.len = source.len().min(ROLLOUT_FINGERPRINT_LEN);
        self.bytes[..self.len].copy_from_slice(&source[..self.len]);
    }

    fn push_tail(&mut self, source: &[u8]) {
        if source.len() >= ROLLOUT_FINGERPRINT_LEN {
            self.bytes
                .copy_from_slice(&source[source.len() - ROLLOUT_FINGERPRINT_LEN..]);
            self.len = ROLLOUT_FINGERPRINT_LEN;
            return;
        }
        if self.len + source.len() <= ROLLOUT_FINGERPRINT_LEN {
            let next_len = self.len + source.len();
            self.bytes[self.len..next_len].copy_from_slice(source);
            self.len = next_len;
            return;
        }

        let retained = ROLLOUT_FINGERPRINT_LEN - source.len();
        self.bytes.copy_within(self.len - retained..self.len, 0);
        self.bytes[retained..].copy_from_slice(source);
        self.len = ROLLOUT_FINGERPRINT_LEN;
    }
}

/// File position and rewrite detection are kept separate from event parsing.
/// A growing rollout can resume only when its consumed prefix is intact.
#[derive(Debug, Clone, Default)]
struct RolloutReadState {
    consumed_bytes: u64,
    /// Leading bytes of the file. Provider normalisation rewrites the
    /// `session_meta` header in place, which leaves the tail untouched.
    head_fingerprint: RolloutFingerprint,
    /// Trailing bytes of the region already consumed. Re-reading just these
    /// detects an in-place rewrite (Codey itself rewrites rollouts when
    /// deleting turns or normalising providers), which must force a full
    /// re-parse instead of appending to stale state.
    tail_fingerprint: RolloutFingerprint,
    parser: RolloutParseState,
}

impl RolloutReadState {
    fn advance(&mut self, chunk: &str, consumed: usize) {
        if consumed == 0 {
            return;
        }
        let taken = &chunk.as_bytes()[..consumed];
        if self.consumed_bytes == 0 {
            self.head_fingerprint.set_prefix(taken);
        }
        self.consumed_bytes += consumed as u64;
        self.tail_fingerprint.push_tail(taken);
    }
}

#[derive(Debug, Clone)]
struct CachedRolloutEvents {
    session_id: String,
    signature: RolloutSignature,
    state: RolloutReadState,
}

#[derive(Debug)]
struct CachedDatabaseConnection {
    signature: RolloutSignature,
    connection: Connection,
}

#[derive(Debug, Default)]
pub struct RecentSessionEventCache {
    rollouts: HashMap<PathBuf, CachedRolloutEvents>,
    database_discovery: CodexSessionDbDiscoveryCache,
    database_connections: HashMap<PathBuf, CachedDatabaseConnection>,
    last_snapshot: Option<Arc<RecentSessionEvents>>,
    #[cfg(test)]
    parse_count: usize,
    #[cfg(test)]
    incremental_parse_count: usize,
    #[cfg(test)]
    database_open_count: usize,
}

impl RecentSessionEventCache {
    /// Finds recent session lifecycle events from rollout data. Unchanged
    /// rollouts reuse their compact parsed event set across polling cycles.
    pub fn refresh(&mut self, home: &Path) -> Arc<RecentSessionEvents> {
        let recent_after = SystemTime::now()
            .checked_sub(RECENT_SESSION_WINDOW)
            .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or_default();
        let rollouts = self.recent_codey_rollouts(home, recent_after);
        self.refresh_rollouts(rollouts)
    }

    pub(crate) fn release_database_connections(&mut self) {
        self.database_connections.clear();
    }

    fn recent_codey_rollouts(&mut self, home: &Path, recent_after: i64) -> Vec<(String, PathBuf)> {
        let database_paths = self.session_database_paths(home);
        let mut rollouts = Vec::new();
        for database_path in database_paths {
            if !self.ensure_database_connection(&database_path) {
                continue;
            }
            let query_result = self
                .database_connections
                .get(&database_path)
                .map(|cached| query_recent_codey_rollouts(&cached.connection, home, recent_after));
            match query_result {
                Some(Ok(rows)) => rollouts.extend(rows),
                Some(Err(_)) => {
                    // A replaced database or schema migration can leave a cached
                    // read handle stale. Reopen it on the next polling cycle.
                    self.database_connections.remove(&database_path);
                }
                None => {}
            }
        }
        rollouts.sort();
        rollouts.dedup();
        rollouts
    }

    fn session_database_paths(&mut self, home: &Path) -> Vec<PathBuf> {
        let database_paths = self.database_discovery.session_db_paths_from_home(home);
        let active_database_paths = database_paths.iter().cloned().collect::<HashSet<_>>();
        self.database_connections
            .retain(|path, _| active_database_paths.contains(path));
        database_paths
    }

    fn ensure_database_connection(&mut self, database_path: &Path) -> bool {
        let Ok(metadata) = fs::metadata(database_path) else {
            self.database_connections.remove(database_path);
            return false;
        };
        let signature = RolloutSignature::from_metadata(&metadata);
        let connection_is_current = self
            .database_connections
            .get(database_path)
            .is_some_and(|cached| cached.signature == signature);
        if !connection_is_current {
            self.database_connections.remove(database_path);
        }
        if !self.database_connections.contains_key(database_path) {
            let Ok(connection) = Connection::open_with_flags(
                database_path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            ) else {
                return false;
            };
            #[cfg(test)]
            {
                self.database_open_count += 1;
            }
            self.database_connections.insert(
                database_path.to_path_buf(),
                CachedDatabaseConnection {
                    signature,
                    connection,
                },
            );
        }
        true
    }

    fn refresh_rollouts(&mut self, rollouts: Vec<(String, PathBuf)>) -> Arc<RecentSessionEvents> {
        let active_paths = rollouts
            .iter()
            .map(|(_, path)| path.clone())
            .collect::<HashSet<_>>();
        let mut snapshot_changed = self.last_snapshot.is_none()
            || self.rollouts.len() != active_paths.len()
            || self
                .rollouts
                .keys()
                .any(|path| !active_paths.contains(path));
        self.rollouts.retain(|path, _| active_paths.contains(path));

        for (session_id, rollout_path) in &rollouts {
            let Ok(metadata) = fs::metadata(rollout_path) else {
                snapshot_changed = true;
                self.rollouts.remove(rollout_path);
                continue;
            };
            let signature = RolloutSignature::from_metadata(&metadata);
            let cache_is_current = self.rollouts.get(rollout_path).is_some_and(|cached| {
                cached.session_id == *session_id && cached.signature == signature
            });
            if !cache_is_current {
                snapshot_changed = true;
                // Reuse the resumable parser state when the file only grew and
                // its consumed prefix is intact; otherwise start from scratch.
                let resumable = self
                    .rollouts
                    .remove(rollout_path)
                    .filter(|cached| cached.session_id == *session_id)
                    .map(|cached| cached.state)
                    .filter(|state| state.consumed_bytes > 0);
                let Some((state, _was_incremental)) = read_rollout_update(rollout_path, resumable)
                else {
                    continue;
                };
                #[cfg(test)]
                {
                    self.parse_count += 1;
                    if _was_incremental {
                        self.incremental_parse_count += 1;
                    }
                }
                self.rollouts.insert(
                    rollout_path.clone(),
                    CachedRolloutEvents {
                        session_id: session_id.clone(),
                        signature: signature.clone(),
                        state,
                    },
                );
            }
        }

        if !snapshot_changed && let Some(snapshot) = self.last_snapshot.as_ref() {
            if snapshot.pending_approvals.is_empty() {
                return Arc::clone(snapshot);
            }
            let snapshot = Arc::new(RecentSessionEvents {
                pending_approvals: self.refresh_pending_approvals(&rollouts),
                started_turns: Arc::clone(&snapshot.started_turns),
                aborted_turns: Arc::clone(&snapshot.aborted_turns),
                completed_turns: Arc::clone(&snapshot.completed_turns),
                session_statuses: Arc::clone(&snapshot.session_statuses),
                turn_configurations: Arc::clone(&snapshot.turn_configurations),
            });
            self.last_snapshot = Some(Arc::clone(&snapshot));
            return snapshot;
        }

        let mut pending_approvals = Vec::new();
        let mut started_turns = Vec::new();
        let mut aborted_turns = Vec::new();
        let mut completed_turns = Vec::new();
        let mut session_statuses = HashMap::new();
        let mut turn_configurations = HashMap::new();
        for (session_id, rollout_path) in rollouts {
            let Some(cached) = self.rollouts.get(&rollout_path).filter(|cached| {
                cached.session_id == session_id && !cached.state.parser.is_subagent()
            }) else {
                continue;
            };
            let state = &cached.state.parser;
            let signature = &cached.signature;
            let waiting_approvals = state.pending_approvals();
            let status = state.lifecycle_status(&waiting_approvals);
            let parsed = state.events();
            let rollout_is_snapshot_replay = rollout_path
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|directory| directory == "imported");
            let duration_ms = signature
                .modified
                .and_then(|modified| modified.elapsed().ok())
                .map(|duration| duration.as_millis())
                .unwrap_or_default();
            session_statuses.insert(session_id.clone(), status);
            turn_configurations.insert(session_id.clone(), parsed.turn_configurations.clone());
            pending_approvals.extend(waiting_approvals.into_iter().map(|(turn_id, waiting_id)| {
                PendingApproval {
                    session_id: session_id.clone(),
                    turn_id,
                    waiting_id,
                    duration_ms,
                }
            }));
            started_turns.extend(
                parsed
                    .started_turns
                    .iter()
                    .cloned()
                    .map(|turn_id| StartedTurn {
                        session_id: session_id.clone(),
                        turn_id,
                    }),
            );
            completed_turns.extend(parsed.completed_turns.iter().cloned().map(
                |(turn_id, duration_ms, completed_at, error)| CompletedTurn {
                    is_snapshot_replay: rollout_is_snapshot_replay
                        || parsed.snapshot_replay_turns.contains(&turn_id),
                    session_id: session_id.clone(),
                    turn_id,
                    duration_ms,
                    completed_at,
                    error,
                },
            ));
            aborted_turns.extend(
                parsed
                    .aborted_turns
                    .iter()
                    .cloned()
                    .map(|turn_id| AbortedTurn {
                        is_snapshot_replay: rollout_is_snapshot_replay
                            || parsed.snapshot_replay_turns.contains(&turn_id),
                        session_id: session_id.clone(),
                        turn_id,
                    }),
            );
        }

        let snapshot = Arc::new(RecentSessionEvents {
            pending_approvals,
            started_turns: Arc::new(started_turns),
            aborted_turns: Arc::new(aborted_turns),
            completed_turns: Arc::new(completed_turns),
            session_statuses: Arc::new(session_statuses),
            turn_configurations: Arc::new(turn_configurations),
        });
        self.last_snapshot = Some(Arc::clone(&snapshot));
        snapshot
    }

    fn refresh_pending_approvals(&self, rollouts: &[(String, PathBuf)]) -> Vec<PendingApproval> {
        let mut pending_approvals = Vec::new();
        for (session_id, rollout_path) in rollouts {
            let Some(cached) = self.rollouts.get(rollout_path).filter(|cached| {
                cached.session_id == *session_id && !cached.state.parser.is_subagent()
            }) else {
                continue;
            };
            let duration_ms = cached
                .signature
                .modified
                .and_then(|modified| modified.elapsed().ok())
                .map(|duration| duration.as_millis())
                .unwrap_or_default();
            pending_approvals.extend(cached.state.parser.pending_approvals().into_iter().map(
                |(turn_id, waiting_id)| PendingApproval {
                    session_id: session_id.clone(),
                    turn_id,
                    waiting_id,
                    duration_ms,
                },
            ));
        }
        pending_approvals
    }
}

/// Returns an updated parser state plus whether the previous state was resumed.
/// Falls back to a fresh state and the whole file whenever the previously
/// consumed prefix cannot be confirmed byte-for-byte.
fn read_rollout_update(
    path: &Path,
    resumable: Option<RolloutReadState>,
) -> Option<(RolloutReadState, bool)> {
    let full = || {
        let file = fs::File::open(path).ok()?;
        let mut state = RolloutReadState::default();
        consume_rollout_reader(BufReader::new(file), &mut state)?;
        Some((state, false))
    };
    let Some(mut state) = resumable else {
        return full();
    };
    if state.parser.is_subagent() {
        return Some((state, true));
    }
    let fingerprint_len = state.tail_fingerprint.len() as u64;
    if fingerprint_len == 0 || fingerprint_len > state.consumed_bytes {
        return full();
    }
    let Ok(mut file) = fs::File::open(path) else {
        return full();
    };
    // The caller only reaches this path because the file changed. Without new
    // bytes the change must have been an in-place rewrite, which invalidates
    // the accumulated state even though the tail may look untouched.
    match file.metadata() {
        Ok(metadata) if metadata.len() > state.consumed_bytes => {}
        _ => return full(),
    }
    // Rollout rewrites (provider normalisation) edit the `session_meta` header
    // and leave the tail byte-identical, so the head has to be checked too.
    if !state.head_fingerprint.is_empty() {
        let head_len = state.head_fingerprint.len();
        let mut head = [0u8; ROLLOUT_FINGERPRINT_LEN];
        if file.read_exact(&mut head[..head_len]).is_err()
            || head[..head_len] != *state.head_fingerprint.as_slice()
        {
            return full();
        }
    }
    if file
        .seek(SeekFrom::Start(state.consumed_bytes - fingerprint_len))
        .is_err()
    {
        return full();
    }
    let tail_len = state.tail_fingerprint.len();
    let mut fingerprint = [0u8; ROLLOUT_FINGERPRINT_LEN];
    if file.read_exact(&mut fingerprint[..tail_len]).is_err()
        || fingerprint[..tail_len] != *state.tail_fingerprint.as_slice()
    {
        return full();
    }
    consume_rollout_reader(BufReader::new(file), &mut state)?;
    Some((state, true))
}

fn consume_rollout_reader(mut reader: impl BufRead, state: &mut RolloutReadState) -> Option<()> {
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line).ok()?;
        if read == 0 {
            return Some(());
        }
        let consumed = state.parser.consume(&line);
        state.advance(&line, consumed);
        if state.parser.is_subagent() {
            return Some(());
        }
    }
}

fn query_recent_codey_rollouts(
    connection: &Connection,
    home: &Path,
    recent_after: i64,
) -> rusqlite::Result<Vec<(String, PathBuf)>> {
    let mut rollouts = Vec::new();
    // SQLite columns can contain values that violate their declared affinity.
    // Ignore only malformed rows here; prepare/query failures still invalidate
    // the cached connection so a replaced database is reopened next poll.
    let mut statement = connection.prepare_cached(
        "SELECT id, rollout_path FROM threads \
         WHERE archived=0 AND updated_at >= ?1 \
           AND typeof(id)='text' AND typeof(rollout_path)='text' \
         ORDER BY updated_at DESC LIMIT ?2",
    )?;
    let rows = statement.query_map(params![recent_after, MAX_RECENT_SESSIONS], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (session_id, rollout_path) = row?;
        let path = PathBuf::from(rollout_path);
        rollouts.push((
            session_id,
            if path.is_absolute() {
                path
            } else {
                home.join(path)
            },
        ));
    }
    Ok(rollouts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn recent_rollout_query_skips_malformed_rows_without_losing_valid_sessions() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE threads (
                    id TEXT NOT NULL,
                    rollout_path,
                    archived INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL
                );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO threads (id, rollout_path, archived, updated_at)
                 VALUES (?1, ?2, 0, ?3)",
                params!["valid", "rollouts/valid.jsonl", 1_i64],
            )
            .unwrap();

        let home = Path::new("codey-home");
        assert_eq!(
            query_recent_codey_rollouts(&connection, home, 0).unwrap(),
            vec![("valid".to_string(), home.join("rollouts/valid.jsonl"),)]
        );

        connection
            .execute(
                "INSERT INTO threads (id, rollout_path, archived, updated_at)
                 VALUES (?1, NULL, 0, ?2)",
                params!["invalid", 2_i64],
            )
            .unwrap();
        assert_eq!(
            query_recent_codey_rollouts(&connection, home, 0).unwrap(),
            vec![("valid".to_string(), home.join("rollouts/valid.jsonl"),)]
        );

        connection.execute_batch("DROP TABLE threads;").unwrap();
        assert!(query_recent_codey_rollouts(&connection, home, 0).is_err());
    }

    #[test]
    fn forked_rollouts_only_mark_inherited_completions_as_snapshot_replays() {
        let temp = tempfile::tempdir().unwrap();
        let rollout_path = temp.path().join("rollout-fork.jsonl");
        fs::write(
            &rollout_path,
            r#"
{"type":"session_meta","payload":{"id":"fork","session_id":"fork","forked_from_id":"parent"}}
{"type":"session_meta","payload":{"id":"parent","session_id":"parent","forked_from_id":"grandparent"}}
{"type":"event_msg","payload":{"type":"task_started","turn_id":"inherited"}}
{"type":"event_msg","payload":{"type":"task_complete","turn_id":"inherited","completed_at":300}}
{"type":"event_msg","payload":{"type":"task_started","turn_id":"inherited-aborted"}}
{"type":"event_msg","payload":{"type":"turn_aborted","turn_id":"inherited-aborted"}}
{"type":"event_msg","payload":{"type":"task_started","turn_id":"live"}}
{"type":"session_meta","payload":{"id":"fork","session_id":"fork","forked_from_id":"parent"}}
{"type":"event_msg","payload":{"type":"task_complete","turn_id":"live","completed_at":301}}
"#,
        )
        .unwrap();
        let mut cache = RecentSessionEventCache::default();

        let events = cache.refresh_rollouts(vec![("fork".to_string(), rollout_path)]);

        assert_eq!(
            events
                .completed_turns
                .iter()
                .map(|turn| (turn.turn_id.as_str(), turn.is_snapshot_replay))
                .collect::<Vec<_>>(),
            vec![("inherited", true), ("live", false)]
        );
        assert_eq!(
            events
                .aborted_turns
                .iter()
                .map(|turn| (turn.turn_id.as_str(), turn.is_snapshot_replay))
                .collect::<Vec<_>>(),
            vec![("inherited-aborted", true)]
        );
    }

    #[test]
    fn unchanged_rollouts_reuse_cached_lifecycle_events() {
        let temp = tempfile::tempdir().unwrap();
        let rollout_path = temp.path().join("rollout-thread-1.jsonl");
        fs::write(
            &rollout_path,
            r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
        )
        .unwrap();
        let rollouts = || vec![("thread-1".to_string(), rollout_path.clone())];
        let mut cache = RecentSessionEventCache::default();

        let first = cache.refresh_rollouts(rollouts());
        let second = cache.refresh_rollouts(rollouts());

        assert_eq!(cache.parse_count, 1);
        assert_eq!(
            first.session_statuses.get("thread-1"),
            Some(&SessionLifecycleStatus::Running)
        );
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(second.started_turns, first.started_turns);

        writeln!(
            fs::OpenOptions::new()
                .append(true)
                .open(&rollout_path)
                .unwrap(),
            r#"{{"type":"event_msg","payload":{{"type":"task_complete","turn_id":"turn-1"}}}}"#
        )
        .unwrap();
        let updated = cache.refresh_rollouts(rollouts());

        assert_eq!(cache.parse_count, 2);
        assert_eq!(
            updated.session_statuses.get("thread-1"),
            Some(&SessionLifecycleStatus::Idle)
        );
    }

    #[test]
    fn pending_approval_snapshots_are_rebuilt_for_fresh_wait_durations() {
        let temp = tempfile::tempdir().unwrap();
        let rollout_path = temp.path().join("rollout-waiting.jsonl");
        fs::write(
            &rollout_path,
            r#"{"type":"turn_context","payload":{"turn_id":"turn-1"}}
{"type":"response_item","payload":{"type":"function_call","name":"request_permissions","call_id":"pending"}}
"#,
        )
        .unwrap();
        let rollouts = || vec![("waiting".to_string(), rollout_path.clone())];
        let mut cache = RecentSessionEventCache::default();

        let first = cache.refresh_rollouts(rollouts());
        let second = cache.refresh_rollouts(rollouts());

        assert_eq!(first.pending_approvals.len(), 1);
        assert_eq!(second.pending_approvals.len(), 1);
        assert!(!Arc::ptr_eq(&first, &second));
        assert!(Arc::ptr_eq(&first.started_turns, &second.started_turns));
        assert!(Arc::ptr_eq(&first.aborted_turns, &second.aborted_turns));
        assert!(Arc::ptr_eq(&first.completed_turns, &second.completed_turns));
        assert!(Arc::ptr_eq(
            &first.session_statuses,
            &second.session_statuses
        ));
        assert!(Arc::ptr_eq(
            &first.turn_configurations,
            &second.turn_configurations
        ));
    }

    #[test]
    fn appended_rollout_lines_are_parsed_incrementally() {
        let temp = tempfile::tempdir().unwrap();
        let rollout_path = temp.path().join("rollout-thread-1.jsonl");
        let mut contents = String::from(
            r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
        );
        contents.push('\n');
        // Pad past the fingerprint window so the resume check is meaningful.
        for index in 0..40 {
            contents.push_str(&format!(
                "{{\"type\":\"turn_context\",\"payload\":{{\"turn_id\":\"turn-{index}\",\"model\":\"m\"}}}}\n"
            ));
        }
        fs::write(&rollout_path, &contents).unwrap();
        let rollouts = || vec![("thread-1".to_string(), rollout_path.clone())];
        let mut cache = RecentSessionEventCache::default();

        let first = cache.refresh_rollouts(rollouts());
        assert_eq!(cache.incremental_parse_count, 0);
        assert_eq!(
            first.session_statuses.get("thread-1"),
            Some(&SessionLifecycleStatus::Running)
        );

        let mut handle = fs::OpenOptions::new()
            .append(true)
            .open(&rollout_path)
            .unwrap();
        writeln!(
            handle,
            r#"{{"type":"event_msg","payload":{{"type":"task_complete","turn_id":"turn-1"}}}}"#
        )
        .unwrap();
        drop(handle);

        let updated = cache.refresh_rollouts(rollouts());

        assert_eq!(
            cache.incremental_parse_count, 1,
            "an appended rollout must resume instead of re-reading the whole file"
        );
        assert_eq!(
            updated.session_statuses.get("thread-1"),
            Some(&SessionLifecycleStatus::Idle)
        );
        assert_eq!(updated.started_turns.len(), 1, "history must not be lost");
        assert_eq!(updated.completed_turns.len(), 1);
        assert_eq!(updated.turn_configurations["thread-1"].len(), 40);
    }

    #[test]
    fn a_partial_jsonl_tail_is_resumed_after_the_writer_finishes_the_line() {
        let temp = tempfile::tempdir().unwrap();
        let rollout_path = temp.path().join("rollout-thread-1.jsonl");
        fs::write(
            &rollout_path,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"thread-1\"}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn"
            ),
        )
        .unwrap();
        let rollouts = || vec![("thread-1".to_string(), rollout_path.clone())];
        let mut cache = RecentSessionEventCache::default();

        let first = cache.refresh_rollouts(rollouts());
        assert!(first.started_turns.is_empty());

        fs::OpenOptions::new()
            .append(true)
            .open(&rollout_path)
            .unwrap()
            .write_all(b"-1\"}}\n")
            .unwrap();
        let updated = cache.refresh_rollouts(rollouts());

        assert_eq!(cache.incremental_parse_count, 1);
        assert_eq!(
            updated
                .started_turns
                .iter()
                .map(|turn| turn.turn_id.as_str())
                .collect::<Vec<_>>(),
            vec!["turn-1"]
        );
        assert_eq!(
            updated.session_statuses.get("thread-1"),
            Some(&SessionLifecycleStatus::Running)
        );
    }

    #[test]
    fn rewritten_rollouts_fall_back_to_a_full_parse() {
        let temp = tempfile::tempdir().unwrap();
        let rollout_path = temp.path().join("rollout-thread-1.jsonl");
        let original = format!(
            "{}\n{}\n",
            r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-2"}}"#
        );
        fs::write(&rollout_path, &original).unwrap();
        let rollouts = || vec![("thread-1".to_string(), rollout_path.clone())];
        let mut cache = RecentSessionEventCache::default();

        let first = cache.refresh_rollouts(rollouts());
        assert_eq!(first.started_turns.len(), 2);

        // Codey rewrites rollouts in place when deleting turns; the cached
        // prefix is then stale. Make the rewrite longer than the original so
        // only the fingerprint check — not the shrink check — can catch it.
        let rewritten = format!(
            "{}\n{}\n{}\n",
            r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-9"}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-9"}}"#,
            r#"{"type":"turn_context","payload":{"turn_id":"turn-9","model":"m"}}"#
        );
        assert!(rewritten.len() > original.len());
        fs::write(&rollout_path, &rewritten).unwrap();

        let updated = cache.refresh_rollouts(rollouts());

        assert_eq!(
            cache.incremental_parse_count, 0,
            "an in-place rewrite must not be treated as an append"
        );
        assert_eq!(
            updated
                .started_turns
                .iter()
                .map(|turn| turn.turn_id.as_str())
                .collect::<Vec<_>>(),
            vec!["turn-9"]
        );
        assert_eq!(
            updated.session_statuses.get("thread-1"),
            Some(&SessionLifecycleStatus::Idle)
        );
    }

    #[test]
    fn header_rewrites_with_an_identical_tail_fall_back_to_a_full_parse() {
        let temp = tempfile::tempdir().unwrap();
        let rollout_path = temp.path().join("rollout-thread-1.jsonl");
        let header = |provider: &str| {
            format!(
                r#"{{"type":"session_meta","payload":{{"model_provider":"{provider}","id":"thread-1"}}}}"#
            )
        };
        let body = r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}"#;
        fs::write(
            &rollout_path,
            format!("{}\n{}\n", header("aaaaa_global"), body),
        )
        .unwrap();
        let rollouts = || vec![("thread-1".to_string(), rollout_path.clone())];
        let mut cache = RecentSessionEventCache::default();

        cache.refresh_rollouts(rollouts());

        // Provider normalisation rewrites only the session_meta header, so the
        // consumed tail stays byte-identical; the head check has to catch it.
        fs::write(
            &rollout_path,
            format!(
                "{}\n{}\n{}\n",
                header("codey_global"),
                body,
                r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-1"}}"#
            ),
        )
        .unwrap();

        cache.refresh_rollouts(rollouts());

        assert_eq!(
            cache.incremental_parse_count, 0,
            "a rewritten header must not be resumed from the stale offset"
        );
    }

    #[test]
    fn unchanged_database_files_reuse_read_connections() {
        let temp = tempfile::tempdir().unwrap();
        let rollout_path = temp.path().join("rollout-thread-1.jsonl");
        fs::write(
            &rollout_path,
            r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"}}"#,
        )
        .unwrap();
        let database_path = temp.path().join("state_5.sqlite");
        let database = Connection::open(&database_path).unwrap();
        database
            .execute_batch(
                "CREATE TABLE threads (
                    id TEXT PRIMARY KEY,
                    rollout_path TEXT NOT NULL,
                    archived INTEGER NOT NULL DEFAULT 0,
                    updated_at INTEGER NOT NULL
                );",
            )
            .unwrap();
        let updated_at = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        database
            .execute(
                "INSERT INTO threads (id, rollout_path, archived, updated_at)
                 VALUES (?1, ?2, 0, ?3)",
                params!["thread-1", rollout_path.to_string_lossy(), updated_at],
            )
            .unwrap();
        drop(database);

        let mut cache = RecentSessionEventCache::default();
        let first = cache.refresh(temp.path());
        let second = cache.refresh(temp.path());

        assert_eq!(cache.database_open_count, 1);
        assert_eq!(
            first.session_statuses.get("thread-1"),
            Some(&SessionLifecycleStatus::Running)
        );
        assert_eq!(second.started_turns, first.started_turns);

        cache.release_database_connections();
        assert!(cache.database_connections.is_empty());
    }
}
