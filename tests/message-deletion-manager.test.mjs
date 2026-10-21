import assert from "node:assert/strict";
import test from "node:test";
import vm from "node:vm";

import { loadStartupPatchTemplate } from "./helpers/startup-patch.mjs";

const managerSource = `class Manager {
  constructor(hostId="local") {
    this.hostId=hostId;
    this.disposed=false;
    this.value={turns:[]};
    const owner=this;
    this.threadStore={getConversation(id){owner.lastStoreThis=this;owner.lastId=id;return owner.value}};
    this.inactiveThreadUnsubscriber={clearConversationStreamOwnership(){}};
  }
  getHostId(){return this.hostId}
  getConversation(e){return this.threadStore.getConversation(e)}
  getStreamRole(){return {role:"owner"}}
  sendRequest(){}
  updateConversationState(){}
  clear(){this.inactiveThreadUnsubscriber.clearConversationStreamOwnership()}
};globalThis.Manager=Manager;`;

async function fixture() {
  const errors = [];
  const context = vm.createContext({ URL, console: { error(...args) { errors.push(args); } } });
  vm.runInContext(await loadStartupPatchTemplate(), context);
  const patch = (source) => context.__codeyRendererPatchEntry("app://-/assets/app-initial-manager.js", source) ?? source;
  return { context, patch, errors };
}

test("native getConversation registers live local managers preserving result and receiver", async () => {
  const { context, patch } = await fixture();
  const patched = patch(managerSource);
  assert.notEqual(patched, managerSource);
  assert.equal(patch(patched), patched, "registration patch is idempotent");
  vm.runInContext(patched, context);
  const first = new context.Manager();
  assert.equal(first.getConversation("thread-1"), first.value);
  assert.equal(first.lastId, "thread-1");
  assert.equal(first.lastStoreThis, first.threadStore);
  assert.equal(context.__codeyAppServerManagers.get("local"), first);
  const second = new context.Manager();
  assert.equal(second.getConversation("thread-2"), second.value);
  assert.equal(context.__codeyAppServerManagers.get("local"), second);
  first.disposed = true;
  assert.equal(first.getConversation("thread-1"), first.value);
  assert.equal(context.__codeyAppServerManagers.get("local"), second);
  const remote = new context.Manager("remote");
  assert.equal(remote.getConversation("remote-thread"), remote.value);
  assert.equal(context.__codeyAppServerManagers.size, 1);
  assert.equal(context.__codeyAppServerManagers.get("local"), second);
});

test("registration failures leave the native conversation read intact", async () => {
  const { context, patch } = await fixture();
  vm.runInContext(patch(managerSource), context);
  const manager = new context.Manager();
  manager.getHostId = () => { throw new Error("disposed host"); };
  assert.equal(manager.getConversation("thread-1"), manager.value);
  assert.equal(context.__codeyAppServerManagers, undefined);
  context.__codeyAppServerManagers = {};
  manager.getHostId = () => "local";
  assert.equal(manager.getConversation("thread-2"), manager.value);
});

test("nonmatching and ambiguous manager sources remain unchanged", async () => {
  const { patch } = await fixture();
  const unmatched = managerSource.replace("return this.threadStore.getConversation(e)", "return this.threadStore.lookup(e)");
  assert.equal(patch(unmatched), unmatched);
  const ambiguous = `${managerSource}\nclass Other {getConversation(e){return this.threadStore.getConversation(e)}}`;
  assert.equal(patch(ambiguous), ambiguous);
  const unrelated = "class Store {getConversation(e){return this.threadStore.getConversation(e)}}";
  assert.equal(patch(unrelated), unrelated);
});
