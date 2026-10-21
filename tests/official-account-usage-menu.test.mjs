import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const panel = await readFile(new URL("../src/OfficialAccountsPanel.tsx", import.meta.url), "utf8");
const models = await readFile(new URL("../src/ModelSection.tsx", import.meta.url), "utf8");
const layout = await readFile(new URL("../src/SettingsLayout.tsx", import.meta.url), "utf8");
const app = await readFile(new URL("../src/App.tsx", import.meta.url), "utf8");

test("设置页只在首次打开对应菜单后挂载模块", () => {
  assert.match(layout, /const \[visitedPages, setVisitedPages\]/);
  assert.match(layout, /visitedPages\.has\(page\.id\) && \(/);
});

test("线路与模型菜单保留激活状态且不再渲染官方账号面板", () => {
  assert.match(app, /models: \(\) => \(/);
  assert.doesNotMatch(app, /<ModelSection\s+active=\{active\}/);
  assert.doesNotMatch(models, /<OfficialAccountsPanel/);
});

test("官方额度只在菜单打开时获取，关闭后停止", () => {
  assert.match(panel, /active: boolean;/);
  const effect = panel.match(
    /useEffect\(\(\) => \{\s*if \(!active\) return;\s*const list = accountsRef\.current;/,
  );
  assert.ok(effect, "额度查询应在菜单未打开时直接返回");
  assert.match(panel, /\}, \[active, accountListKey, refreshUsage\]\);/);
  // 账号失效等其它入口仍保留原行为。
  assert.match(panel, /await refreshUsage\(account\.id, false, \(\) => cancelled\)/);
  assert.match(panel, /onClick=\{\(\) => void refreshUsage\(account\.id, true\)\}/);
});
