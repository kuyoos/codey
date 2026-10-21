import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import ts from "typescript";

const panel = await readFile(new URL("../src/OfficialAccountsPanel.tsx", import.meta.url), "utf8");
const store = await readFile(new URL("../backend/src/official_accounts.rs", import.meta.url), "utf8");
const commands = await readFile(new URL("../backend/src/commands.rs", import.meta.url), "utf8");

// 从实际组件提取纯函数并执行，避免只验证源码里存在套餐字符串。
const helperNames = ["normalizePlan", "isProPlan", "formatPlan", "planTagClass"];
const parsedPanel = ts.createSourceFile("OfficialAccountsPanel.tsx", panel, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
const helpers = parsedPanel.statements.filter((statement) =>
  ts.isFunctionDeclaration(statement) && helperNames.includes(statement.name?.text),
);
assert.equal(helpers.length, helperNames.length);
const compiledHelpers = ts.transpileModule(helpers.map((statement) => statement.getText(parsedPanel)).join("\n"), {
  compilerOptions: { target: ts.ScriptTarget.ES2020 },
}).outputText;
const { formatPlan, isProPlan, planTagClass } = new Function(`${compiledHelpers}\nreturn { formatPlan, isProPlan, planTagClass };`)();

test("官方 Pro 套餐与明确数字别名显示对应档位与分档徽标", () => {
  for (const [value, expected] of [
    ["prolite", "Pro 100"], ["pro", "Pro 200"], ["promax", "Pro 500"],
    [" PROLITE ", "Pro 100"], [" Pro ", "Pro 200"], [" PROMAX ", "Pro 500"],
    ["pro100", "Pro 100"], ["pro200", "Pro 200"], ["pro500", "Pro 500"],
    [" Pro $100 ", "Pro 100"], ["pro_200", "Pro 200"], ["PRO-500", "Pro 500"],
  ]) {
    assert.equal(formatPlan(value), expected, value);
    assert.equal(isProPlan(value), true, value);
    assert.equal(planTagClass(value), `official-account-plan-badge is-pro is-pro-${expected.slice(4)}`, value);
  }
});

test("其它套餐保持原有名称与徽标，未知值不会被推断成 Pro 档位", () => {
  for (const [value, expected, tone] of [
    ["free", "Free", "default"], [" PLUS ", "Plus", "plus"],
    ["team", "Team", "team"], ["business", "Business", "team"],
    ["enterprise", "Enterprise", "team"], ["edu", "Edu", "default"],
    [undefined, "", "default"], ["", "", "default"], ["  ", "", "default"],
    ["business_prolite", "business_prolite", "default"],
    ["pro500_extra", "pro500_extra", "default"], ["pro50", "pro50", "default"],
    ["pro5x", "pro5x", "default"], ["pro20x", "pro20x", "default"],
    [" FuturePlan ", "FuturePlan", "default"],
    ["constructor", "constructor", "default"], ["__proto__", "__proto__", "default"],
  ]) {
    assert.equal(formatPlan(value), expected, value);
    assert.equal(isProPlan(value), false, value);
    assert.equal(planTagClass(value), `official-account-plan-badge is-${tone}`, value);
  }
});

test("账号记录里的套餐随登录信息刷新，而不是只在添加账号时解析一次", () => {
  // 解析逻辑集中在一处，登录、令牌刷新与 Codex 同步都走它。
  assert.match(store, /pub\(crate\) fn plan_type_from_auth\(auth: &Value\)/);
  assert.match(store, /let plan_type = plan_type_from_auth\(&auth\);/);
  const applyTokenResponse = store.match(
    /fn apply_token_response\([\s\S]*?\n\}/,
  )?.[0];
  assert.ok(applyTokenResponse, "应存在令牌响应应用函数");
  assert.match(applyTokenResponse, /record\.set_plan_type\(plan_type\.as_deref\(\)\)/);
  const syncDefault = store.match(/pub fn sync_default_from_codex_home\([\s\S]*?\n    \}/)?.[0];
  assert.ok(syncDefault, "应存在默认账号同步函数");
  assert.match(syncDefault, /let plan_type = plan_type_from_auth\(&auth\);/);
  assert.match(syncDefault, /record\.set_plan_type\(plan_type\.as_deref\(\)\)/);
});

test("额度接口返回的实时套餐回写到账号记录", () => {
  assert.match(store, /pub fn set_plan_type\(&mut self, plan_type: Option<&str>\) -> bool/);
  assert.match(store, /pub fn update_plan_type\(&self, id: &str, plan_type: Option<&str>\)/);
  // 凭据条件更新时套餐一起写入，否则刷新后的新套餐会被磁盘旧值盖回去。
  assert.match(store, /current\.plan_type = updated\.plan_type\.clone\(\);/);
  const query = commands.slice(
    commands.indexOf("async fn query_stored_official_account_usage"),
  );
  assert.match(query, /snapshot\.get\("planType"\)/);
  assert.match(query, /update_plan_type\(&plan_id, Some\(&plan_type\)\)/);
  // 只有自己打开的线路菜单会写回套餐：页头的定时读取不碰账号记录。
  assert.match(query, /if write_back_plan\s*&& snapshot\.get\("status"\)/);
  const entry = commands.slice(
    commands.indexOf("async fn query_official_account_usage"),
    commands.indexOf("async fn header_official_account_id"),
  );
  assert.match(entry, /query_stored_official_account_usage\(state, force_refresh, account_id, true\)/);
  assert.match(entry, /query_stored_official_account_usage\(state, force_refresh, account_id, false\)/);
});

test("账号卡片优先显示额度接口的当前套餐", () => {
  assert.match(panel, /const snapshotPlan = usages\[account\.id\]\?\.status === "ok"/);
  assert.match(panel, /const planType = snapshotPlan \|\| account\.planType;/);
  assert.match(panel, /const plan = formatPlan\(planType\);/);
  assert.match(panel, /planTagClass\(planType\)/);
  assert.match(panel, /isProPlan\(planType\) \? \([\s\S]*?<IconSparkles/);
  assert.doesNotMatch(panel, /planTagClass\(account\.planType\)/);
});
