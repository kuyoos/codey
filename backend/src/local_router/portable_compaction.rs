use super::*;

const MAX_PORTABLE_TICKETS: usize = 128;
const PREPARE_TTL: Duration = Duration::from_secs(120);
const RETAIN_TTL: Duration = Duration::from_secs(15 * 60);

#[derive(Clone, Debug)]
pub(crate) struct PortableTicket {
    token: String,
    source: Arc<RouteTarget>,
    model: String,
    generation: u64,
    created: Instant,
    state: &'static str,
}

impl PortableTicket {
    fn expire(&mut self) {
        if self.state == "prepared" && self.created.elapsed() >= PREPARE_TTL {
            self.state = "expired";
        }
    }
}

// A concrete thread header always wins. A child must never consume a ticket
// registered for its parent through a shared session header.
fn portable_request_key(request: &HttpRequest) -> Option<String> {
    if let Some(thread) = incoming_header(request, "thread-id") {
        return valid_codex_session_id(thread).map(|id| format!("thread-id:{id}"));
    }
    if request_is_subagent(request)
        || incoming_header(request, "x-codex-parent-thread-id").is_some()
    {
        return None;
    }
    incoming_header(request, "session-id")
        .and_then(valid_codex_session_id)
        .map(|id| format!("thread-id:{id}"))
}

impl LocalRouter {
    pub(crate) fn portable_compaction(&self, operation: &str, payload: &Value) -> Result<Value> {
        self.portable_compaction_from_home(operation, payload, crate::codex_config::codex_home())
    }

    fn portable_compaction_from_home(
        &self,
        operation: &str,
        payload: &Value,
        home: &std::path::Path,
    ) -> Result<Value> {
        let thread = payload
            .get("threadId")
            .and_then(Value::as_str)
            .and_then(valid_codex_session_id)
            .context("缺少有效的 threadId")?;
        let key = format!("thread-id:{thread}");
        let restore_source = if operation == "prepare"
            && !self
                .bindings
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .routes
                .contains_key(&key)
        {
            crate::session_model::native_compaction_source(home, thread)?
        } else {
            None
        };
        let snapshot = self
            .snapshot
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut bindings = self
            .bindings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for ticket in bindings.portable_tickets.values_mut() {
            ticket.expire();
        }
        if operation == "prepare" {
            bindings.portable_tickets.retain(|_, ticket| {
                ticket.state == "running" || ticket.created.elapsed() < RETAIN_TTL
            });
            anyhow::ensure!(
                !bindings
                    .portable_tickets
                    .get(&key)
                    .is_some_and(|ticket| matches!(ticket.state, "prepared" | "running")),
                "此会话已有可移植摘要正在准备或执行"
            );
            anyhow::ensure!(
                !bindings.compacting.contains(&key)
                    && !bindings
                        .compacting
                        .contains(&format!("session-id:{thread}")),
                "此会话正在压缩，请完成后重试"
            );
            let target = payload
                .get("targetProviderId")
                .and_then(Value::as_str)
                .context("缺少 targetProviderId")?;
            let target_model = payload
                .get("targetModel")
                .and_then(Value::as_str)
                .context("缺少 targetModel")?;
            let target_route = snapshot.target_for_request(target_model, Some(target), None)?;
            // 文件读取期间可能已有请求确认了活动线路，不能覆盖该绑定。
            if !bindings.routes.contains_key(&key)
                && let Some(source) = restore_source.as_deref()
            {
                anyhow::ensure!(
                    snapshot.aliases.contains_key(&model_id::key(source)),
                    "压缩历史的源线路或模型已不可用，请恢复原线路后重试"
                );
                let resolved = snapshot.target_for_request(source, None, None)?;
                anyhow::ensure!(
                    resolved.fallback_reason.is_none(),
                    "压缩历史的源线路已不可用"
                );
                anyhow::ensure!(
                    ProtocolBridge::from_upstream_protocol(resolved.protocol)
                        == ProtocolBridge::NativeResponses,
                    "压缩历史的源线路协议已变化"
                );
                bindings.remember_resolved(
                    std::slice::from_ref(&key),
                    &resolved,
                    false,
                    true,
                    Some(false),
                );
            }
            let Some(binding) = bindings.routes.get(&key).cloned() else {
                return Ok(json!({"status":"ok","required":false}));
            };
            if binding.portable
                || (binding.provider_id == target
                    && binding.context_config == Some(target_route.route.context_config))
            {
                return Ok(json!({"status":"ok","required":false}));
            }
            let source = snapshot
                .routes
                .get(&binding.provider_id)
                .context("源线路已不可用，无法生成交接摘要")?;
            if ProtocolBridge::from_upstream_protocol(source.protocol)
                != ProtocolBridge::NativeResponses
            {
                return Ok(json!({"status":"ok","required":false}));
            }
            anyhow::ensure!(
                !binding.upstream_model.is_empty()
                    && binding.context_config == Some(source.context_config),
                "源线路身份已变化，请恢复原线路后重试"
            );
            anyhow::ensure!(
                bindings.portable_tickets.contains_key(&key)
                    || bindings.portable_tickets.len() < MAX_PORTABLE_TICKETS,
                "可移植摘要任务已满，请稍后重试"
            );
            let token = Uuid::new_v4().to_string();
            bindings.portable_tickets.insert(
                key,
                PortableTicket {
                    token: token.clone(),
                    source: Arc::clone(source),
                    model: binding.upstream_model.clone(),
                    generation: binding.generation,
                    created: Instant::now(),
                    state: "prepared",
                },
            );
            return Ok(json!({"status":"ok","required":true,"token":token,
                "sourceProviderId":binding.provider_id,"sourceModel":binding.upstream_model}));
        }
        let token = payload
            .get("token")
            .and_then(Value::as_str)
            .context("缺少 token")?;
        let ticket = bindings
            .portable_tickets
            .get(&key)
            .filter(|ticket| ticket.token == token)
            .context("可移植摘要任务不存在或令牌不匹配")?;
        match operation {
            "status" => Ok(json!({"status":"ok","state":ticket.state})),
            "confirm" => {
                anyhow::ensure!(ticket.state == "completed", "可移植摘要尚未完成");
                let generation = ticket.generation;
                let source = Arc::clone(&ticket.source);
                anyhow::ensure!(
                    snapshot
                        .routes
                        .get(&source.provider_id)
                        .is_some_and(|route| route.context_config == source.context_config),
                    "源线路身份已变化"
                );
                let binding = bindings.routes.get_mut(&key).context("源会话绑定已失效")?;
                anyhow::ensure!(
                    binding.generation == generation
                        && binding.context_config == Some(source.context_config),
                    "源会话已变化，请重新生成摘要"
                );
                binding.portable = true;
                Ok(json!({"status":"ok"}))
            }
            "cancel" => {
                anyhow::ensure!(
                    matches!(ticket.state, "prepared" | "failed" | "expired"),
                    "只能取消尚未开始、失败或过期的可移植摘要"
                );
                bindings.portable_tickets.remove(&key);
                Ok(json!({"status":"ok"}))
            }
            _ => anyhow::bail!("未知的可移植摘要操作"),
        }
    }
}

pub(crate) struct PortableAttempt {
    bindings: Arc<Mutex<RouteBindings>>,
    key: String,
    token: String,
    pub(crate) source: Arc<RouteTarget>,
    pub(crate) model: String,
    completed: bool,
}

impl PortableAttempt {
    pub(crate) fn acquire(
        server: &RouterServer,
        request: &HttpRequest,
        compacting: bool,
        snapshot: &RouterSnapshot,
    ) -> Result<Option<Self>> {
        let Some(key) = portable_request_key(request) else {
            return Ok(None);
        };
        let mut bindings = server
            .bindings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let binding = bindings.routes.get(&key).cloned();
        let Some(ticket) = bindings.portable_tickets.get_mut(&key) else {
            return Ok(None);
        };
        ticket.expire();
        if !compacting {
            anyhow::ensure!(
                !matches!(ticket.state, "prepared" | "running"),
                "此会话正在准备可移植摘要，请完成后再发送请求"
            );
            return Ok(None);
        }
        if ticket.state == "completed" {
            return Ok(None);
        }
        anyhow::ensure!(
            ticket.state == "prepared",
            "可移植摘要任务已失效或正在运行，请重新准备"
        );
        let source_matches = binding.is_some_and(|binding| {
            binding.generation == ticket.generation
                && binding.provider_id == ticket.source.provider_id
                && binding.upstream_model == ticket.model
                && binding.context_config == Some(ticket.source.context_config)
        });
        if !source_matches
            || !snapshot
                .routes
                .get(&ticket.source.provider_id)
                .is_some_and(|route| {
                    route.context_config == ticket.source.context_config
                        && route.models.contains(&ticket.model)
                })
        {
            ticket.state = "failed";
            anyhow::bail!("源线路或模型已变化，请重新准备可移植摘要");
        }
        ticket.state = "running";
        Ok(Some(Self {
            bindings: Arc::clone(&server.bindings),
            key,
            token: ticket.token.clone(),
            source: Arc::clone(&ticket.source),
            model: ticket.model.clone(),
            completed: false,
        }))
    }

    pub(crate) fn complete(&mut self) {
        self.completed = true;
    }
}

impl Drop for PortableAttempt {
    fn drop(&mut self) {
        let mut bindings = self
            .bindings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(ticket) = bindings.portable_tickets.get_mut(&self.key)
            && ticket.token == self.token
            && ticket.state == "running"
        {
            ticket.state = if self.completed {
                "completed"
            } else {
                "failed"
            };
        }
    }
}

// None denotes an unresolved delta; it cannot replace the stored history state.
pub(crate) fn resolved_history_portability(body: &Value) -> Option<bool> {
    if validate_cross_route_context(body).is_err() {
        Some(false)
    } else if responses_previous_response_id(body).is_none() {
        Some(true)
    } else {
        None
    }
}

impl RouteBindings {
    pub(crate) fn remember_resolved(
        &mut self,
        keys: &[String],
        resolved: &RouteSelection,
        refresh_session: bool,
        native_compaction: bool,
        history_portable: Option<bool>,
    ) {
        let portable = keys
            .iter()
            .map(|key| {
                (
                    key.clone(),
                    self.routes.get(key).is_some_and(|binding| {
                        binding.portable
                            && binding.provider_id == resolved.provider_id
                            && binding.context_config == Some(resolved.route.context_config)
                            && binding.upstream_model == resolved.upstream_model
                    }),
                )
            })
            .collect::<HashMap<_, _>>();
        self.remember(keys, &resolved.provider_id, refresh_session);
        for key in keys {
            if let Some(binding) = self.routes.get_mut(key)
                && binding.provider_id == resolved.provider_id
                && binding.upstream_model.is_empty()
            {
                binding.upstream_model = resolved.upstream_model.clone();
                binding.context_config = Some(resolved.route.context_config);
                binding.portable = !native_compaction
                    && history_portable
                        .unwrap_or_else(|| portable.get(key).copied().unwrap_or(false));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cold_native_history(source: &str, target: &str) -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        let records = [
            json!({"type":"session_meta","payload":{"id":"parent","model_provider":"codey_router"}}),
            json!({"type":"compacted","payload":{
                "replacement_history":[{"type":"compaction","encrypted_content":"source-ciphertext"}],
                "resume_metadata":{"previous_turn_settings":{"model":source}}
            }}),
            json!({"type":"turn_context","payload":{"model":target}}),
        ];
        std::fs::write(
            home.path().join("session.jsonl"),
            records
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        let db = rusqlite::Connection::open(home.path().join("state_5.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE threads (id TEXT, rollout_path TEXT, model TEXT);")
            .unwrap();
        db.execute(
            "INSERT INTO threads VALUES ('parent','session.jsonl',?1)",
            [target],
        )
        .unwrap();
        home
    }

    #[tokio::test]
    async fn portable_compaction_cold_native_history_restores_source_before_target() {
        let (router, upstream, source, target, model) = fixture().await;
        let source_alias = model_alias(&source, &model);
        let target_alias = model_alias(&target, "target-model");
        let home = cold_native_history(&source_alias, &target_alias);
        let before = std::fs::read(home.path().join("session.jsonl")).unwrap();
        let payload =
            json!({"threadId":"parent","targetProviderId":target,"targetModel":"target-model"});
        let prepared = router
            .portable_compaction_from_home("prepare", &payload, home.path())
            .unwrap();
        assert_eq!(prepared["required"], true);
        assert_eq!(prepared["sourceProviderId"], source);
        assert_eq!(prepared["sourceModel"], model);
        let upstream_task = tokio::spawn(async move {
            for stage in 0..2 {
                let (mut socket, _) = upstream.accept().await.unwrap();
                let request = read_http_request(&mut socket).await.unwrap();
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                if stage == 0 {
                    assert_eq!(body["model"], "provider-model");
                    assert_eq!(body["input"][0]["encrypted_content"], "source-ciphertext");
                } else {
                    assert_eq!(body["model"], "target-model");
                    assert_eq!(body["input"][0]["content"][0]["text"], "portable handoff");
                    assert!(!body.to_string().contains("source-ciphertext"));
                }
                write_json_response(&mut socket, 200, &json!({"id":"reply","status":"completed",
                    "output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"portable handoff"}]}]})).await.unwrap();
            }
        });
        let client = client();
        let response = post(&client, &router, "parent", &target_alias,
            json!([{"type":"compaction","encrypted_content":"source-ciphertext"},{"type":"compaction_trigger"}]))
            .send().await.unwrap();
        assert_eq!(response.status().as_u16(), 200);
        let result: Value = response.json().await.unwrap();
        router
            .portable_compaction(
                "confirm",
                &json!({"threadId":"parent","token":prepared["token"]}),
            )
            .unwrap();
        assert_eq!(
            post(
                &client,
                &router,
                "parent",
                &target_alias,
                result["output"].clone()
            )
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
            200
        );
        upstream_task.await.unwrap();
        assert_eq!(
            std::fs::read(home.path().join("session.jsonl")).unwrap(),
            before
        );
        router.stop().await.unwrap();
    }

    #[tokio::test]
    async fn portable_compaction_cold_source_never_falls_back_or_overwrites_active_binding() {
        let (router, _upstream, source, target, model) = fixture().await;
        let payload =
            json!({"threadId":"parent","targetProviderId":target,"targetModel":"target-model"});
        for invalid_source in [
            "missing/provider-model".to_string(),
            model_alias(&source, "disabled-model"),
            model.clone(),
        ] {
            let home = cold_native_history(&invalid_source, &model_alias(&target, "target-model"));
            assert!(
                router
                    .portable_compaction_from_home("prepare", &payload, home.path())
                    .is_err()
            );
            assert!(
                !router
                    .bindings
                    .lock()
                    .unwrap()
                    .routes
                    .contains_key("thread-id:parent")
            );
        }
        let home = cold_native_history(
            &model_alias(&source, &model),
            &model_alias(&target, "target-model"),
        );
        bind_source(&router, &target, "target-model");
        assert_eq!(
            router
                .portable_compaction_from_home("prepare", &payload, home.path())
                .unwrap()["required"],
            false
        );
        assert_eq!(
            router.bindings.lock().unwrap().routes["thread-id:parent"].provider_id,
            target
        );
        router.stop().await.unwrap();
    }

    #[test]
    fn cross_route_compaction_trigger_is_an_operation_not_history() {
        let valid = json!({"input":[{"role":"user","content":"full history"},{"type":"compaction_trigger"}]});
        assert!(validate_cross_route_context(&valid).is_ok());
        assert!(validate_portable_context(&valid).is_err());
        for input in [
            json!([{"type":"compaction","encrypted_content":"opaque"},{"type":"compaction_trigger"}]),
            json!([{"type":"compaction_trigger"},{"role":"user","content":"next"}]),
            json!([{"type":"compaction_trigger","encrypted_content":"opaque"}]),
            json!([{"role":"user","content":[{"type":"compaction_trigger"}]}]),
            json!([{"type":"item_reference","id":"old"},{"type":"compaction_trigger"}]),
        ] {
            assert!(validate_cross_route_context(&json!({"input":input})).is_err());
        }
    }

    #[test]
    fn portable_compaction_unrestored_delta_cannot_establish_portability() {
        assert_eq!(
            resolved_history_portability(
                &json!({"input":"next","previous_response_id":"resp-old"})
            ),
            None
        );
        assert_eq!(
            resolved_history_portability(
                &json!({"input":[{"type":"compaction","encrypted_content":"opaque"}],"previous_response_id":"resp-old"})
            ),
            Some(false)
        );
        assert_eq!(
            resolved_history_portability(
                &json!({"input":[{"type":"item_reference","id":"item-old"}]})
            ),
            Some(false)
        );
        assert_eq!(
            resolved_history_portability(&json!({"input":"full history"})),
            Some(true)
        );
    }

    #[tokio::test]
    async fn portable_compaction_cross_route_full_history_can_compact_on_target() {
        let (router, upstream, source, target, model) = fixture().await;
        bind_source(&router, &source, &model);
        let history = json!([{"role":"user","content":"complete visible history"},{"type":"compaction_trigger"}]);
        let expected = history.clone();
        let upstream_task = tokio::spawn(async move {
            let (mut socket, _) = upstream.accept().await.unwrap();
            let request = read_http_request(&mut socket).await.unwrap();
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["input"], expected);
            assert_eq!(body["model"], "target-model");
            write_json_response(
                &mut socket,
                200,
                &json!({"id":"native","status":"completed",
                "output":[{"type":"compaction","encrypted_content":"target-ciphertext"}]}),
            )
            .await
            .unwrap();
        });
        let response = post(
            &client(),
            &router,
            "parent",
            &model_alias(&target, "target-model"),
            history,
        )
        .send()
        .await
        .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        upstream_task.await.unwrap();
        router.stop().await.unwrap();
    }

    #[tokio::test]
    async fn portable_compaction_full_native_history_switch_preserves_messages_and_tools() {
        let (router, upstream, source, target, model) = fixture().await;
        let history = json!([
            {"role":"user","content":"inspect the file"},
            {"type":"function_call","call_id":"call-1","name":"read_file","arguments":"{}"},
            {"type":"function_call_output","call_id":"call-1","output":"file contents"},
            {"role":"assistant","content":[{"type":"output_text","text":"found it"}]}
        ]);
        let expected = history.clone();
        let upstream_task = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut socket, _) = upstream.accept().await.unwrap();
                let request = read_http_request(&mut socket).await.unwrap();
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                assert_eq!(body["input"], expected);
                write_json_response(&mut socket, 200, &json!({"id":"reply","output":[]}))
                    .await
                    .unwrap();
            }
        });
        let client = client();
        assert_eq!(
            post(
                &client,
                &router,
                "parent",
                &model_alias(&source, &model),
                history.clone()
            )
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
            200
        );
        let payload =
            json!({"threadId":"parent","targetProviderId":target,"targetModel":"target-model"});
        assert_eq!(
            router.portable_compaction("prepare", &payload).unwrap()["required"],
            false
        );
        assert_eq!(
            post(
                &client,
                &router,
                "parent",
                &model_alias(&target, "target-model"),
                history
            )
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
            200
        );
        upstream_task.await.unwrap();
        router.stop().await.unwrap();
    }

    #[tokio::test]
    async fn portable_compaction_incremental_history_preserves_opaque_state_and_identity() {
        let (router, _upstream, source, target, model) = fixture().await;
        let mut resolved = router
            .snapshot
            .read()
            .unwrap()
            .target_for_request(&model, Some(&source), None)
            .unwrap();
        let keys = ["thread-id:parent".into()];
        let payload =
            json!({"threadId":"parent","targetProviderId":target,"targetModel":"target-model"});
        for (native, history, expected) in [
            (false, None, false),
            (false, Some(true), true),
            (false, None, true),
            (true, Some(true), false),
            (false, None, false),
            (false, Some(false), false),
            (false, Some(true), true),
        ] {
            router
                .bindings
                .lock()
                .unwrap()
                .remember_resolved(&keys, &resolved, false, native, history);
            assert_eq!(
                router.bindings.lock().unwrap().routes[&keys[0]].portable,
                expected
            );
        }
        resolved.upstream_model = "changed-model".into();
        router
            .bindings
            .lock()
            .unwrap()
            .remember_resolved(&keys, &resolved, false, false, None);
        assert!(!router.bindings.lock().unwrap().routes[&keys[0]].portable);
        assert_eq!(
            router.portable_compaction("prepare", &payload).unwrap()["required"],
            true
        );
        let token = router.bindings.lock().unwrap().portable_tickets[&keys[0]]
            .token
            .clone();
        router
            .portable_compaction("cancel", &json!({"threadId":"parent","token":token}))
            .unwrap();
        assert_eq!(
            router
                .portable_compaction(
                    "prepare",
                    &json!({"threadId":"parent","targetProviderId":source,"targetModel":model})
                )
                .unwrap()["required"],
            false
        );
        router.bindings.lock().unwrap().remember_resolved(
            &keys,
            &resolved,
            false,
            false,
            Some(true),
        );
        router
            .bindings
            .lock()
            .unwrap()
            .routes
            .get_mut(&keys[0])
            .unwrap()
            .context_config = Some([1; 32]);
        router
            .bindings
            .lock()
            .unwrap()
            .remember_resolved(&keys, &resolved, false, false, None);
        assert!(!router.bindings.lock().unwrap().routes[&keys[0]].portable);
        router.stop().await.unwrap();
    }

    #[test]
    fn portable_compaction_binding_keys_isolate_session_only_children() {
        let mut request = HttpRequest {
            method: "POST".into(),
            path: "/responses".into(),
            headers: vec![("session-id".into(), "parent".into())],
            body: vec![],
            _body_budget_permit: None,
        };
        assert_eq!(
            request_binding_keys(&request),
            ["thread-id:parent", "session-id:parent"]
        );
        assert_eq!(
            portable_request_key(&request).as_deref(),
            Some("thread-id:parent")
        );
        for header in ["x-openai-subagent", "x-codex-parent-thread-id"] {
            request.headers.push((header.into(), "parent".into()));
            assert_eq!(request_binding_keys(&request), ["session-id:parent"]);
            assert_eq!(portable_request_key(&request), None);
            request.headers.pop();
        }
        request.headers.push(("thread-id".into(), "child".into()));
        assert_eq!(
            request_binding_keys(&request),
            ["thread-id:child", "session-id:parent"]
        );
        assert_eq!(
            portable_request_key(&request).as_deref(),
            Some("thread-id:child")
        );
    }

    #[tokio::test]
    async fn portable_compaction_session_only_round_trip_refreshes_source_binding() {
        let (router, upstream, source, target, model) = fixture().await;
        // Seed a stale shared fallback as an earlier request would have done.
        let mut resolved = router
            .snapshot
            .read()
            .unwrap()
            .target_for_request(&model, Some(&source), None)
            .unwrap();
        resolved.upstream_model = "old-model".into();
        router.bindings.lock().unwrap().remember_resolved(
            &["session-id:parent".into()],
            &resolved,
            false,
            false,
            None,
        );
        let old_generation = router.bindings.lock().unwrap().routes["session-id:parent"].generation;
        let upstream_task = tokio::spawn(async move {
            for stage in 0..3 {
                let (mut socket, _) = upstream.accept().await.unwrap();
                let request = read_http_request(&mut socket).await.unwrap();
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                assert_eq!(
                    body["model"],
                    if stage == 2 {
                        "target-model"
                    } else {
                        "provider-model"
                    }
                );
                if stage == 1 {
                    assert_eq!(body["input"][0]["encrypted_content"], "opaque");
                } else if stage == 2 {
                    assert_eq!(body["input"][0]["content"][0]["text"], "portable handoff");
                }
                write_json_response(&mut socket, 200, &json!({"id":"reply","status":"completed",
                    "output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"portable handoff"}]}]})).await.unwrap();
            }
        });
        let client = client();
        let send = |model: String, input: Value| {
            client
                .post(format!("{}/responses", router.endpoint.base_url))
                .bearer_auth(&router.endpoint.token)
                .header("session-id", "parent")
                .json(&json!({"model":model,"input":input,"stream":false}))
        };
        // No route metadata: this must still refresh the session model/generation.
        assert_eq!(
            send(
                model.clone(),
                json!([{"type":"compaction","encrypted_content":"opaque"}])
            )
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
            200
        );
        {
            let bindings = router.bindings.lock().unwrap();
            assert_eq!(bindings.routes["session-id:parent"].upstream_model, model);
            assert_ne!(
                bindings.routes["session-id:parent"].generation,
                old_generation
            );
            assert_eq!(bindings.routes["thread-id:parent"].upstream_model, model);
        }
        let payload =
            json!({"threadId":"parent","targetProviderId":target,"targetModel":"target-model"});
        let prepared = router.portable_compaction("prepare", &payload).unwrap();
        assert_eq!(prepared["required"], true);
        assert_eq!(prepared["sourceModel"], model);
        let handle = json!({"threadId":"parent","token":prepared["token"]});
        let compact = json!([{"type":"compaction","encrypted_content":"opaque"},{"type":"compaction_trigger"}]);
        assert_eq!(
            send(model.clone(), json!("blocked"))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            409
        );
        for header in ["x-openai-subagent", "x-codex-parent-thread-id"] {
            let response = send(model_alias(&target, "target-model"), compact.clone())
                .header(header, "parent")
                .send()
                .await
                .unwrap();
            assert_ne!(response.status().as_u16(), 200);
            assert_eq!(
                router.portable_compaction("status", &handle).unwrap()["state"],
                "prepared"
            );
        }
        let response = send(model_alias(&target, "target-model"), compact)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        let result: Value = response.json().await.unwrap();
        router.portable_compaction("confirm", &handle).unwrap();
        let response = send(
            model_alias(&target, "target-model"),
            result["output"].clone(),
        )
        .send()
        .await
        .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        // A subsequent normal turn advances the exact binding, invalidating old confirmation.
        assert!(router.portable_compaction("confirm", &handle).is_err());
        upstream_task.await.unwrap();
        router.stop().await.unwrap();
    }

    #[tokio::test]
    async fn portable_compaction_terminal_ticket_requires_explicit_cancel_for_native_recovery() {
        for state in ["failed", "expired"] {
            let (router, upstream, source, target, model) = fixture().await;
            bind_source(&router, &source, &model);
            let prepared = router
                .portable_compaction(
                    "prepare",
                    &json!({
                        "threadId":"parent","targetProviderId":target,"targetModel":"target-model"
                    }),
                )
                .unwrap();
            let handle = json!({"threadId":"parent","token":prepared["token"]});
            router
                .bindings
                .lock()
                .unwrap()
                .portable_tickets
                .get_mut("thread-id:parent")
                .unwrap()
                .state = state;
            let compact = json!([{"type":"compaction_trigger"}]);
            let response = post(
                &client(),
                &router,
                "parent",
                &model_alias(&source, &model),
                compact.clone(),
            )
            .send()
            .await
            .unwrap();
            assert_eq!(response.status().as_u16(), 409);
            assert_eq!(
                router.portable_compaction("status", &handle).unwrap()["state"],
                state
            );
            router.portable_compaction("cancel", &handle).unwrap();
            assert!(router.portable_compaction("status", &handle).is_err());
            let upstream_task = tokio::spawn(async move {
                let (mut socket, _) = upstream.accept().await.unwrap();
                let request = read_http_request(&mut socket).await.unwrap();
                assert_eq!(
                    incoming_header(&request, "authorization"),
                    Some("Bearer sk-upstream")
                );
                write_json_response(
                    &mut socket,
                    200,
                    &json!({"id":"native","status":"completed",
                    "output":[{"type":"compaction","encrypted_content":"native-ciphertext"}]}),
                )
                .await
                .unwrap();
            });
            let response = post(
                &client(),
                &router,
                "parent",
                &model_alias(&source, &model),
                compact,
            )
            .send()
            .await
            .unwrap();
            assert_eq!(response.status().as_u16(), 200);
            assert_eq!(
                response.json::<Value>().await.unwrap()["output"][0]["encrypted_content"],
                "native-ciphertext"
            );
            upstream_task.await.unwrap();
            router.stop().await.unwrap();
        }
    }

    #[tokio::test]
    async fn portable_compaction_running_cannot_cancel_and_new_native_compaction_invalidates_confirmation()
     {
        let (router, _upstream, source, target, model) = fixture().await;
        bind_source(&router, &source, &model);
        let payload =
            json!({"threadId":"parent","targetProviderId":target,"targetModel":"target-model"});
        let prepared = router.portable_compaction("prepare", &payload).unwrap();
        let handle = json!({"threadId":"parent","token":prepared["token"]});
        let route = Arc::clone(&router.snapshot.read().unwrap().routes[&source]);
        router
            .bindings
            .lock()
            .unwrap()
            .portable_tickets
            .get_mut("thread-id:parent")
            .unwrap()
            .state = "running";
        assert!(router.portable_compaction("cancel", &handle).is_err());
        let attempt = PortableAttempt {
            bindings: Arc::clone(&router.bindings),
            key: "thread-id:parent".into(),
            token: prepared["token"].as_str().unwrap().into(),
            source: route,
            model: model.clone(),
            completed: false,
        };
        drop(attempt);
        assert_eq!(
            router.portable_compaction("status", &handle).unwrap()["state"],
            "failed"
        );
        let prepared = router.portable_compaction("prepare", &payload).unwrap();
        let handle = json!({"threadId":"parent","token":prepared["token"]});
        router
            .bindings
            .lock()
            .unwrap()
            .portable_tickets
            .get_mut("thread-id:parent")
            .unwrap()
            .state = "completed";
        router.portable_compaction("confirm", &handle).unwrap();
        let resolved = router
            .snapshot
            .read()
            .unwrap()
            .target_for_request(&model, Some(&source), None)
            .unwrap();
        router.bindings.lock().unwrap().remember_resolved(
            &["thread-id:parent".into()],
            &resolved,
            false,
            true,
            Some(true),
        );
        assert!(router.portable_compaction("confirm", &handle).is_err());
        assert_eq!(
            router.portable_compaction("prepare", &payload).unwrap()["required"],
            true
        );
        router.stop().await.unwrap();
    }

    async fn fixture() -> (LocalRouter, TcpListener, String, String, String) {
        let upstream = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let (mut config, source, model) = super::super::tests::router_config(format!(
            "http://{}/v1",
            upstream.local_addr().unwrap()
        ));
        config.profiles[0].supports_remote_compaction = true;
        let mut target = config.profiles[0].clone();
        target.id = "route-b".into();
        target.name = "Target".into();
        target.api_key = "sk-target".into();
        let target_id = target.provider_id().to_string();
        config.profiles.push(target);
        config
            .selected_models_by_provider
            .insert(target_id.clone(), vec!["target-model".into()]);
        let router = LocalRouter::start(&config).await.unwrap();
        (router, upstream, source, target_id, model)
    }

    fn bind_source(router: &LocalRouter, provider: &str, model: &str) {
        let resolved = router
            .snapshot
            .read()
            .unwrap()
            .target_for_request(model, Some(provider), None)
            .unwrap();
        router.bindings.lock().unwrap().remember_resolved(
            &["thread-id:parent".into()],
            &resolved,
            false,
            false,
            None,
        );
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap()
    }

    fn post(
        client: &reqwest::Client,
        router: &LocalRouter,
        thread: &str,
        model: &str,
        input: Value,
    ) -> reqwest::RequestBuilder {
        client
            .post(format!("{}/responses", router.endpoint.base_url))
            .bearer_auth(&router.endpoint.token)
            .header("thread-id", thread)
            .json(&json!({"model":model,"input":input,"stream":false}))
    }

    #[tokio::test]
    async fn portable_compaction_source_ciphertext_becomes_confirmed_target_history() {
        let (router, upstream, source, target, model) = fixture().await;
        bind_source(&router, &source, &model);
        let prepare =
            json!({"threadId":"parent","targetProviderId":target,"targetModel":"target-model"});
        assert_eq!(router.portable_compaction("prepare", &json!({"threadId":"missing","targetProviderId":target,"targetModel":"target-model"})).unwrap()["required"], false);
        assert_eq!(
            router
                .portable_compaction(
                    "prepare",
                    &json!({"threadId":"parent","targetProviderId":source,"targetModel":model})
                )
                .unwrap()["required"],
            false
        );
        let client = client();
        let opaque = json!([{"type":"compaction","encrypted_content":"source-only-ciphertext"}]);
        let rejected = post(
            &client,
            &router,
            "parent",
            &model_alias(&target, "target-model"),
            opaque.clone(),
        )
        .send()
        .await
        .unwrap();
        assert_eq!(rejected.status().as_u16(), 400);
        assert_eq!(
            router.bindings.lock().unwrap().routes["thread-id:parent"].provider_id,
            source
        );
        let prepared = router.portable_compaction("prepare", &prepare).unwrap();
        let handle = json!({"threadId":"parent","token":prepared["token"]});
        assert_eq!(prepared["sourceModel"], model);
        assert!(router.portable_compaction("prepare", &prepare).is_err());
        assert!(
            router
                .portable_compaction(
                    "status",
                    &json!({"threadId":"child","token":prepared["token"]})
                )
                .is_err()
        );
        let blocked = post(
            &client,
            &router,
            "parent",
            &model_alias(&source, &model),
            json!("new turn"),
        )
        .send()
        .await
        .unwrap();
        assert_eq!(blocked.status().as_u16(), 409);
        // A child's concrete ID must override a shared parent session header.
        let child_source = router
            .snapshot
            .read()
            .unwrap()
            .target_for_request(&model, Some(&source), None)
            .unwrap();
        router.bindings.lock().unwrap().remember_resolved(
            &["thread-id:child".into()],
            &child_source,
            false,
            false,
            None,
        );
        let child = post(
            &client,
            &router,
            "child",
            &model_alias(&target, "target-model"),
            opaque.clone(),
        )
        .header("session-id", "parent")
        .header("x-codex-parent-thread-id", "parent")
        .send()
        .await
        .unwrap();
        assert_ne!(child.status().as_u16(), 200);
        assert_eq!(
            router.portable_compaction("status", &handle).unwrap()["state"],
            "prepared"
        );
        let upstream_task = tokio::spawn(async move {
            let (mut socket, _) = upstream.accept().await.unwrap();
            let request = read_http_request(&mut socket).await.unwrap();
            assert_eq!(request.path, "/v1/responses");
            assert_eq!(
                incoming_header(&request, "authorization"),
                Some("Bearer sk-upstream")
            );
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["model"], "provider-model");
            assert_eq!(
                body["input"][0]["encrypted_content"],
                "source-only-ciphertext"
            );
            assert!(
                body["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|item| item["type"] != "compaction_trigger")
            );
            write_json_response(&mut socket, 200, &json!({"id":"summary","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"portable handoff"}]}]})).await.unwrap();
            let (mut socket, _) = upstream.accept().await.unwrap();
            let request = read_http_request(&mut socket).await.unwrap();
            assert_eq!(
                incoming_header(&request, "authorization"),
                Some("Bearer sk-target")
            );
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["input"][0]["content"][0]["text"], "portable handoff");
            write_json_response(
                &mut socket,
                200,
                &json!({"id":"target","status":"completed","output":[]}),
            )
            .await
            .unwrap();
        });
        let mut compact = opaque.as_array().unwrap().clone();
        compact.push(json!({"type":"compaction_trigger"}));
        let response = post(
            &client,
            &router,
            "parent",
            &model_alias(&target, "target-model"),
            json!(compact),
        )
        .send()
        .await
        .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        let result = response.json::<Value>().await.unwrap();
        assert!(
            result["output"][0]["encrypted_content"]
                .as_str()
                .unwrap()
                .starts_with(CODEY_SUMMARY_PREFIX)
        );
        assert_eq!(
            router.portable_compaction("status", &handle).unwrap()["state"],
            "completed"
        );
        assert!(!router.bindings.lock().unwrap().routes["thread-id:parent"].portable);
        router.portable_compaction("confirm", &handle).unwrap();
        assert_eq!(
            router.portable_compaction("prepare", &prepare).unwrap()["required"],
            false
        );
        let response = post(
            &client,
            &router,
            "parent",
            &model_alias(&target, "target-model"),
            result["output"].clone(),
        )
        .send()
        .await
        .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        upstream_task.await.unwrap();
        router.stop().await.unwrap();
    }

    #[tokio::test]
    async fn portable_compaction_failure_expiry_cancel_and_identity_changes_do_not_confirm() {
        let (router, upstream, source, target, model) = fixture().await;
        bind_source(&router, &source, &model);
        let payload =
            json!({"threadId":"parent","targetProviderId":target,"targetModel":"target-model"});
        let prepare = router.portable_compaction("prepare", &payload).unwrap();
        let handle = json!({"threadId":"parent","token":prepare["token"]});
        router.portable_compaction("cancel", &handle).unwrap();
        assert!(router.portable_compaction("status", &handle).is_err());
        let prepare = router.portable_compaction("prepare", &payload).unwrap();
        let handle = json!({"threadId":"parent","token":prepare["token"]});
        router
            .bindings
            .lock()
            .unwrap()
            .portable_tickets
            .get_mut("thread-id:parent")
            .unwrap()
            .created = Instant::now() - PREPARE_TTL;
        assert_eq!(
            router.portable_compaction("status", &handle).unwrap()["state"],
            "expired"
        );
        let prepare = router.portable_compaction("prepare", &payload).unwrap();
        let handle = json!({"threadId":"parent","token":prepare["token"]});
        let upstream_task = tokio::spawn(async move {
            let (mut socket, _) = upstream.accept().await.unwrap();
            read_http_request(&mut socket).await.unwrap();
            write_json_response(&mut socket, 400, &json!({"error":{"message":"failed"}}))
                .await
                .unwrap();
        });
        let response = post(&client(), &router, "parent", &model_alias(&target, "target-model"), json!([{"type":"compaction","encrypted_content":"opaque"},{"type":"compaction_trigger"}])).send().await.unwrap();
        assert_eq!(response.status().as_u16(), 502);
        assert_eq!(
            router.portable_compaction("status", &handle).unwrap()["state"],
            "failed"
        );
        assert!(router.portable_compaction("confirm", &handle).is_err());
        assert_eq!(
            router.bindings.lock().unwrap().routes["thread-id:parent"].provider_id,
            source
        );
        upstream_task.await.unwrap();
        let prepared = router.portable_compaction("prepare", &payload).unwrap();
        let handle = json!({"threadId":"parent","token":prepared["token"]});
        router
            .bindings
            .lock()
            .unwrap()
            .routes
            .get_mut("thread-id:parent")
            .unwrap()
            .context_config = Some([1; 32]);
        let response = post(
            &client(),
            &router,
            "parent",
            &model_alias(&target, "target-model"),
            json!([{"type":"compaction_trigger"}]),
        )
        .send()
        .await
        .unwrap();
        assert_eq!(response.status().as_u16(), 409);
        assert_eq!(
            router.portable_compaction("status", &handle).unwrap()["state"],
            "failed"
        );
        router.stop().await.unwrap();
    }
}
