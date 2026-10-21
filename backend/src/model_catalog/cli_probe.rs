use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
pub(super) struct RuntimeCatalogSource {
    path: PathBuf,
    len: u64,
    modified: std::time::SystemTime,
}

#[cfg(not(test))]
pub(super) fn catalog_source_for_app(codex_app_path: &str) -> Option<RuntimeCatalogSource> {
    runtime_catalog_source(&codex_cli_candidates_for(codex_app_path))
}

// Tests use a stable virtual runtime for existing catalog fixtures and real
// temporary executables for installation-switch and replacement coverage.
#[cfg(test)]
pub(super) fn catalog_source_for_app(codex_app_path: &str) -> Option<RuntimeCatalogSource> {
    if codex_app_path.is_empty() {
        Some(RuntimeCatalogSource {
            path: PathBuf::from("test-codex-runtime"),
            len: 1,
            modified: std::time::SystemTime::UNIX_EPOCH,
        })
    } else {
        runtime_catalog_source(&[PathBuf::from(codex_app_path)])
    }
}

pub(super) fn runtime_catalog_source(candidates: &[PathBuf]) -> Option<RuntimeCatalogSource> {
    let path = candidates.first()?.canonicalize().ok()?;
    let metadata = path.metadata().ok()?;
    Some(RuntimeCatalogSource {
        path,
        len: metadata.len(),
        modified: metadata.modified().ok()?,
    })
}

pub(super) fn snapshot_matches_runtime_source(
    snapshot: &Value,
    source: Option<&RuntimeCatalogSource>,
) -> bool {
    let recorded = snapshot
        .get("codey_runtime_source")
        .and_then(|value| serde_json::from_value::<RuntimeCatalogSource>(value.clone()).ok());
    source.is_some() && recorded.as_ref() == source
}

/// Only the selected application's runtime determines catalog freshness.
pub(super) fn codex_cli_stamp_for(candidates: &[PathBuf]) -> Option<std::time::SystemTime> {
    candidates
        .iter()
        .filter_map(|path| {
            path.metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
        })
        .max()
}

/// The snapshot is missing or predates the local Codex CLI. A missing snapshot
/// counts as stale because a Codex upgrade that arrives without a
/// `models_cache.json` has no other way to produce one, while an existing
/// instruction-bearing source would otherwise suppress the capture.
#[cfg(not(test))]
pub(super) fn runtime_snapshot_is_stale(home: &Path, codex_app_path: &str) -> bool {
    let snapshot = home.join(DEBUG_CATALOG_RELATIVE_PATH);
    let Some(value) = read_catalog_value(&snapshot) else {
        return true;
    };
    let candidates = codex_cli_candidates_for(codex_app_path);
    if value.get("codey_account_snapshot").and_then(Value::as_bool) != Some(true)
        || !snapshot_matches_runtime_source(&value, runtime_catalog_source(&candidates).as_ref())
    {
        return true;
    }
    let snapshot_mtime = fs::metadata(home.join(DEBUG_CATALOG_RELATIVE_PATH))
        .and_then(|metadata| metadata.modified())
        .ok();
    runtime_snapshot_is_stale_at(snapshot_mtime, codex_cli_stamp_for(&candidates))
}

pub(super) fn runtime_snapshot_is_stale_at(
    snapshot_mtime: Option<std::time::SystemTime>,
    cli_stamp: Option<std::time::SystemTime>,
) -> bool {
    let Some(cli_stamp) = cli_stamp else {
        return false;
    };
    snapshot_mtime.is_none_or(|snapshot_mtime| cli_stamp > snapshot_mtime)
}

/// Unit tests never run the Codex CLI installed on the build machine, and the
/// snapshot capture itself is stubbed there. Staleness stays reachable through
/// `runtime_snapshot_is_stale_at` so the decision logic can still be tested.
#[cfg(test)]
pub(super) fn runtime_snapshot_is_stale(_home: &Path, _codex_app_path: &str) -> bool {
    false
}

#[cfg(not(test))]
pub(super) fn debug_model_entries(home: &Path, candidates: &[PathBuf]) -> Option<Vec<Value>> {
    for cli in candidates {
        for bundled in [false, true] {
            let Some(output) = run_codex_debug_models(cli, home, bundled) else {
                continue;
            };
            let Ok(value) = serde_json::from_slice::<Value>(&output) else {
                continue;
            };
            let models = official_models_from_value(&value);
            if snapshot_has_runtime_models(&models) {
                return Some(models);
            }
        }
    }
    None
}

/// Resolve the desktop application once and use the same runtime preparation as
/// launch. In particular, a protected Store package gets its own verified staged
/// copy; unrelated caches, installations and PATH executables are never used.
#[cfg(not(test))]
pub(super) fn codex_cli_candidates_for(codex_app_path: &str) -> Vec<PathBuf> {
    selected_app_cli_candidates(
        codex_app_path,
        codey_runtime_core::app_paths::resolve_codex_app_dir,
        |app_dir| {
            codey_runtime_core::app_paths::validate_codex_app_dir(app_dir).ok()?;
            #[cfg(windows)]
            {
                crate::launcher::windows_cli_wrapper_target(app_dir).ok()
            }
            #[cfg(not(windows))]
            {
                codey_runtime_core::app_paths::codex_runtime_executable(app_dir)
            }
        },
    )
}

fn selected_app_cli_candidates(
    configured: &str,
    resolve: impl FnOnce(Option<&Path>) -> Option<PathBuf>,
    prepare_runtime: impl FnOnce(&Path) -> Option<PathBuf>,
) -> Vec<PathBuf> {
    let configured = configured.trim();
    let app_path = (!configured.is_empty()).then(|| Path::new(configured));
    resolve(app_path)
        .and_then(|app_dir| prepare_runtime(&app_dir))
        .filter(|runtime| runtime.is_file())
        .into_iter()
        .collect()
}

#[cfg(test)]
mod selection_tests {
    use super::*;

    #[test]
    fn snapshot_source_requires_the_same_runtime_even_when_switching_to_an_older_build() {
        let current = RuntimeCatalogSource {
            path: PathBuf::from("current/codex"),
            len: 100,
            modified: std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(20),
        };
        let snapshot = json!({"codey_runtime_source": current});
        assert!(snapshot_matches_runtime_source(&snapshot, Some(&current)));
        let older = RuntimeCatalogSource {
            path: PathBuf::from("older/codex"),
            modified: std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10),
            ..current.clone()
        };
        assert!(!snapshot_matches_runtime_source(&snapshot, Some(&older)));
        let replaced = RuntimeCatalogSource {
            len: 200,
            ..current.clone()
        };
        assert!(!snapshot_matches_runtime_source(&snapshot, Some(&replaced)));
        assert!(!snapshot_matches_runtime_source(&snapshot, None));
        assert!(!snapshot_matches_runtime_source(&json!({}), Some(&current)));
        assert!(!snapshot_matches_runtime_source(
            &json!({"codey_runtime_source": "invalid"}),
            Some(&current),
        ));
    }

    #[test]
    fn snapshot_sync_claim_distinguishes_installations_with_equal_timestamps() {
        let first = RuntimeCatalogSource {
            path: PathBuf::from("first/codex"),
            len: 100,
            modified: std::time::SystemTime::UNIX_EPOCH,
        };
        let second = RuntimeCatalogSource {
            path: PathBuf::from("second/codex"),
            ..first.clone()
        };
        let attempted = std::sync::Mutex::new(None);
        assert!(claim_snapshot_sync_slot(&attempted, Some(first.clone())));
        assert!(!claim_snapshot_sync_slot(&attempted, Some(first)));
        assert!(claim_snapshot_sync_slot(&attempted, Some(second.clone())));
        release_snapshot_sync_slot(&attempted, Some(second.clone()));
        assert!(claim_snapshot_sync_slot(&attempted, Some(second)));
    }

    #[test]
    fn runtime_source_records_the_canonical_executable_and_detects_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = temp.path().join("codex");
        fs::write(&runtime, "first").unwrap();
        let first = runtime_catalog_source(std::slice::from_ref(&runtime)).unwrap();
        assert_eq!(first.path, runtime.canonicalize().unwrap());
        fs::write(&runtime, "replacement").unwrap();
        let replaced = runtime_catalog_source(&[runtime]).unwrap();
        assert_ne!(first, replaced);
        assert!(runtime_catalog_source(&[]).is_none());
    }

    #[test]
    fn explicit_application_uses_only_its_prepared_runtime() {
        let temp = tempfile::tempdir().unwrap();
        let app_dir = temp.path().join("selected-app");
        let staged = temp.path().join("verified-staged-codex.exe");
        fs::write(&staged, "selected runtime").unwrap();
        let candidates = selected_app_cli_candidates(
            "  selected.exe  ",
            |selected| {
                assert_eq!(selected, Some(Path::new("selected.exe")));
                Some(app_dir.clone())
            },
            |selected| {
                assert_eq!(selected, app_dir);
                Some(staged.clone())
            },
        );
        assert_eq!(candidates, vec![staged]);
    }

    #[test]
    fn invalid_explicit_application_does_not_discover_another_installation() {
        let candidates = selected_app_cli_candidates(
            "missing.exe",
            |selected| {
                assert_eq!(selected, Some(Path::new("missing.exe")));
                None
            },
            |_| panic!("an unresolved application must not prepare a runtime"),
        );
        assert!(candidates.is_empty());
    }

    #[test]
    fn unavailable_selected_runtime_does_not_fall_back() {
        let temp = tempfile::tempdir().unwrap();
        for runtime in [None, Some(temp.path().join("missing-codex.exe"))] {
            let candidates = selected_app_cli_candidates(
                "selected-app",
                |_| Some(temp.path().to_path_buf()),
                |_| runtime,
            );
            assert!(candidates.is_empty());
        }
    }

    #[test]
    fn automatic_selection_also_uses_only_the_resolved_application() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = temp.path().join("codex");
        fs::write(&runtime, "bundled runtime").unwrap();
        let candidates = selected_app_cli_candidates(
            " \n ",
            |selected| {
                assert!(selected.is_none());
                Some(temp.path().to_path_buf())
            },
            |_| Some(runtime.clone()),
        );
        assert_eq!(candidates, vec![runtime]);
    }
}

#[cfg(not(test))]
const DEBUG_MODELS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[cfg(not(test))]
fn run_codex_debug_models(cli: &Path, home: &Path, bundled: bool) -> Option<Vec<u8>> {
    use std::io::Read;
    use std::process::{Command, Stdio};

    let mut command = Command::new(cli);
    command.args(["debug", "models"]);
    if bundled {
        command.arg("--bundled");
    }
    command
        .env("CODEX_HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(codey_runtime_core::windows_create_no_window());
    }
    let mut child = command.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    // The catalog is larger than the pipe buffer, so it must be drained while
    // the deadline is being polled.
    let reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        stdout.read_to_end(&mut buffer).ok().map(|_| buffer)
    });
    let deadline = std::time::Instant::now() + DEBUG_MODELS_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = reader.join().ok().flatten();
                return status.success().then_some(()).and(output);
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return None;
            }
        }
    }
}
