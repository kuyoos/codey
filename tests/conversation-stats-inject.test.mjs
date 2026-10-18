import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";

import { flushTimers } from "./helpers/flush.mjs";
import { FakeElementCore } from "./helpers/fake-element.mjs";

const source = readFileSync(
  new URL("../public/conversation-stats-inject.js", import.meta.url),
  "utf8",
);

class FakeElement extends FakeElementCore {
  constructor(tagName = "div", { visible = true } = {}) {
    super(tagName);
    this.visible = visible;
  }

  matches(selector) {
    if (selector === ":popover-open") {
      if (typeof this.showPopover !== "function") throw new SyntaxError("Unsupported popover selector");
      return this.popoverOpen === true;
    }
    const match = selector.match(/^(\w+)?\.([\w-]+)$/);
    if (match) return (!match[1] || this.tagName === match[1].toUpperCase())
      && String(this.className || "").split(/\s+/).includes(match[2]);
    return super.matches(selector);
  }

  focus() {
    if (this.ownerDocument) this.ownerDocument.activeElement = this;
  }

  getClientRects() {
    return this.visible ? [{ bottom: 10, left: 0, right: 10, top: 0 }] : [];
  }

  getBoundingClientRect() {
    return this.visible
      ? { bottom: 160, height: 60, left: 0, right: 600, top: 100, width: 600 }
      : { bottom: 0, height: 0, left: 0, right: 0, top: 0, width: 0 };
  }
}

const createEnvironment = (options = {}) => {
  const documentElement = new FakeElement("html");
  const body = new FakeElement("body");
  const scope = new FakeElement("div");
  const anchor = new FakeElement("div", { visible: options.anchorVisible !== false });
  anchor.setAttribute(
    "data-above-composer-conversation-id",
    options.conversationId ?? "conversation-1",
  );
  const textarea = new FakeElement("textarea", {
    visible: options.inputVisible !== false,
  });
  documentElement.appendChild(body);
  body.appendChild(scope);
  if (options.anchor !== false) {
    scope.appendChild(anchor);
    scope.appendChild(textarea);
  }

  const findById = (root, id) => {
    if (root.id === id || root.getAttribute?.("id") === id) return root;
    for (const child of root.children || []) {
      const found = findById(child, id);
      if (found) return found;
    }
    return null;
  };

  let latestObserver = null;
  class FakeMutationObserver {
    constructor(callback) {
      this.callback = callback;
      this.observed = false;
      latestObserver = this;
    }

    observe() {
      this.observed = true;
    }

    disconnect() {
      this.observed = false;
    }
  }

  const document = {
    body,
    documentElement,
    visibilityState: "visible",
    activeElement: body,
    createElement: (tagName) => Object.assign(new FakeElement(tagName), { ownerDocument: document }, options.popover ? {
      showPopover() { this.popoverOpen = true; },
      hidePopover() { this.popoverOpen = false; },
    } : {}),
    getElementById: (id) => findById(documentElement, id),
    querySelector: (selector) => documentElement.querySelector(selector),
    querySelectorAll: (selector) => documentElement.querySelectorAll(selector),
    listeners: new Map(),
    addEventListener: FakeElementCore.prototype.addEventListener,
    removeEventListener: FakeElementCore.prototype.removeEventListener,
    dispatchEvent: FakeElementCore.prototype.dispatchEvent,
  };
  for (const element of [documentElement, body, scope, anchor, textarea]) element.ownerDocument = document;

  const location = {
    pathname: options.pathname ?? "/",
    search: options.search ?? "",
  };

  const statsBySession = new Map(Object.entries(options.statsBySession ?? {}));
  const calls = [];
  const bridge = async (path, payload) => {
    calls.push({ path, payload });
    if (path === "/session/stats") {
      if (options.request) return options.request(payload.sessionId);
      return {
        status: "ok",
        stats: statsBySession.get(payload.sessionId) ?? null,
      };
    }
    return { status: "ok" };
  };

  const window = {
    __codexSessionDeleteBridge: options.bridgeReady === false ? undefined : bridge,
    innerHeight: 800,
    innerWidth: 1000,
    location,
    listeners: new Map(),
    addEventListener: FakeElementCore.prototype.addEventListener,
    removeEventListener: FakeElementCore.prototype.removeEventListener,
  };

  const testSetTimeout = (callback, delay, ...args) => {
    const timer = setTimeout(callback, delay, ...args);
    timer.unref?.();
    return timer;
  };
  const sandbox = {
    document,
    window,
    location,
    URLSearchParams,
    MutationObserver: FakeMutationObserver,
    HTMLElement: FakeElement,
    setTimeout: testSetTimeout,
    clearTimeout,
  };
  const context = vm.createContext(sandbox);
  vm.runInContext(source, context);
  const observer = latestObserver;

  return {
    anchor,
    textarea,
    document,
    scope,
    calls,
    statsBySession,
    bar: () => findById(documentElement, "codey-conversation-stats"),
    snapshot: () => context.window.__codeyConversationStats?.snapshot(),
    refresh: () => context.window.__codeyConversationStats.refresh(),
    setStats: (sessionId, value) => statsBySession.set(sessionId, value),
    setConversation: (id) => {
      anchor.setAttribute("data-above-composer-conversation-id", id);
    },
    emitMutation: (mutations = [{ type: "childList", target: documentElement }]) => {
      if (observer?.observed) observer.callback(mutations);
    },
    emitAnchorMutation: () => {
      observer?.callback?.([
        { type: "attributes", target: anchor, attributeName: "data-above-composer-conversation-id" },
      ]);
    },
    rerun: () => vm.runInContext(source, context),
    dispose: () => context.window.__codeyConversationStats.dispose(),
    markPreviousVersion: () => { context.window.__codeyConversationStats.version = 3; },
    window,
  };
};

const STATS = {
  usageSource: "runtime",
  usageScope: "runtime",
  requestsScope: "runtime",
  model: "gpt-5",
  requests: 2,
  inFlight: 0,
  lastStatus: "succeeded",
  outputTokensPerSecond: 999,
  averageOutputTokensPerSecond: 187.52,
  averageSpeedIsEstimate: false,
  averageSpeedSource: "runtime",
  averageSpeedScope: "runtime",
  averageSpeedRequestCount: 2,
  inputTokens: 128_400,
  outputTokens: 3_100,
  cachedInputTokens: 120_000,
  cacheWriteInputTokens: 2_000,
  totalTokens: 131_500,
  updatedAtUnixMs: 1_760_000_000_000,
};

const metric = (bar, name) => name === "status" ? bar.dataset.status : bar.querySelector(`.codey-cs-${name}`).textContent;
const field = (bar, name) => bar.__codeyCs.fields[name].textContent;
const summary = (env) => env.bar().querySelector("button.codey-cs-summary");
const panel = (env) => env.document.getElementById("codey-conversation-stats-activity");
const click = (element) => element.dispatchEvent({ type: "click" });
const assertExpanded = (env, expanded) => {
  assert.equal(summary(env).getAttribute("aria-expanded"), String(expanded));
  assert.equal(panel(env).hidden, !expanded);
};

const observeRenderWork = (env) => {
  const counts = { composerScans: 0, textWrites: 0 };
  const querySelectorAll = env.document.querySelectorAll;
  env.document.querySelectorAll = (selector) => {
    if (selector === "[data-above-composer-conversation-id]") counts.composerScans += 1;
    return querySelectorAll(selector);
  };
  const refs = env.bar().__codeyCs;
  for (const element of [refs.speed, refs["cache-rate"], refs.total, refs.activity,
    ...Object.values(refs.fields), ...Object.values(refs.notes)]) {
    let value = element.textContent;
    Object.defineProperty(element, "textContent", {
      configurable: true,
      get: () => value,
      set: (next) => { counts.textWrites += 1; value = next; },
    });
  }
  return counts;
};

test("refreshes with one composer scan per async phase and only writes changed text", async () => {
  const env = createEnvironment({ statsBySession: { "conversation-1": STATS } });
  try {
    await flushTimers(30);
    const counts = observeRenderWork(env);
    const initialCalls = env.calls.length;
    env.refresh();
    await flushTimers(30);
    assert.deepEqual(counts, { composerScans: 2, textWrites: 0 });
    assert.equal(env.calls.length, initialCalls + 1);
    env.setStats("conversation-1", { ...STATS, totalTokens: 500, averageOutputTokensPerSecond: 50 });
    env.refresh();
    await flushTimers(30);
    assert.deepEqual(counts, { composerScans: 4, textWrites: 4 });
    assert.equal(metric(env.bar(), "total"), "500 tok");
    assert.equal(field(env.bar(), "totalTokens"), "500 tok");
    assert.equal(metric(env.bar(), "speed"), "50.0 tok/s");
    assert.equal(field(env.bar(), "speed"), "50.0 tok/s");
  } finally {
    env.dispose();
  }
});

test("remounts into a replacement composer after an in-flight stats response", async () => {
  let resolve;
  const env = createEnvironment({ request: () => new Promise(done => { resolve = done; }) });
  try {
    await flushTimers(30);
    const originalBar = env.bar();
    env.scope.remove();
    const replacement = env.document.createElement("div");
    const anchor = env.document.createElement("div");
    anchor.setAttribute("data-above-composer-conversation-id", "conversation-1");
    replacement.append(anchor, env.document.createElement("textarea"));
    env.document.body.appendChild(replacement);
    // 不发 MutationObserver 通知，确保完成异步读取时主动重新定位。
    resolve({ status: "ok", stats: STATS });
    await flushTimers(30);
    assert.equal(env.bar(), originalBar);
    assert.equal(env.bar().parentElement, replacement);
    assert.equal(metric(env.bar(), "total"), "132k tok");
  } finally {
    env.dispose();
  }
});

test("discards a response after navigation without a mutation notification", async () => {
  const pending = [];
  const env = createEnvironment({ request: sessionId => new Promise(resolve => pending.push({ sessionId, resolve })) });
  try {
    await flushTimers(30);
    env.setConversation("conversation-2");
    pending[0].resolve({ status: "ok", stats: STATS });
    await flushTimers(30);
    assert.equal(metric(env.bar(), "total"), "—");
    assert.equal(pending[1].sessionId, "conversation-2");
    pending[1].resolve({ status: "ok", stats: { ...STATS, totalTokens: 700 } });
    await flushTimers(30);
    assert.equal(metric(env.bar(), "total"), "700 tok");
  } finally {
    env.dispose();
  }
});

test("mounts cached stats when the composer appears after the initial response", async () => {
  const env = createEnvironment({ anchor: false, pathname: "/c/conversation-1", statsBySession: { "conversation-1": STATS } });
  try {
    await flushTimers(30);
    assert.equal(env.bar(), null);
    assert.equal(env.calls.length, 1);
    env.scope.append(env.anchor, env.textarea);
    env.emitAnchorMutation();
    await flushTimers(160);
    assert.equal(env.bar().parentElement, env.scope);
    assert.equal(metric(env.bar(), "total"), "132k tok");
    assert.equal(env.calls.length, 1, "mounting must not refetch the same session");
  } finally {
    env.dispose();
  }
});

test("mounts the stats bar under the composer and renders speed/cache/total", async () => {
  const env = createEnvironment({ statsBySession: { "conversation-1": STATS } });
  await flushTimers(30);

  const bar = env.bar();
  assert.ok(bar, "stats bar should exist");
  assert.equal(bar.parentElement, env.scope);
  assert.equal(bar.hidden, false);
  assert.equal(metric(bar, "speed"), "188 tok/s");
  assert.equal(metric(bar, "cache-rate"), "93%");
  assert.equal(metric(bar, "total"), "132k tok");
  assert.equal(field(bar, "outputTokens"), "3,100 tok");
  assert.equal(summary(env).querySelectorAll(".codey-cs-metric").length, 2);
  for (const selector of [".codey-cs-meta", ".codey-cs-status", '[data-field="model"]']) {
    assert.equal(summary(env).querySelector(selector), null);
  }
  assert.equal(field(bar, "totalTokens"), "131,500 tok");
  assert.equal(field(bar, "cachedInputTokens"), "120,000 tok");
  assert.match(summary(env).getAttribute("aria-label"), /会话统计/);
  assert.match(summary(env).getAttribute("aria-label"), /查看统计详情/);
  assert.equal(
    env.calls.filter((call) => call.path === "/session/stats").at(-1).payload.sessionId,
    "conversation-1",
  );
});

test("keeps the bar visible while the current conversation has no stats", async () => {
  const env = createEnvironment();
  await flushTimers(30);
  assert.equal(env.bar()?.hidden, false);
  assert.equal(metric(env.bar(), "status"), "暂无统计");
  assert.equal(metric(env.bar(), "total"), "—");
});

test("shows unknown values when upstream omits speed and cache", async () => {
  const env = createEnvironment({
    statsBySession: {
      "conversation-1": {
        ...STATS,
        averageOutputTokensPerSecond: null,
        cachedInputTokens: null,
        inputTokens: null,
      },
    },
  });
  await flushTimers(30);
  const bar = env.bar();
  assert.equal(bar.hidden, false);
  assert.equal(metric(bar, "speed"), "—");
  assert.equal(metric(bar, "cache-rate"), "—");
  assert.equal(field(bar, "cachedInputTokens"), "—");
  assert.equal(metric(bar, "total"), "132k tok");
});

test("switches stats context when the conversation changes", async () => {
  const env = createEnvironment({
    statsBySession: {
      "conversation-1": STATS,
      "conversation-2": { ...STATS, totalTokens: 900, model: "claude-sonnet" },
    },
  });
  await flushTimers(30);
  assert.equal(metric(env.bar(), "total"), "132k tok");

  env.setConversation("conversation-2");
  env.emitAnchorMutation();
  await flushTimers(200);

  const bar = env.bar();
  assert.equal(metric(bar, "total"), "900 tok");
  assert.equal(env.calls.at(-1).payload.sessionId, "conversation-2");
});

test("clears stale numbers immediately on a session switch before refetch", async () => {
  const env = createEnvironment({ statsBySession: { "conversation-1": STATS } });
  await flushTimers(30);
  assert.equal(env.bar().hidden, false);

  env.setConversation("conversation-3");
  env.emitAnchorMutation();
  await flushTimers(160);

  const bar = env.bar();
  assert.equal(bar.hidden, false);
  assert.equal(metric(bar, "total"), "—", "previous session totals must be cleared");
  assert.equal(metric(bar, "status"), "暂无统计");
  assert.match(summary(env).getAttribute("aria-label"), /会话统计/);
  assert.doesNotMatch(summary(env).getAttribute("aria-label"), /132k/);
});

test("remounts the bar after Codex rebuilds the composer DOM", async () => {
  const env = createEnvironment({ statsBySession: { "conversation-1": STATS } });
  await flushTimers(30);
  const bar = env.bar();
  assert.ok(bar);
  bar.remove();
  env.emitMutation([
    { type: "childList", target: env.scope, removedNodes: [bar], addedNodes: [] },
  ]);
  await flushTimers(200);
  const remounted = env.bar();
  assert.ok(remounted, "bar should be re-created after removal");
  assert.equal(remounted.parentElement, env.scope);
  assert.equal(remounted.hidden, false);
});

test("marks in-flight requests as running", async () => {
  const env = createEnvironment({
    statsBySession: { "conversation-1": { ...STATS, inFlight: 1 } },
  });
  await flushTimers(30);
  const bar = env.bar();
  assert.equal(bar.hidden, false);
  assert.equal(bar.dataset.running, "true");
});

test("does not fetch in a hidden page and refreshes when visibility returns", async () => {
  const env = createEnvironment({ statsBySession: { "conversation-1": { ...STATS, inFlight: 1 } } });
  await flushTimers(30);
  const calls = env.calls.length;
  env.document.visibilityState = "hidden";
  env.refresh();
  await flushTimers(30);
  assert.equal(env.calls.length, calls);
  env.document.visibilityState = "visible";
  env.document.dispatchEvent({ type: "visibilitychange" });
  await flushTimers(30);
  assert.equal(env.calls.length, calls + 1);
});

test("shows unsuccessful request states and prioritizes active requests", async () => {
  const env = createEnvironment();
  for (const [lastStatus, label, detail] of [
    ["failed", "最近请求失败", "失败"],
    ["incomplete", "最近请求未完成", "未完成"],
    ["cancelled", "最近请求已取消", "已取消"],
  ]) {
    env.setStats("conversation-1", { ...STATS, lastStatus });
    env.refresh();
    await flushTimers(30);
    assert.equal(metric(env.bar(), "status"), label);
    assert.ok(summary(env).getAttribute("aria-label").includes(label));
  }
  env.setStats("conversation-1", { ...STATS, lastStatus: "failed", inFlight: 1 });
  env.refresh();
  await flushTimers(30);
  assert.equal(metric(env.bar(), "status"), "1 个进行中");
});

test("ignores hidden composer anchors and stays empty without a session", async () => {
  const env = createEnvironment({ inputVisible: false });
  await flushTimers(30);
  assert.equal(env.snapshot().sessionId, "");
});

test("does not double-install on repeated injection", async () => {
  const env = createEnvironment({ statsBySession: { "conversation-1": STATS } });
  await flushTimers(30);
  // 同一上下文再次执行整个脚本：installed 标记应阻止重复挂载。
  env.rerun();
  await flushTimers(30);
  const count = env.scope.children.filter(
    (child) => child.id === "codey-conversation-stats",
  ).length;
  assert.equal(count, 1);
  assert.equal(env.snapshot().installed !== false, true);
});

test("keeps a waiting bar without a session and does not request stats", async () => {
  const env = createEnvironment({
    conversationId: "", pathname: "/c/previous-conversation",
    statsBySession: { "previous-conversation": STATS },
  });
  await flushTimers(30);
  assert.equal(env.bar().hidden, false);
  assert.equal(metric(env.bar(), "status"), "等待会话");
  assert.equal(metric(env.bar(), "total"), "—");
  assert.equal(env.calls.length, 0);
  click(summary(env));
  assertExpanded(env, true);
});

test("preserves reported zero tokens and zero cache hit rate", async () => {
  const env = createEnvironment({ statsBySession: { "conversation-1": {
    requests: 1, totalTokens: 0, inputTokens: 100, outputTokens: 0,
    cachedInputTokens: 0, cacheWriteInputTokens: 0, inFlight: 0,
  } } });
  await flushTimers(30);
  assert.equal(metric(env.bar(), "total"), "0 tok");
  assert.equal(field(env.bar(), "outputTokens"), "0 tok");
  assert.equal(metric(env.bar(), "cache-rate"), "0%");
  assert.equal(metric(env.bar(), "status"), "运行期统计");
});

for (const stats of [{}, { requests: 0 }]) {
  test(`treats ${JSON.stringify(stats)} as an empty session`, async () => {
    const env = createEnvironment({ statsBySession: { "conversation-1": stats } });
    await flushTimers(30);
    assert.equal(env.bar().hidden, false);
    assert.equal(metric(env.bar(), "status"), "暂无统计");
    assert.equal(metric(env.bar(), "total"), "—");
  });
}

test("pins each independent panel and switches between both groups", async () => {
  const env = createEnvironment({ statsBySession: { "conversation-1": STATS } });
  await flushTimers(30);
  assert.equal(summary(env).getAttribute("aria-controls"), panel(env).id);
  click(summary(env));
  assertExpanded(env, true);
  const tokens = env.bar().querySelector(".codey-cs-tokens-summary");
  click(tokens);
  assertExpanded(env, false);
  assert.equal(tokens.getAttribute("aria-expanded"), "true");
  click(tokens);
  assert.equal(tokens.getAttribute("aria-expanded"), "false");
});

test("centers the details immediately above the trigger and clamps the viewport", async () => {
  const env = createEnvironment({ statsBySession: { "conversation-1": STATS } });
  await flushTimers(30);
  summary(env).getBoundingClientRect = () => ({ left: 200, top: 700, width: 400, height: 28 });
  panel(env).getBoundingClientRect = () => ({ width: 420, height: 300 });
  env.textarea.getBoundingClientRect = () => ({ left: 100, top: 520, width: 600, height: 140 });
  click(summary(env));
  assert.equal(panel(env).style.left, "190px");
  assert.equal(panel(env).style.top, "392px");
  click(summary(env));
  summary(env).getBoundingClientRect = () => ({ left: 0, top: 700, width: 200, height: 28 });
  click(summary(env));
  assert.equal(panel(env).style.left, "12px");
});

test("Escape restores focus only when focus was inside the stats bar", async () => {
  const env = createEnvironment();
  await flushTimers(30);
  for (const inside of [true, false]) {
    click(summary(env));
    const target = inside ? summary(env) : env.textarea;
    target.focus();
    let prevented = false;
    let stopped = false;
    env.document.dispatchEvent({
      type: "keydown", key: "Escape",
      preventDefault: () => { prevented = true; },
      stopPropagation: () => { stopped = true; },
    });
    assertExpanded(env, false);
    assert.equal(env.document.activeElement, inside ? summary(env) : env.textarea);
    assert.equal(prevented, inside);
    assert.equal(stopped, inside);
  }
});

test("pointerdown inside details keeps them open, outside closes them", async () => {
  const env = createEnvironment();
  await flushTimers(30);
  click(summary(env));
  assert.equal(panel(env).parentElement.parentElement, env.document.body);
  assert.equal(env.bar().contains(panel(env)), false, "details must escape the clipping composer");
  env.document.dispatchEvent({ type: "pointerdown", target: panel(env).querySelector("dd") });
  assertExpanded(env, true);
  env.textarea.focus();
  env.document.dispatchEvent({ type: "pointerdown", target: env.textarea });
  assertExpanded(env, false);
  assert.equal(env.document.activeElement, env.textarea);
});

test("Escape from the independent panel restores its trigger focus", async () => {
  const env = createEnvironment();
  await flushTimers(30);
  click(summary(env));
  panel(env).focus();
  env.document.dispatchEvent({ type: "keydown", key: "Escape", preventDefault() {}, stopPropagation() {} });
  assertExpanded(env, false);
  assert.equal(env.document.activeElement, summary(env));
});

test("native popovers switch and close together with their triggers", async () => {
  const env = createEnvironment({ popover: true });
  await flushTimers(30);
  click(summary(env));
  assert.equal(panel(env).popoverOpen, true);
  const tokens = env.bar().__codeyCs.panels.tokens;
  click(env.bar().__codeyCs.buttons.tokens);
  assert.equal(panel(env).popoverOpen, false);
  assert.equal(tokens.popoverOpen, true);
  env.document.dispatchEvent({ type: "pointerdown", target: env.textarea });
  assert.equal(tokens.popoverOpen, false);
});

test("reuses one detached bar and overlay and closes panels on remount", async () => {
  const env = createEnvironment({ statsBySession: { "conversation-1": STATS } });
  await flushTimers(30);
  const originalBar = env.bar();
  const overlay = panel(env).parentElement;
  click(summary(env));
  originalBar.remove();
  env.emitMutation([{ type: "childList", target: env.scope, removedNodes: [originalBar] }]);
  await flushTimers(160);
  assert.equal(env.bar(), originalBar);
  assert.equal(panel(env).parentElement, overlay);
  assert.equal(env.document.querySelectorAll("#codey-conversation-stats-overlay").length, 1);
  assertExpanded(env, false);
  overlay.remove();
  env.emitMutation([{ type: "childList", target: env.document.body, removedNodes: [overlay] }]);
  await flushTimers(160);
  assert.equal(overlay.parentElement, env.document.body);
  assert.equal(metric(env.bar(), "total"), "132k tok");
});

test("dispose removes portal, listeners and pending response without resurrecting UI", async () => {
  let resolve;
  const env = createEnvironment({ request: () => new Promise(done => { resolve = done; }) });
  await flushTimers(30);
  env.dispose();
  assert.equal(env.bar(), null);
  assert.equal(env.document.getElementById("codey-conversation-stats-overlay"), null);
  assert.equal(env.document.getElementById("codey-conversation-stats-styles"), null);
  assert.ok([...env.document.listeners.values(), ...env.window.listeners.values()].every(list => list.length === 0));
  resolve({ status: "ok", stats: STATS });
  await flushTimers(30);
  assert.equal(env.bar(), null);
  assert.equal(env.calls.length, 1);
});

test("version replacement disposes previous ownership before installing one new portal", async () => {
  const env = createEnvironment();
  await flushTimers(30);
  const originalBar = env.bar();
  env.markPreviousVersion();
  env.rerun();
  await flushTimers(30);
  assert.notEqual(env.bar(), originalBar);
  assert.equal(env.document.querySelectorAll("#codey-conversation-stats-overlay").length, 1);
  assert.equal(env.document.listeners.get("pointerdown").length, 1);
  click(summary(env));
  assertExpanded(env, true);
});

test("hiding the composer closes its open details", async () => {
  const env = createEnvironment();
  await flushTimers(30);
  click(summary(env));
  env.textarea.visible = false;
  env.textarea.setAttribute("hidden", "");
  env.emitMutation([{ type: "attributes", target: env.textarea, attributeName: "hidden" }]);
  await flushTimers(160);
  assert.equal(env.bar().hidden, true);
  assertExpanded(env, false);
});

test("keeps current totals on failed refresh and recovers after retry", async () => {
  let response = { status: "ok", stats: STATS };
  const env = createEnvironment({ request: () => {
    if (response instanceof Error) throw response;
    return response;
  } });
  await flushTimers(30);
  for (const failure of [{ status: "failed" }, new Error("offline")]) {
    response = failure;
    env.refresh();
    await flushTimers(30);
    assert.equal(metric(env.bar(), "total"), "132k tok");
    assert.equal(metric(env.bar(), "status"), "统计暂不可用");
    assert.equal(env.bar().hidden, false);
  }
  response = { status: "ok", stats: { ...STATS, totalTokens: 200 } };
  env.refresh();
  await flushTimers(30);
  assert.equal(metric(env.bar(), "total"), "200 tok");
  assert.equal(metric(env.bar(), "status"), "运行期统计");
});

test("discards the original A response after switching A to B to A", async () => {
  const pending = [];
  const env = createEnvironment({ request: (sessionId) => new Promise((resolve) => {
    pending.push({ sessionId, resolve });
  }) });
  await flushTimers(30);
  assert.equal(pending[0].sessionId, "conversation-1");
  click(summary(env));
  env.setConversation("conversation-2");
  env.emitAnchorMutation();
  await flushTimers(160);
  assertExpanded(env, false);
  env.setConversation("conversation-1");
  env.emitAnchorMutation();
  await flushTimers(160);
  pending[0].resolve({ status: "ok", stats: STATS });
  await flushTimers(30);
  assert.equal(metric(env.bar(), "total"), "—", "old A request must not repopulate A after a round trip");
  assert.equal(pending.length, 2, "current A must fetch again immediately after the stale request settles");
  assert.equal(pending[1].sessionId, "conversation-1");
  pending[1].resolve({ status: "ok", stats: { ...STATS, totalTokens: 700 } });
  await flushTimers(30);
  assert.equal(metric(env.bar(), "total"), "700 tok");
});

test("restores historical totals without inventing historical request information", async () => {
  const history = { ...STATS, usageSource: "rollout", usageScope: "session", requestsScope: null, requests: null, inFlight: null, cacheWriteInputTokens: null };
  const env = createEnvironment({ statsBySession: { "conversation-1": history } });
  await flushTimers(30);
  assert.equal(metric(env.bar(), "total"), "132k tok");
  assert.equal(metric(env.bar(), "status"), "历史统计");
  assert.match(env.bar().querySelector(".codey-cs-tokens-summary").getAttribute("aria-label"), /会话累计 132k/);
  assert.equal(env.bar().dataset.running, "false");
});

test("historical tokens and current runtime request details keep separate scopes", async () => {
  const env = createEnvironment({ statsBySession: { "conversation-1": { ...STATS, usageSource: "rollout", usageScope: "session" } } });
  await flushTimers(30);
  assert.equal(field(env.bar(), "totalTokens"), "131,500 tok");
  assert.equal(metric(env.bar(), "speed"), "188 tok/s");
  env.setStats("conversation-1", { ...STATS, usageSource: "rollout", usageScope: "session", averageOutputTokensPerSecond: null });
  env.refresh();
  await flushTimers(30);
  assert.equal(metric(env.bar(), "speed"), "—");
});

test("shows recorded historical average independently of runtime request fields", async () => {
  const history = {
    usageSource: "rollout", usageScope: "session", requestsScope: null,
    totalTokens: 131500, requests: null, model: null, lastStatus: null,
    averageOutputTokensPerSecond: 62.35, averageSpeedSource: "request_log",
    averageSpeedScope: "recorded_requests", averageSpeedRequestCount: 20,
  };
  const env = createEnvironment({ statsBySession: { "conversation-1": history } });
  await flushTimers(30);
  assert.equal(metric(env.bar(), "speed"), "62.4 tok/s");
  assert.match(summary(env).getAttribute("aria-label"), /仅覆盖有记录且有有效计时的请求/);
  env.setStats("conversation-1", { ...history, totalTokens: null, usageSource: "unknown", usageScope: "unknown" });
  env.refresh();
  await flushTimers(30);
  assert.equal(metric(env.bar(), "speed"), "62.4 tok/s");
  assert.equal(env.bar().dataset.state, "ok");
  assert.match(env.bar().querySelector(".codey-cs-tokens-summary").getAttribute("aria-label"), /统计范围未知/);
  assert.equal(metric(env.bar(), "status"), "历史速度记录");
});

test("preserves the recorded average while the next step waits for output", async () => {
  const data = { ...STATS, averageSpeedSource: "request_log", averageSpeedScope: "recorded_requests" };
  const env = createEnvironment({ statsBySession: { "conversation-1": data } });
  await flushTimers(30);
  assert.equal(metric(env.bar(), "speed"), "188 tok/s");
  for (const changes of [{ inFlight: 1, speedState: "waiting" }, { averageOutputTokensPerSecond: null }, { averageSpeedScope: null }]) {
    env.setStats("conversation-1", { ...data, ...changes });
    env.refresh();
    await flushTimers(30);
    assert.equal(metric(env.bar(), "speed"), changes.inFlight ? "188 tok/s" : "—");
  }
  env.setStats("conversation-1", { ...STATS, speedSource: "runtime", speedScope: "latest_request" });
  env.refresh();
  await flushTimers(30);
  assert.equal(metric(env.bar(), "speed"), "188 tok/s");
});

test("legacy backend is runtime only and cannot present a single-request speed as an average", async () => {
  const legacy = { ...STATS };
  delete legacy.usageSource;
  delete legacy.usageScope;
  delete legacy.requestsScope;
  delete legacy.averageOutputTokensPerSecond;
  delete legacy.averageSpeedSource;
  delete legacy.averageSpeedScope;
  const env = createEnvironment({ statsBySession: { "conversation-1": legacy } });
  await flushTimers(30);
  assert.match(env.bar().querySelector(".codey-cs-tokens-summary").getAttribute("aria-label"), /本次运行/);
  assert.doesNotMatch(summary(env).getAttribute("aria-label"), /会话累计/);
  assert.equal(metric(env.bar(), "speed"), "—");
});

test("shows exact and estimated session averages during generation", async () => {
  const env = createEnvironment();
  for (const [averageSpeedIsEstimate, expected] of [[false, "62.4 tok/s"], [true, "≈62.4 tok/s"]]) {
    env.setStats("conversation-1", { ...STATS, inFlight: 1, averageOutputTokensPerSecond: 62.35,
      averageSpeedIsEstimate });
    env.refresh();
    await flushTimers(30);
    assert.equal(metric(env.bar(), "speed"), expected);
    assert.equal(field(env.bar(), "speed"), expected);
    assert.equal(env.bar().dataset.running, "true");
    assert.match(summary(env).getAttribute("aria-label"), /会话平均速度/);
    assert.match(env.bar().__codeyCs.notes.activity.textContent, /累计输出 Token ÷ 累计生成耗时，工具间隔不计/);
    assert.equal(env.bar().__codeyCs.notes.activity.textContent.includes("含生成中的 Token 估算"), averageSpeedIsEstimate);
  }
});

test("uses the backend weighted average instead of total tokens or the last step speed", async () => {
  // 两步各输出 100 token，用时 1 秒和 9 秒；累计平均为 20，不能取两步速度的算术平均。
  const env = createEnvironment({ statsBySession: { "conversation-1": {
    ...STATS, outputTokens: 10000, outputTokensPerSecond: 100 / 9,
    averageOutputTokensPerSecond: 200 / 10, modelDurationMs: 30000,
  } } });
  await flushTimers(30);
  assert.equal(metric(env.bar(), "speed"), "20.0 tok/s");
  assert.equal(field(env.bar(), "speed"), "20.0 tok/s");
  assert.equal(panel(env).querySelectorAll("dt").at(-1).textContent, "平均速度（TPS）");
});

test("keeps the accumulated runtime average and open panel across step boundaries", async () => {
  const env = createEnvironment({ statsBySession: { "conversation-1": STATS } });
  await flushTimers(30);
  click(summary(env));
  const originalPanel = panel(env);
  env.setStats("conversation-1", { ...STATS, requests: 3, inFlight: 1,
    speedState: "waiting", outputTokensPerSecond: null });
  env.refresh();
  await flushTimers(30);
  assert.equal(metric(env.bar(), "speed"), "188 tok/s");
  assert.equal(field(env.bar(), "speed"), "188 tok/s");
  assert.equal(panel(env), originalPanel);
  assertExpanded(env, true);
});

test("shows combined historical and live averages with an explicit coverage limit", async () => {
  const env = createEnvironment({ statsBySession: { "conversation-1": {
    ...STATS, usageSource: "rollout", usageScope: "session", inFlight: 1,
    averageOutputTokensPerSecond: 42, averageSpeedIsEstimate: true,
    averageSpeedSource: "runtime_and_request_log", averageSpeedScope: "recorded_and_runtime",
  } } });
  await flushTimers(30);
  assert.equal(metric(env.bar(), "speed"), "≈42.0 tok/s");
  assert.match(env.bar().__codeyCs.notes.activity.textContent, /覆盖有记录的历史请求与本次运行，未记录的历史请求不计入/);
});

test("rejects invalid averages and mismatched or missing average attribution", async () => {
  const env = createEnvironment();
  for (const changes of [
    { averageOutputTokensPerSecond: NaN }, { averageOutputTokensPerSecond: Infinity },
    { averageOutputTokensPerSecond: -5 },
    { averageSpeedSource: undefined }, { averageSpeedSource: "request_log" },
    { averageSpeedScope: "latest_request" },
  ]) {
    env.setStats("conversation-1", { ...STATS, ...changes });
    env.refresh();
    await flushTimers(30);
    assert.equal(metric(env.bar(), "speed"), "—");
  }
});

test("preserves a reported zero session average", async () => {
  const env = createEnvironment({ statsBySession: { "conversation-1": {
    ...STATS, averageOutputTokensPerSecond: 0,
  } } });
  await flushTimers(30);
  assert.equal(metric(env.bar(), "speed"), "0.0 tok/s");
  assert.equal(field(env.bar(), "speed"), "0.0 tok/s");
});

test("waits for current output instead of showing a previous request speed", async () => {
  const env = createEnvironment();
  for (const speedScope of [undefined, "latest_request", "latest_recorded_request", "current_request"]) {
    env.setStats("conversation-1", { ...STATS, inFlight: 1, averageOutputTokensPerSecond: null,
      speedSource: speedScope === "latest_recorded_request" ? "request_log" : "runtime",
      speedScope, speedState: "waiting", outputTokensPerSecond: speedScope === "current_request" ? null : 188 });
    env.refresh();
    await flushTimers(30);
    assert.equal(metric(env.bar(), "speed"), "等待输出");
    assert.equal(field(env.bar(), "speed"), "等待输出");
  }
});

test("distinguishes initial waiting from output arriving without a speed sample", async () => {
  const env = createEnvironment();
  for (const [speedState, expected] of [["waiting", "等待输出"], ["streaming", "生成中"], [undefined, "生成中"]]) {
    env.setStats("conversation-1", { ...STATS, inFlight: 1, speedSource: "runtime",
      speedScope: "current_request", averageOutputTokensPerSecond: null, speedState });
    env.refresh();
    await flushTimers(30);
    assert.equal(metric(env.bar(), "speed"), expected);
    assert.equal(field(env.bar(), "speed"), expected);
  }
});

test("transitions the live average to a completed average without using the final single-step speed", async () => {
  const live = { ...STATS, inFlight: 1, averageOutputTokensPerSecond: 62.35,
    averageSpeedIsEstimate: true, outputTokensPerSecond: 900 };
  const env = createEnvironment({ statsBySession: { "conversation-1": live } });
  await flushTimers(30);
  assert.equal(metric(env.bar(), "speed"), "≈62.4 tok/s");
  env.setStats("conversation-1", { ...live, inFlight: 0, averageOutputTokensPerSecond: null });
  env.refresh();
  await flushTimers(30);
  assert.equal(metric(env.bar(), "speed"), "—", "a single-step measurement cannot substitute a missing session average");
  env.setStats("conversation-1", { ...live, inFlight: 0, averageOutputTokensPerSecond: 71.2,
    averageSpeedIsEstimate: false });
  env.refresh();
  await flushTimers(30);
  assert.equal(metric(env.bar(), "speed"), "71.2 tok/s");
  assert.match(summary(env).getAttribute("aria-label"), /会话平均速度/);
});

test("hides a frozen live speed after refresh failure and resumes on recovery", async () => {
  const live = { ...STATS, inFlight: 1, averageSpeedIsEstimate: true };
  let response = { status: "ok", stats: live };
  const env = createEnvironment({ request: () => response });
  await flushTimers(30);
  assert.equal(metric(env.bar(), "speed"), "≈188 tok/s");
  response = { status: "failed" };
  env.refresh();
  await flushTimers(30);
  assert.equal(metric(env.bar(), "speed"), "更新暂停");
  assert.equal(field(env.bar(), "speed"), "更新暂停");
  assert.equal(metric(env.bar(), "total"), "132k tok");
  response = { status: "ok", stats: { ...live, averageOutputTokensPerSecond: 54 } };
  env.refresh();
  await flushTimers(30);
  assert.equal(metric(env.bar(), "speed"), "≈54.0 tok/s");
});

test("clears live speed and waiting state on conversation changes", async () => {
  const env = createEnvironment({ statsBySession: { "conversation-1": {
    ...STATS, inFlight: 1, averageSpeedIsEstimate: true,
  } } });
  await flushTimers(30);
  assert.equal(metric(env.bar(), "speed"), "≈188 tok/s");
  env.setConversation("conversation-2");
  env.emitAnchorMutation();
  await flushTimers(160);
  assert.equal(metric(env.bar(), "speed"), "—");
  assert.equal(env.bar().dataset.running, "false");
});

test("partial historical usage keeps absent values unknown and preserves explicit zero", async () => {
  const env = createEnvironment({ statsBySession: { "conversation-1": {
    usageSource: "rollout", usageScope: "session", requestsScope: null,
    totalTokens: null, inputTokens: 0, outputTokens: null, cachedInputTokens: null,
  } } });
  await flushTimers(30);
  assert.equal(field(env.bar(), "outputTokens"), "—");
  for (const name of ["total", "cache-rate", "speed"]) assert.equal(metric(env.bar(), name), "—");
  assert.equal(metric(env.bar(), "status"), "历史统计");
});

test("normalizes local conversation IDs before reading historical stats", async () => {
  const env = createEnvironment({ conversationId: " local:conversation-1 ", statsBySession: { "conversation-1": STATS } });
  await flushTimers(30);
  assert.equal(env.calls[0].payload.sessionId, "conversation-1");
  assert.equal(metric(env.bar(), "total"), "132k tok");
});

test("preserves the same session values while refreshing and marks failed values stale", async () => {
  let resolveRefresh;
  let first = true;
  const env = createEnvironment({ request: () => {
    if (first) { first = false; return { status: "ok", stats: STATS }; }
    return new Promise((resolve) => { resolveRefresh = resolve; });
  } });
  await flushTimers(30);
  env.refresh();
  await flushTimers(30);
  assert.equal(metric(env.bar(), "total"), "132k tok");
  assert.equal(metric(env.bar(), "status"), "更新统计中");
  resolveRefresh({ status: "failed" });
  await flushTimers(30);
  assert.equal(metric(env.bar(), "total"), "132k tok");
  assert.match(env.bar().__codeyCs.notes.activity.textContent, /可能已过期/);
});

test("shows reference activity rows and computes uncached input without double counting", async () => {
  const env = createEnvironment({ statsBySession: { "conversation-1": { ...STATS,
    turnCount: 4, stepCount: 209, modelDurationMs: 1286000, toolDurationMs: 573000,
    averageTtftMs: 2800, activityScope: "recorded_session", timingScope: "recorded_requests",
  } } });
  await flushTimers(30);
  assert.equal(metric(env.bar(), "activity"), "4 轮 209 步");
  assert.equal(field(env.bar(), "modelDurationMs"), "21分 26秒");
  assert.equal(field(env.bar(), "toolDurationMs"), "9分 33秒");
  assert.equal(field(env.bar(), "averageTtftMs"), "2.8秒");
  assert.equal(field(env.bar(), "uncachedInputTokens"), "8,400 tok");
  assert.equal(env.bar().querySelectorAll(".codey-cs-summary").length, 2);
});

test("preview remains open during pointer travel and click pins it", async () => {
  const env = createEnvironment();
  await flushTimers(30);
  summary(env).dispatchEvent({ type: "pointerenter" });
  assertExpanded(env, true);
  summary(env).dispatchEvent({ type: "pointerleave" });
  panel(env).dispatchEvent({ type: "pointerenter" });
  await flushTimers(380);
  assertExpanded(env, true);
  panel(env).dispatchEvent({ type: "pointerleave" });
  await flushTimers(380);
  assertExpanded(env, false);
  summary(env).dispatchEvent({ type: "focus" });
  click(summary(env));
  summary(env).dispatchEvent({ type: "pointerleave" });
  await flushTimers(380);
  assertExpanded(env, true);
});

test("does not fabricate missing activity or subtract inconsistent cache", async () => {
  const env = createEnvironment({ statsBySession: { "conversation-1": { ...STATS,
    turnCount: 0, stepCount: null, toolDurationMs: 0, modelDurationMs: -1,
    averageTtftMs: null, cachedInputTokens: 999999,
  } } });
  await flushTimers(30);
  assert.equal(metric(env.bar(), "activity"), "0 轮 — 步");
  assert.equal(field(env.bar(), "toolDurationMs"), "0秒");
  for (const key of ["modelDurationMs", "averageTtftMs", "uncachedInputTokens", "cacheRate"]) assert.equal(field(env.bar(), key), "—");
});

test("explains independently sourced steps and accumulated tool timing", async () => {
  const env = createEnvironment({ statsBySession: { "conversation-1": { ...STATS,
    turnCount: 4, stepCount: 2, activityScope: "recorded_session", stepScope: "runtime",
    timingScope: "recorded_requests", toolTimingScope: "recorded_tools",
  } } });
  await flushTimers(30);
  const note = env.bar().__codeyCs.notes.activity.textContent;
  assert.match(note, /步数仅覆盖本次运行/);
  assert.match(note, /有有效计时的请求/);
  assert.match(note, /工具用时为累计值，并行调用可能重叠/);
});
