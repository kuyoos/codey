import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createInterface } from "node:readline";
import test from "node:test";

// Opt in with CODEY_TEST_NATIVE_CLI and a freshly built CODEY_TEST_BINARY.
// All app-server state and model requests stay in a temporary home and localhost.
test("native restart restores confirmed choices and legacy turn models", {
  skip: !process.env.CODEY_TEST_NATIVE_CLI || !process.env.CODEY_TEST_BINARY,
  timeout: 120_000,
}, async () => {
  const home = await mkdtemp(join(tmpdir(), "codey-model-restart-"));
  const native = resolve(process.env.CODEY_TEST_NATIVE_CLI);
  const binary = resolve(process.env.CODEY_TEST_BINARY);
  const env = { PATH: process.env.PATH, HOME: process.env.HOME, CODEX_HOME: home };
  const server = createServer((_request, response) => {
    response.writeHead(400, { "Content-Type": "application/json" });
    response.end(JSON.stringify({ error: { message: "Synthetic model persistence test" } }));
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const clients = [];
  try {
    const dump = spawnSync(native, ["debug", "models", "--bundled"], {
      env, encoding: "utf8", timeout: 20_000, maxBuffer: 32 * 1024 * 1024,
    });
    assert.equal(dump.status, 0, dump.stderr);
    const bundled = JSON.parse(dump.stdout);
    const source = (Array.isArray(bundled) ? bundled : bundled.models)
      .find((model) => model.slug === "gpt-6-astra");
    assert.ok(source, "native bundled catalog must provide a runtime model template");
    const defaultModel = "probe/model-a";
    const selectedModel = "probe/model-b";
    const catalogPath = join(home, "catalog.json");
    await writeFile(catalogPath, JSON.stringify({ models: [defaultModel, selectedModel].map((slug) => ({
      ...source, slug, display_name: slug, prefer_websockets: false,
    })) }));
    const overrides = [
      `model=${JSON.stringify(defaultModel)}`,
      'model_reasoning_effort="medium"',
      'model_provider="codey_router"',
      `model_catalog_json=${JSON.stringify(catalogPath)}`,
      `model_providers.codey_router={name="probe",base_url="http://127.0.0.1:${server.address().port}/v1",wire_api="responses",requires_openai_auth=false,request_max_retries=0,stream_max_retries=0}`,
    ];
    const start = async (wrapped) => {
      const child = spawn(wrapped ? binary : native, wrapped ? ["app-server"]
        : ["app-server", ...overrides.flatMap((value) => ["-c", value])], {
        env: wrapped ? { ...env, CODEY_CODEX_CLI_WRAPPER_TARGET: native,
          CODEY_CODEX_CLI_WRAPPER_OVERRIDES: JSON.stringify(overrides) } : env,
        stdio: ["pipe", "pipe", "pipe"],
      });
      const messages = [];
      let stderr = "";
      child.stderr.on("data", (chunk) => { stderr = (stderr + chunk).slice(-8000); });
      const reader = createInterface({ input: child.stdout });
      reader.on("line", (line) => messages.push(JSON.parse(line)));
      const waitFor = async (predicate) => {
        const deadline = Date.now() + 15_000;
        while (Date.now() < deadline) {
          const message = messages.find(predicate);
          if (message) return message;
          assert.equal(child.exitCode, null, stderr);
          await new Promise((done) => setTimeout(done, 10));
        }
        assert.fail(`Native app-server response timeout: ${stderr}`);
      };
      let id = 0;
      const call = async (method, params) => {
        const requestId = ++id;
        child.stdin.write(`${JSON.stringify({ id: requestId, method, params })}\n`);
        const response = await waitFor((message) => message.id === requestId);
        assert.equal(response.error, undefined, JSON.stringify(response.error));
        return response.result;
      };
      const client = { call, waitFor, child, async stop() {
        if (child.exitCode !== null || child.signalCode !== null) return;
        const exited = once(child, "close");
        child.stdin.end();
        const timer = setTimeout(() => child.kill("SIGTERM"), 5000);
        await exited;
        clearTimeout(timer);
      } };
      clients.push(client);
      await call("initialize", { clientInfo: { name: "codey-test", version: "1" },
        capabilities: { experimentalApi: true } });
      child.stdin.write('{"method":"initialized"}\n');
      return client;
    };
    const first = await start(true);
    const createHistory = async (model, effort = "medium") => {
      const result = await first.call("thread/start", {
        model, cwd: home, approvalPolicy: "never", sandbox: "read-only",
        config: { model_reasoning_effort: effort },
      });
      const threadId = result.thread.id;
      const { turn } = await first.call("turn/start", {
        threadId, input: [{ type: "text", text: "Synthetic persistence test", text_elements: [] }],
      });
      await first.waitFor((message) => message.method === "turn/completed"
        && message.params?.threadId === threadId && message.params?.turn?.id === turn.id);
      return threadId;
    };
    const selectedThread = await createHistory(defaultModel);
    await first.call("thread/settings/update", { threadId: selectedThread, model: selectedModel });
    await first.waitFor((message) => message.method === "thread/settings/updated"
      && message.params?.threadId === selectedThread
      && message.params?.threadSettings?.model === selectedModel);
    await first.call("thread/settings/update", { threadId: selectedThread, effort: "high" });
    await first.waitFor((message) => message.method === "thread/settings/updated"
      && message.params?.threadId === selectedThread && message.params?.threadSettings?.effort === "high");
    assert.equal(JSON.parse(await readFile(join(home, "codey-session-models", `${selectedThread}.json`))).model,
      selectedModel, "successful unsent selection must be on disk before its notification is forwarded");
    assert.equal(JSON.parse(await readFile(join(home, "codey-session-models", `${selectedThread}.json`))).reasoning_effort,
      "high", "effort-only successful changes must be persisted before forwarding");
    await first.call("thread/settings/update", { threadId: selectedThread, model: null, effort: null });
    const unchanged = await first.call("thread/resume", { threadId: selectedThread, model: null,
      config: { model: defaultModel } });
    assert.equal(unchanged.thread.model, selectedModel, "null model updates preserve the current selection");
    assert.equal(unchanged.reasoningEffort, "high", "null effort updates preserve the current selection");
    await first.call("thread/unsubscribe", { threadId: selectedThread });
    const reopened = await first.call("thread/resume", { threadId: selectedThread, model: null,
      config: { model: defaultModel, model_reasoning_effort: "medium" } });
    assert.equal(reopened.thread.model, selectedModel,
    "closing and reopening a session also restores its confirmed choice");
    assert.equal(reopened.reasoningEffort, "high", "closing and reopening retains the unsent effort change");
    const legacyThread = await createHistory(selectedModel, "low");
    await first.stop();
    await rm(join(home, "codey-session-models", `${legacyThread}.json`), { force: true });

    // Reproduce the native reset and stale database before testing our cold restore.
    const unwrapped = await start(false);
    for (const threadId of [selectedThread, legacyThread]) {
      const result = await unwrapped.call("thread/resume", { threadId, model: null, config: { model: defaultModel } });
      assert.equal(result.thread.model, defaultModel);
    }
    await unwrapped.stop();

    const restarted = await start(true);
    for (const threadId of [selectedThread, legacyThread]) {
      const result = await restarted.call("thread/resume", { threadId, model: null,
        config: { model: defaultModel, model_reasoning_effort: "medium" } });
      assert.equal(result.model, selectedModel);
      assert.equal(result.thread.model, selectedModel, "selector reads nested thread.model");
      assert.equal(result.reasoningEffort, threadId === selectedThread ? "high" : "low",
        "restore effort with its model even when UI copies the global default");
    }
    const fresh = await restarted.call("thread/start", { cwd: home, approvalPolicy: "never", sandbox: "read-only" });
    assert.equal(fresh.thread.model, defaultModel, "restoring a session must not change the default");
    assert.equal(fresh.reasoningEffort, "medium", "restoring effort must not change new-session defaults");
    await restarted.call("thread/settings/update", { threadId: selectedThread, effort: "xhigh" });
    assert.equal((await restarted.call("thread/resume", { threadId: legacyThread, model: null })).reasoningEffort,
      "low", "effort changes must stay isolated by session");
    await restarted.call("thread/settings/update", { threadId: selectedThread, model: defaultModel });
    assert.equal((await restarted.call("thread/resume", { threadId: selectedThread, model: null })).thread.model,
      defaultModel, "same-process resume preserves the current unsent selection");
    assert.equal((await restarted.call("thread/resume", { threadId: legacyThread, model: null })).thread.model,
      selectedModel, "changing one session must not affect another");
  } finally {
    for (const client of clients) await client.stop();
    await new Promise((done) => server.close(done));
    await rm(home, { recursive: true, force: true });
  }
});
