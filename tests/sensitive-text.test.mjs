import assert from "node:assert/strict";
import test from "node:test";

import { loadTypeScriptModule } from "./helpers/load-typescript-module.mjs";
import { readSource } from "./helpers/read-source.mjs";

const root = new URL("../", import.meta.url);
const [sensitiveText, modelSectionSource, panelSource] = await Promise.all([
  loadTypeScriptModule(new URL("src/sensitiveText.ts", root)),
  readSource("src/ModelSection.tsx"),
  readSource("src/OfficialAccountsPanel.tsx"),
]);

test("url masking hides the host and keeps the scheme, port and path", () => {
  const { maskUrl } = sensitiveText;
  const domain = maskUrl("https://codex.aicode.foo/v1");
  assert.match(domain, /^https:\/\/cod•+\.foo\/v1$/);
  assert.doesNotMatch(domain, /aicode/);
  assert.equal(
    maskUrl("http://172.31.184.162:38080/v1"),
    "http://172.•••.•••.162:38080/v1",
  );
  assert.equal(maskUrl("https://opencode.ai"), "https://ope•••••.ai");
  assert.equal(maskUrl(""), "");
  assert.doesNotMatch(maskUrl("不是地址"), /不是地址/);
});

test("email masking keeps the domain and hides the local part", () => {
  const { maskEmail } = sensitiveText;
  assert.match(maskEmail("kimzane9991@gmail.com"), /^ki•+@gmail\.com$/);
  assert.doesNotMatch(maskEmail("kimzane9991@gmail.com"), /kimzane9991/);
  assert.doesNotMatch(maskEmail("ab@example.com"), /ab@/);
  assert.match(maskEmail("no-at-sign"), /^no•+$/);
  assert.match(maskEmail("@example.com"), /^@e•+$/);
});

test("the route page masks on demand and never persists the toggle", () => {
  assert.match(modelSectionSource, /const \[maskSensitive, setMaskSensitive\] = useState\(false\)/);
  assert.match(modelSectionSource, /aria-label=\{maskSensitive \? "显示线路 URL 与邮箱" : "隐藏线路 URL 与邮箱"\}/);
  assert.match(modelSectionSource, /hideUrl\(profile\.baseUrl\)/);
  assert.doesNotMatch(modelSectionSource, /localStorage/);
  assert.match(panelSource, /maskSensitive = false/);
});
