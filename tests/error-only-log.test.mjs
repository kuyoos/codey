import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const root = new URL("../", import.meta.url);

async function source(path) {
  return readFile(new URL(path, root), "utf8");
}

test("startup renders a loading state until config and provider are ready", async () => {
  const [app, notice] = await Promise.all([
    source("src/App.tsx"),
    source("src/useAppNotice.tsx"),
  ]);

  assert.match(app, /if \(!config \|\| !provider\)/);
  assert.match(app, /正在载入 Codey/);
  assert.match(
    app,
    /<p>\s*<NoticeLoadingText controller=\{noticeController\} \/>\s*<\/p>/,
  );
  assert.match(notice, /return <>\{notice\.text\}<\/>/);
});

test("desktop and FastCtx entrypoints keep crash logging wired", async () => {
  const [errorLog, main, fastctx, startupPatch] = await Promise.all([
    source("backend/src/error_log.rs"),
    source("backend/src/main.rs"),
    source("backend/src/bin/codey-fastctx.rs"),
    source("backend/src/renderer_asset_patch.js"),
  ]);

  assert.match(errorLog, /codey-errors\.log/);
  assert.match(main, /install_crash_log_hook\("codey"/);
  assert.match(main, /record_process_failure/);
  assert.match(fastctx, /install_crash_log_hook\("fastctx"/);
  assert.match(startupPatch, /recordCodeyPatchFailure/);
});
