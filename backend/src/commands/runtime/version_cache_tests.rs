use super::*;
use std::sync::atomic::AtomicUsize;

async fn settled(state: &AppState) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if state
                .codex_app_version_cache
                .lock()
                .await
                .as_ref()
                .is_some_and(|c| !c.refreshing)
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("version lookup did not finish");
}

async fn read(state: &Arc<AppState>, path: &str, lookup: &Arc<VersionLookup>) -> String {
    tokio::time::timeout(
        Duration::from_millis(100),
        cached_codex_app_version(state, None, path.into(), Arc::clone(lookup)),
    )
    .await
    .expect("status waited for filesystem lookup")
}

#[tokio::test]
async fn slow_lookup_returns_immediately_and_deduplicates_status_requests() {
    let state = Arc::new(AppState::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let (started_tx, started_rx) = oneshot::channel();
    let started = std::sync::Mutex::new(Some(started_tx));
    let (release, wait) = std::sync::mpsc::channel();
    let wait = std::sync::Mutex::new(wait);
    let lookup: Arc<VersionLookup> = Arc::new({
        let calls = Arc::clone(&calls);
        move |_, _| {
            calls.fetch_add(1, Ordering::SeqCst);
            started.lock().unwrap().take().unwrap().send(()).unwrap();
            wait.lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(2))
                .unwrap();
            "26.1".into()
        }
    });
    assert_eq!(read(&state, "a", &lookup).await, "");
    started_rx.await.unwrap();
    for _ in 0..20 {
        assert_eq!(read(&state, "a", &lookup).await, "");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    release.send(()).unwrap();
    settled(&state).await;
    assert_eq!(read(&state, "a", &lookup).await, "26.1");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn stale_cache_is_available_while_refreshing_and_failures_have_ttl() {
    let state = Arc::new(AppState::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let lookup: Arc<VersionLookup> = Arc::new({
        let calls = Arc::clone(&calls);
        move |_, _| {
            calls.fetch_add(1, Ordering::SeqCst);
            String::new()
        }
    });
    *state.codex_app_version_cache.lock().await = Some(CodexAppVersionCache {
        runtime_app_path: None,
        configured_app_path: "a".into(),
        version: "old".into(),
        checked_at: Some(Instant::now() - CODEX_APP_VERSION_CACHE_TTL),
        generation: 0,
        refreshing: false,
    });
    assert_eq!(read(&state, "a", &lookup).await, "old");
    settled(&state).await;
    for _ in 0..20 {
        assert_eq!(read(&state, "a", &lookup).await, "");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn path_changes_coalesce_and_discard_old_results_even_when_switching_back() {
    let state = Arc::new(AppState::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let (started_tx, started_rx) = oneshot::channel();
    let started = std::sync::Mutex::new(Some(started_tx));
    let (release, wait) = std::sync::mpsc::channel();
    let wait = std::sync::Mutex::new(wait);
    let lookup: Arc<VersionLookup> = Arc::new({
        let calls = Arc::clone(&calls);
        move |_, path| {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                started.lock().unwrap().take().unwrap().send(()).unwrap();
                wait.lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap();
                "obsolete".into()
            } else {
                format!("new-{path}")
            }
        }
    });
    assert_eq!(read(&state, "a", &lookup).await, "");
    started_rx.await.unwrap();
    for path in ["b", "c", "a"] {
        assert_eq!(read(&state, path, &lookup).await, "");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    release.send(()).unwrap();
    settled(&state).await;
    assert_eq!(read(&state, "a", &lookup).await, "new-a");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn shutdown_does_not_start_lookup() {
    let state = Arc::new(AppState::default());
    state.shutting_down.store(true, Ordering::Release);
    let lookup: Arc<VersionLookup> = Arc::new(|_, _| panic!("lookup during shutdown"));
    assert_eq!(read(&state, "a", &lookup).await, "");
    settled(&state).await;
}
