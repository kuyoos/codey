import assert from "node:assert/strict";
import test from "node:test";

import { readSource } from "./helpers/read-source.mjs";
import {
  autoStubModule,
  collectElements,
  createComponentModule,
  elementProps,
  textContent,
} from "./helpers/jsx-tree.mjs";

const [dialogSource, appSource, stylesSource, operationsSource] = await Promise.all([
  readSource("src/SystemSettingsDialog.tsx"),
  readSource("src/App.tsx"),
  readSource("src/styles.css"),
  readSource("src/OperationsPanel.tsx"),
]);

// 组件库与图标按名字生成桩：断言的是真实组件渲染出的元素树（含事件接线），
// 而不是源码里的字符串，因此把 onClick/onCheckedChange 改成空操作一定会失败。
const ui = autoStubModule("ui");
const icons = autoStubModule("icon");
const { exports: dialog, reset } = await createComponentModule(
  new URL("../src/SystemSettingsDialog.tsx", import.meta.url),
  { stubs: { "./components/ui": ui, "@tabler/icons-react": icons } },
);

const clipboardWrites = [];
Object.defineProperty(globalThis, "navigator", {
  configurable: true,
  value: {
    clipboard: {
      writeText(value) {
        clipboardWrites.push(value);
        return Promise.resolve();
      },
    },
  },
});

function renderDialog(overrides = {}) {
  reset();
  const calls = { repair: 0 };
  const props = {
    open: true,
    onOpenChange: () => {},
    container: null,
    appVersion: "1.1.1",
    codexAppVersion: "0.4.0",
    codexAppPath: "/Applications/ChatGPT.app",
    isBusy: false,
    busy: null,
    onRepairCodexConfig: () => {
      calls.repair += 1;
    },
    configRepairNotice: null,
    ...overrides,
  };
  return { calls, props, tree: dialog.SystemSettingsDialog(props) };
}

const byLabel = (tree, label) =>
  collectElements(tree, (element) => elementProps(element)["aria-label"] === label);
const byText = (tree, text) =>
  collectElements(
    tree,
    (element) =>
      typeof elementProps(element).onClick === "function" &&
      textContent(element).replace(/\s+/g, "") === text,
  );
test("复制路径与修复配置都接到真实回调", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  clipboardWrites.length = 0;
  const { calls, tree } = renderDialog();
  const text = textContent(tree);
  assert.match(text, /v1\.1\.1/);
  assert.match(text, /v0\.4\.0/);
  assert.match(text, /\/Applications\/ChatGPT\.app/);

  const [copyButton] = byLabel(tree, "复制 Codex 路径");
  assert.ok(copyButton, "复制按钮必须带 aria-label");
  await elementProps(copyButton).onClick();
  assert.deepEqual(clipboardWrites, ["/Applications/ChatGPT.app"]);

  // 未配置 Codex 路径时复制默认路径，按钮依旧可用。
  clipboardWrites.length = 0;
  const fallback = renderDialog({ codexAppPath: "" });
  await elementProps(byLabel(fallback.tree, "复制 Codex 路径")[0]).onClick();
  assert.deepEqual(clipboardWrites, ["/Applications/ChatGPT.app"]);

  const [repairButton] = byText(tree, "修复");
  assert.ok(repairButton, "修复按钮必须存在");
  assert.equal(elementProps(repairButton).disabled, false);
  elementProps(repairButton).onClick();
  assert.equal(calls.repair, 1, "点击修复必须触发修复回调");
});

test("修复中显示进行态", () => {
  const busy = renderDialog({ busy: "repair-codex-config", isBusy: true });
  const [busyButton] = byText(busy.tree, "修复中…");
  assert.ok(busyButton, "修复进行中必须显示进行态");
  assert.equal(elementProps(busyButton).disabled, true);
  assert.equal(busy.calls.repair, 0);
});

test("系统设置不出现冗余状态文案", () => {
  const { tree } = renderDialog();
  const text = textContent(tree);
  assert.doesNotMatch(text, /当前应用/);
  assert.doesNotMatch(text, /运行中/);
  assert.doesNotMatch(text, /检查配置文件并修复异常/);
  assert.doesNotMatch(text, /后台每隔 30 分钟自动检查新版本/);
  assert.doesNotMatch(dialogSource, /检查配置文件并修复异常/);
});

test("App 把系统设置对话框接到修复命令", () => {
  assert.match(appSource, /onRepairCodexConfig=\{askRepairCodexConfig\}/);
  assert.match(appSource, /invoke<[\s\S]{0,240}?>\("repair_codex_config"\)/);
  assert.match(appSource, /className="sidebar-footer-bar"/);
  assert.match(appSource, /className="sidebar-footer-left"/);
  assert.doesNotMatch(appSource, /className="sidebar-footer-brand"/);
  assert.match(appSource, /className="sidebar-footer-version font-mono"/);
  assert.match(appSource, /className="sidebar-footer-right"/);
  assert.match(appSource, /setSystemSettingsOpen\(true\)/);
  assert.match(appSource, /<SystemSettingsDialog[\s\S]*open=\{systemSettingsOpen\}/);
});

test("styles define sidebar footer layout and responsive behavior", () => {
  assert.match(stylesSource, /\.sidebar-footer-bar\s*\{/);
  assert.match(stylesSource, /\.sidebar-footer-left\s*\{/);
  assert.match(stylesSource, /\.sidebar-footer-right\s*\{/);
  assert.match(stylesSource, /\.sidebar-footer-version\s*\{/);
});

test("OperationsPanel removes redundant top hero card and config repair button", () => {
  assert.doesNotMatch(operationsSource, /operations-hero-card/);
  assert.doesNotMatch(operationsSource, /Codex 运行状态/);
  assert.doesNotMatch(operationsSource, /修复 Codex 配置/);
  assert.doesNotMatch(operationsSource, /expanded-tray-toolbar/);
  assert.match(operationsSource, /已生效 \$\{enabledOptimizationFeatures\.length\} 项功能/);
});
