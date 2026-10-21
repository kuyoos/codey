//! 原生事件中的轮次、生成步骤和工具计时。按追加位置读取，避免轮询重复扫描历史。
use super::{ConversationStatsSnapshot, normalize_session_id};
use serde_json::Value;
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;

#[derive(Default)]
pub(super) struct Activity {
    pub offset: u64,
    turns: HashSet<String>,
    responses: HashSet<String>,
    tools: HashSet<String>,
    tool_ms: u64,
    own_segment: Option<bool>,
    pub(super) usage_snapshot: Option<ConversationStatsSnapshot>,
}

impl Activity {
    pub fn read_append(&mut self, path: &Path, session_id: &str) -> std::io::Result<()> {
        let mut file = std::fs::File::open(path)?;
        file.seek(SeekFrom::Start(self.offset))?;
        let mut reader = BufReader::new(file);
        loop {
            // 单行保留上限 8 MiB，超大工具输出按块跳过。未写完的末行下次再读。
            let start = self.offset;
            let mut line = Vec::new();
            let mut oversized = false;
            let mut consumed = 0u64;
            loop {
                let buf = reader.fill_buf()?;
                if buf.is_empty() {
                    // 完整 JSON 可以暂时没有换行；不推进偏移，追加后仍重新校验。
                    if !oversized && let Ok(record) = serde_json::from_slice::<Value>(&line) {
                        self.record(&record, session_id);
                    }
                    return Ok(());
                }
                let end = buf.iter().position(|v| *v == b'\n');
                let size = end.map_or(buf.len(), |v| v + 1);
                consumed += size as u64;
                if !oversized && line.len() + size <= 8 * 1024 * 1024 {
                    line.extend_from_slice(&buf[..size]);
                } else {
                    oversized = true;
                    line.clear();
                }
                reader.consume(size);
                if end.is_some() {
                    break;
                }
            }
            self.offset = start + consumed;
            if !oversized && let Ok(record) = serde_json::from_slice::<Value>(&line) {
                self.record(&record, session_id);
            }
        }
    }

    pub(super) fn record(&mut self, record: &Value, session_id: &str) {
        let payload = &record["payload"];
        if record["type"] == "session_meta" {
            if let Some(id) = payload["session_id"]
                .as_str()
                .or_else(|| payload["id"].as_str())
            {
                self.own_segment = Some(normalize_session_id(id).eq_ignore_ascii_case(session_id));
            }
            return;
        }
        if self.own_segment == Some(false) {
            return;
        }
        if ["thread_id", "session_id"].iter().any(|key| {
            payload[*key]
                .as_str()
                .is_some_and(|id| !normalize_session_id(id).eq_ignore_ascii_case(session_id))
        }) {
            return;
        }
        if let Some(usage) = super::history::usage_from_record(record, session_id) {
            self.usage_snapshot = Some(usage);
        }
        match (record["type"].as_str(), payload["type"].as_str()) {
            (Some("event_msg"), Some("task_started")) => {
                if let Some(id) = payload["turn_id"].as_str().filter(|id| !id.is_empty()) {
                    self.turns.insert(id.to_owned());
                }
            }
            (Some("token_usage_record"), _) => {
                if payload["session_id"]
                    .as_str()
                    .is_some_and(|id| normalize_session_id(id).eq_ignore_ascii_case(session_id))
                    && let Some(id) = payload["response_id"].as_str().filter(|id| !id.is_empty())
                {
                    self.responses.insert(id.to_owned());
                }
            }
            (Some("event_msg"), Some("item_completed")) => {
                let item = &payload["item"];
                if !matches!(
                    item["type"].as_str(),
                    Some(
                        "McpToolCall"
                            | "CommandExecution"
                            | "CollabAgentToolCall"
                            | "FileChange"
                            | "WebSearch"
                            | "ImageGeneration"
                            | "DynamicToolCall"
                    )
                ) {
                    return;
                }
                let duration = item["duration"]["secs"]
                    .as_u64()
                    .and_then(|secs| {
                        let nanos = item["duration"]["nanos"].as_u64()?;
                        (nanos < 1_000_000_000)
                            .then(|| secs.saturating_mul(1000).saturating_add(nanos / 1_000_000))
                    })
                    .or_else(|| {
                        payload["completed_at_ms"]
                            .as_u64()?
                            .checked_sub(payload["started_at_ms"].as_u64()?)
                    });
                if let (Some(id), Some(duration)) = (item["id"].as_str(), duration)
                    && self.tools.insert(id.to_owned())
                {
                    self.tool_ms = self.tool_ms.saturating_add(duration);
                }
            }
            _ => {}
        }
    }

    pub fn apply(&self, snapshot: &mut ConversationStatsSnapshot) {
        if !self.turns.is_empty() || !self.responses.is_empty() {
            snapshot.turn_count = (!self.turns.is_empty()).then_some(self.turns.len() as u64);
            snapshot.step_count =
                (!self.responses.is_empty()).then_some(self.responses.len() as u64);
            snapshot.activity_source = Some("rollout");
            snapshot.activity_scope = Some("recorded_session");
            if snapshot.step_count.is_some() {
                snapshot.step_source = Some("rollout");
                snapshot.step_scope = Some("recorded_session");
            }
        }
        if !self.tools.is_empty() {
            snapshot.tool_duration_ms = Some(self.tool_ms);
            snapshot.tool_timing_source = Some("rollout");
            snapshot.tool_timing_scope = Some("recorded_tools");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn activity_deduplicates_and_requires_own_session_and_actual_tool_timing() {
        let mut stats = Activity::default();
        let turn = json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"turn"}});
        let response = json!({"type":"token_usage_record","payload":{"session_id":"own","response_id":"response"}});
        let tool = json!({"type":"event_msg","payload":{"type":"item_completed","thread_id":"own","item":{"type":"McpToolCall","id":"tool","duration":{"secs":2,"nanos":500000000}},"started_at_ms":1,"completed_at_ms":9}});
        for item in [&turn, &turn, &response, &response, &tool, &tool] {
            stats.record(item, "own");
        }
        stats.record(&json!({"type":"token_usage_record","payload":{"session_id":"foreign","response_id":"foreign"}}), "own");
        stats.record(&json!({"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":10}}}}), "own");
        stats.record(&json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"Reasoning","id":"reason"},"started_at_ms":1,"completed_at_ms":10000}}), "own");
        let mut snapshot = ConversationStatsSnapshot::default();
        stats.apply(&mut snapshot);
        assert_eq!(snapshot.turn_count, Some(1));
        assert_eq!(snapshot.step_count, Some(1));
        assert_eq!(snapshot.tool_duration_ms, Some(2500));
        assert_eq!(snapshot.model_duration_ms, None);
    }

    #[test]
    fn append_reader_retries_incomplete_line_and_ignores_inherited_segment() {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            file,
            "{}",
            json!({"type":"session_meta","payload":{"id":"parent"}})
        )
        .unwrap();
        writeln!(
            file,
            "{}",
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"foreign"}})
        )
        .unwrap();
        writeln!(
            file,
            "{}",
            json!({"type":"session_meta","payload":{"id":"own"}})
        )
        .unwrap();
        write!(file, "{{\"type\":\"event_msg\",\"payload\":").unwrap();
        let mut activity = Activity::default();
        activity.read_append(file.path(), "own").unwrap();
        assert!(activity.turns.is_empty());
        write!(
            file,
            "{{\"type\":\"task_started\",\"turn_id\":\"own-turn\"}}}}"
        )
        .unwrap();
        activity.read_append(file.path(), "own").unwrap();
        assert_eq!(activity.turns.len(), 1);
        writeln!(file).unwrap();
        activity.read_append(file.path(), "own").unwrap();
        activity.read_append(file.path(), "own").unwrap();
        assert_eq!(activity.turns.len(), 1);
    }
}
