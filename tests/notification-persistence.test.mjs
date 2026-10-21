import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const appSource = readFileSync(
  new URL("../src/App.tsx", import.meta.url),
  "utf8",
);
const dialogSource = readFileSync(
  new URL("../src/notifications/NotificationChannelDialog.tsx", import.meta.url),
  "utf8",
);

test("notification dialog waits for the channel configuration to persist", () => {
  assert.match(dialogSource, /onSave: \(channel: NotificationChannel\) => Promise<boolean>/);
  assert.match(dialogSource, /if \(await onSave\(draft\)\) onOpenChange\(false\)/);
  assert.match(appSource, /async function persistNotificationChannels/);
  assert.match(appSource, /await persist\(\{[\s\S]*?webhook:[\s\S]*?channels/);
});
