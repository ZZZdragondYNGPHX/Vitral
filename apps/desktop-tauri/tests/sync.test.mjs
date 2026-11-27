import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";

const source = readFileSync(new URL("../src/ui.js", import.meta.url), "utf8");
const handlers = source.slice(source.indexOf('all("[data-action]").forEach'), source.indexOf("function renderSyncHealth("));

function click(command, { health = {}, failure = null } = {}) {
  const button = { dataset: { action: command } };
  const calls = [];
  const notices = [];
  let operation;
  const context = vm.createContext({
    all: () => [button],
    CLOUD_ACTIONS: new Set(["sync-now", "sync-status", "storage", "restore-from-koofr"]),
    prepareCloudAction: async () => { calls.push("prepare"); },
    run: task => { operation = task(); },
    editSeq: 0,
    dirty: false,
    api: async cmd => {
      calls.push(cmd);
      if (failure && cmd !== "sync-status") throw new Error(failure);
      return ["sync-status", "sync-now"].includes(cmd) ? health : {};
    },
    renderSyncHealth() {},
    renderUsage() {},
    setNotice: (...args) => notices.push(args),
  });
  vm.runInContext(handlers, context);
  button.onclick();
  return { operation, calls, notices };
}

test("云端同步完成后不再附带扫描云盘用量", async () => {
  const result = click("sync-now", { health: { cloud_configured: true, last_cloud_sync_ms: 1 } });
  await result.operation;
  assert.deepEqual(result.calls, ["prepare", "sync-now"]);
  assert.deepEqual(result.notices.at(-1), ["已完成云端同步", "ok"]);
});

for (const health of [
  { cloud_configured: false },
  { cloud_configured: true, last_cloud_sync_ms: 0 },
  { cloud_configured: true, last_cloud_sync_ms: 1, pure_offline: true },
  { cloud_configured: true, last_cloud_sync_ms: 1, last_cloud_error: "网络错误" },
]) {
  test(`未同步或失败时不能误报成功 ${JSON.stringify(health)}`, async () => {
    const result = click("sync-now", { health });
    await assert.rejects(result.operation, /云端同步未完成|云端同步失败/);
    assert.equal(result.notices.some(([, tone]) => tone === "ok"), false);
  });
}

test("检查链路发出轻量连接请求，不扫描云盘用量", async () => {
  const result = click("sync-status");
  await result.operation;
  assert.deepEqual(result.calls, ["prepare", "sync-status", "cloud-check"]);
  assert.equal(result.notices.at(-1)[0], "云端连接正常，已刷新同步状态");
});

test("连接检查失败不得显示连接正常", async () => {
  const result = click("sync-status", { failure: "WebDAV 认证失败" });
  await assert.rejects(result.operation, /认证失败/);
  assert.equal(result.notices.some(([, tone]) => tone === "ok"), false);
});
