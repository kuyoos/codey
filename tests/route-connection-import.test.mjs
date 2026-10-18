import assert from "node:assert/strict";
import test from "node:test";
import { loadTypeScriptModule } from "./helpers/load-typescript-module.mjs";
import { readSource } from "./helpers/read-source.mjs";

const { parseNewApiConnectionInfo, suggestRouteNamesFromUrl } =
  await loadTypeScriptModule(new URL("../src/routeConnectionImport.ts", import.meta.url));

test("parses a newapi channel connection payload", () => {
  const info = parseNewApiConnectionInfo(
    '{"_type":"newapi_channel_conn","key":"sk-abc123","url":"https://api.pei.ee"}',
  );
  assert.deepEqual(info, { key: "sk-abc123", url: "https://api.pei.ee" });
});

test("tolerates surrounding whitespace and wrapped text", () => {
  const payload = '{"_type":"newapi_channel_conn","key":"sk-abc","url":"https://a.dev"}';
  assert.deepEqual(parseNewApiConnectionInfo(`\n ${payload} \n`), {
    key: "sk-abc",
    url: "https://a.dev",
  });
  assert.deepEqual(parseNewApiConnectionInfo(`连接信息：${payload}（完）`), {
    key: "sk-abc",
    url: "https://a.dev",
  });
  assert.deepEqual(
    parseNewApiConnectionInfo(' { "_type" : "newapi_channel_conn", "key" : " sk-k ", "url" : " https://a.dev " } '),
    { key: "sk-k", url: "https://a.dev" },
  );
});

test("rejects non-connection payloads and malformed input", () => {
  for (const text of [
    "",
    "   ",
    null,
    undefined,
    "sk-abc123",
    "not json",
    "[]",
    "null",
    '{"_type":"other","key":"sk-a","url":"https://a.dev"}',
    '{"key":"sk-a","url":"https://a.dev"}',
    '{"_type":"newapi_channel_conn","key":"sk-a"}',
    '{"_type":"newapi_channel_conn","key":1,"url":"https://a.dev"}',
    '{"_type":"newapi_channel_conn","key":"  ","url":"https://a.dev"}',
  ]) {
    assert.equal(parseNewApiConnectionInfo(text), null, JSON.stringify(text));
  }
});

test("suggests route names from the connection url", () => {
  assert.deepEqual(suggestRouteNamesFromUrl("https://api.pei.ee", 15, 4), {
    name: "api.pei.ee",
    shortName: "pei",
  });
  assert.deepEqual(suggestRouteNamesFromUrl("https://api.newapi.io/v1", 15, 4), {
    name: "api.newapi.io",
    shortName: "newa",
  });
  assert.deepEqual(suggestRouteNamesFromUrl("https://relay.example.com/v1", 15, 4), {
    name: "",
    shortName: "rela",
  });
  assert.deepEqual(suggestRouteNamesFromUrl("https://this-hostname-is-way-too-long.example.com", 15, 4), {
    name: "",
    shortName: "this",
  });
  assert.deepEqual(suggestRouteNamesFromUrl("not a url", 15, 4), {
    name: "",
    shortName: "",
  });
});

test("the new-route dialog wires the quick import affordance", async () => {
  const source = await readSource("src/ModelSection.tsx");
  assert.match(source, /parseNewApiConnectionInfo/);
  assert.match(source, /快速导入/);
  assert.match(source, /navigator\.clipboard\.readText\(\)/);
  assert.match(source, /route-import-panel/);
  assert.match(source, /isNewRouteDraft && routeImportOpen &&/);
});
