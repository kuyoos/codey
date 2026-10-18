import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const source = readFileSync(new URL("../public/codey-inject.js", import.meta.url), "utf8");
const start = source.indexOf("  const appServerManagerRegistryResolverFromSource =");
const end = source.indexOf("  window.__codeyAppServerManagerRegistryResolverFromSource", start);
assert.ok(start >= 0 && end > start);
const resolve = Function(`${source.slice(start, end)}; return appServerManagerRegistryResolverFromSource;`)();
const fixture = "class Registry {getForHostId(e){return this.scope.get(hosts).includes(e)?this.scope.get(managers,e):null}};export {hosts as h,managers as m};";

test("unexported registry resolves existing local state through renamed module exports", () => {
  const hosts = {}, managers = {}, manager = {};
  const lookup = resolve(fixture, { h: hosts, m: managers });
  const calls = [];
  const scope = { get(key, host) {
    calls.push([key, host]);
    if (key === hosts) return ["local"];
    assert.equal(key, managers);
    assert.equal(host, "local");
    return manager;
  } };
  assert.equal(lookup(scope, "local"), manager);
  assert.deepEqual(calls, [[hosts, undefined], [managers, "local"]]);
  calls.length = 0;
  assert.equal(lookup(scope, "remote"), null);
  assert.deepEqual(calls, [[hosts, undefined]], "unregistered hosts must not create a manager");
});

test("lookup accepts whitespace and shorthand exports without evaluating bundle code", () => {
  const hosts = {}, managers = {};
  const text = "throw Error('must not execute'); class R { getForHostId( id ) { return this.scope.get( hosts ).includes( id ) ? this.scope.get( managers, id ) : null; } } export { hosts, managers };";
  const lookup = resolve(text, { hosts, managers });
  assert.equal(typeof lookup, "function");
  assert.equal(lookup({ get() { return null; } }, "local"), null);
});

test("missing exports and ambiguous registry bindings fail without calling a scope", () => {
  assert.equal(resolve(fixture, { h: {} }), null);
  assert.equal(resolve(fixture.replace("export", "const"), { h: {}, m: {} }), null);
  assert.equal(resolve(fixture.replace(".includes(e)", ".has(e)"), { h: {}, m: {} }), null);
  const different = "class Other {getForHostId(e){return this.scope.get(otherHosts).includes(e)?this.scope.get(otherManagers,e):null}}";
  assert.equal(resolve(different + fixture, { h: {}, m: {} }), null);
  const same = {};
  assert.equal(resolve(fixture, { h: same, m: same }), null);
});
