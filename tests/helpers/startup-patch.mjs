import assert from "node:assert/strict";

import { readSource } from "./read-source.mjs";

// Loads backend/src/renderer_asset_patch.js with the same options prelude the
// CDP document script uses (see renderer_asset_patch_document_script), so tests
// can eval the result the way the page receives it.
export async function loadStartupPatchTemplate({ disablePet = false } = {}) {
  const template = await readSource("backend/src/renderer_asset_patch.js");
  assert.ok(template, "renderer asset patch template should be readable");
  return `globalThis.__CODEY_RENDERER_PATCH_OPTIONS__ = ${JSON.stringify({ disablePet })};\n${template}`;
}

// Slices spawn_codex in backend/src/launcher/process.rs into its per-platform
// `#[cfg]` blocks so platform contracts can be asserted from any host.
export async function loadSpawnCodexSections() {
  const [launcher, launcherPlatform, startupPatch] = await Promise.all([
    readSource("backend/src/launcher/process.rs"),
    readSource("backend/src/launcher/platform.rs"),
    readSource("backend/src/codex_startup_patch.rs"),
  ]);
  const spawnStart = launcher.indexOf("async fn spawn_codex");
  const macosStart = launcher.indexOf('#[cfg(target_os = "macos")]\n    {', spawnStart);
  const otherStart = launcher.indexOf('#[cfg(not(any(windows, target_os = "macos")))]', macosStart);
  assert.ok(spawnStart >= 0 && macosStart > spawnStart && otherStart > macosStart);
  const windowsSpawn = launcher.slice(
    launcher.indexOf("#[cfg(windows)]\n    {", spawnStart),
    launcher.indexOf('#[cfg(target_os = "macos")]', spawnStart),
  );
  const macosSpawn = launcher.slice(macosStart, otherStart);
  const cleanup = launcherPlatform.slice(
    launcherPlatform.indexOf("async fn stop_windows_spawned_codex"),
    launcherPlatform.indexOf(
      '#[cfg(target_os = "macos")]\npub(super) fn build_fresh_macos_open_command',
    ),
  );
  return { cleanup, launcher, launcherPlatform, macosSpawn, startupPatch, windowsSpawn };
}
