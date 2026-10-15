import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import ts from "typescript";
import { loadTypeScriptModule } from "./helpers/load-typescript-module.mjs";

for (const operation of ["repair", "restart"]) {
test(`starting ${operation} discards older status flights and queued injection refreshes`, async () => {
  const values = [];
  const requests = [];
  const react = {
    useCallback: (callback) => callback,
    useEffect: () => {},
    useRef: (current) => ({ current }),
    useState: (initial) => {
      const index = values.push(initial) - 1;
      return [initial, (next) => {
        values[index] = typeof next === "function" ? next(values[index]) : next;
      }];
    },
  };
  const scheduler = await loadTypeScriptModule(new URL("../src/runtimeStatusPollScheduler.ts", import.meta.url));
  const dependencies = {
    react,
    "./api": { invoke: () => new Promise((resolve) => requests.push(resolve)) },
    "./appUtils": { withTimeout: (promise) => promise },
    "./runtimeStatusPollScheduler": {
      ...scheduler,
      createStatusPollScheduler: (request) => scheduler.createStatusPollScheduler(request, {
        now: Date.now, setTimeout, clearTimeout,
      }),
    },
    "./runtimeStatusSnapshot": await loadTypeScriptModule(new URL("../src/runtimeStatusSnapshot.ts", import.meta.url)),
  };
  const source = await readFile(new URL("../src/useRuntimeStatus.ts", import.meta.url), "utf8");
  const compiled = ts.transpileModule(source, {
    compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 },
  }).outputText;
  const exports = {};
  new Function("require", "exports", compiled)((name) => {
    assert.ok(name in dependencies, name);
    return dependencies[name];
  }, exports);
  const hook = exports.useRuntimeStatus({ active: true, embedded: false });
  const oldFlight = hook.refreshStatus();
  const queuedRefresh = hook.refreshStatusForLoad();
  if (operation === "repair") {
    hook.markRestartInProgress();
  } else {
    const appSource = await readFile(new URL("../src/App.tsx", import.meta.url), "utf8");
    const tree = ts.createSourceFile("App.tsx", appSource, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
    let handler;
    const visit = (node) => {
      if (ts.isFunctionDeclaration(node) && node.name?.text === "restartCodex") handler = node.getText(tree);
      ts.forEachChild(node, visit);
    };
    visit(tree);
    assert.ok(handler);
    const calls = [];
    const context = {
      config: {}, dirty: false, remoteDirty: false, remoteControlRef: { current: null },
      runOperation: async (_name, run) => run(),
      setNotice: () => {},
      withTimeout: (promise) => promise,
      invoke: async (command) => { calls.push(command); },
      setRestartStatusError: () => assert.fail("restart unexpectedly failed"),
      markRestartInProgress: hook.markRestartInProgress,
    };
    const compiledHandler = ts.transpileModule(handler, {}).outputText;
    const restart = new Function(...Object.keys(context), `${compiledHandler}; return restartCodex;`)(...Object.values(context));
    await restart();
    assert.deepEqual(calls, ["restart_codey"]);
  }
  assert.equal(values[0].restartInProgress, true);
  const freshFlight = hook.refreshStatus();
  assert.equal(requests.length, 2);
  requests[0]({ running: true, restartInProgress: false });
  await Promise.all([oldFlight, queuedRefresh]);
  assert.equal(values[0].restartInProgress, true, "old response must not end the repair");
  assert.equal(requests.length, 2, "stale queued refresh must not start");
  requests[1]({ running: true, restartInProgress: false, startupError: "修复失败，已恢复启动" });
  await freshFlight;
  assert.equal(values[0].restartInProgress, false);
  assert.equal(values[0].startupError, "修复失败，已恢复启动");
});
}
