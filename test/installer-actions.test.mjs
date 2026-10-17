import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { test } from "node:test";
import vm from "node:vm";
import * as stateFunctions from "../ui/state.mjs";

class Element {
  constructor(tagName = "div") { this.tagName = tagName; }
  hidden = false;
  disabled = false;
  value = "";
  textContent = "";
  children = [];
  dataset = {};
  style = {};
  listeners = {};
  append(...children) { this.children.push(...children); }
  replaceChildren(...children) { this.children = children; }
  addEventListener(name, fn) { this.listeners[name] = fn; }
  setAttribute() {}
  scrollIntoView() {}
  querySelector(selector) {
    for (const child of this.children) {
      if (selector.startsWith(".") ? child.className?.split(" ").includes(selector.slice(1)) : child.tagName === selector) return child;
      const found = child.querySelector(selector);
      if (found) return found;
    }
    return null;
  }
  insertBefore(child, before) {
    this.children = this.children.filter((item) => item !== child);
    const index = before ? this.children.indexOf(before) : this.children.length;
    this.children.splice(index, 0, child);
  }
}

async function failedInstallerPage(discover = async () => ({ currentWorkspaceId: 42 }), overrides = {}) {
  const elements = new Map();
  const calls = [];
  const timers = [];
  const setup = {
    status: "failed", retryable: true, error: "安装此应用需要管理员权限",
    installerStatus: { platform: "windows", steps: [], packageRecovery: {
      packagePath: "C:\\用户\\package.msix", sha256: "a".repeat(64), requiresElevation: true,
    } },
  };
  const context = vm.createContext({
    ...stateFunctions,
    document: {
      getElementById(id) {
        if (!elements.has(id)) elements.set(id, new Element());
        return elements.get(id);
      },
      createElement: (tag) => new Element(tag),
      addEventListener: () => {},
      querySelectorAll: () => [],
    },
    window: {
      setTimeout: (fn) => timers.push(fn),
      baijimuLocalApp: { version: 1, async invoke(operation, args) {
        calls.push({ operation, args });
        if (overrides[operation]) return overrides[operation](args);
        if (operation === "discoverWorkspaces") return discover();
        if (operation === "credentialState") return { currentWorkspaceId: 42 };
        if (operation === "setupState") return setup;
        if (operation === "ensureCodexReady") return { readiness: "failed", setup };
        if (operation === "setupRetry") return { status: "running" };
        if (operation === "revealInstallerPackage") return { opened: true };
        throw new Error(`Unexpected operation: ${operation}`);
      } },
    },
  });
  const app = (await readFile(new URL("../ui/app.js", import.meta.url), "utf8"))
    .replace(/^import\s*\{[\s\S]*?\}\s*from\s*"\.\/state\.mjs";/, "");
  vm.runInContext(app, context);
  await new Promise(setImmediate);
  return { elements, calls, timers, context };
}

test("both failure-page retry buttons explicitly request elevation", async () => {
  for (const id of ["setup-action-button", "error-retry-button"]) {
    const { elements, calls } = await failedInstallerPage();
    assert.equal(elements.get(id).textContent, "以管理员权限重试安装");
    assert.equal(elements.get(id).disabled, false);
    elements.get(id).listeners.click();
    await new Promise(setImmediate);
    const retry = calls.find((call) => call.operation === "setupRetry");
    assert.equal(retry.args.elevate, true);
    assert.equal(retry.args.workspaceId, 42);
    assert.equal(elements.get("setup-action-button").disabled, true);
  }
});

test("open-directory button uses the backend record without submitting a path", async () => {
  const { elements, calls } = await failedInstallerPage();
  const button = elements.get("setup-reveal-package-button");
  assert.equal(button.hidden, false);
  await button.listeners.click();
  const request = calls.find((call) => call.operation === "revealInstallerPackage");
  assert.deepEqual(Object.keys(request.args), []);
  assert.match(elements.get("message").textContent, /选中文件/);
});


test("slow or failed workspace discovery does not hide local installation state", async () => {
  let reject;
  const pending = new Promise((_, rejectPromise) => { reject = rejectPromise; });
  const { elements, calls } = await failedInstallerPage(() => pending);
  assert.ok(calls.some((call) => call.operation === "discoverWorkspaces"));
  assert.equal(elements.get("setup-action-button").textContent, "以管理员权限重试安装");
  reject(new Error("平台发现超时"));
  await new Promise(setImmediate);
  assert.match(elements.get("warning").textContent, /平台发现超时/);
  assert.notEqual(elements.get("runtime-status-badge").textContent, "检查失败");
});

for (const status of ["failed", "interrupted"]) {
  test(`running installation becoming ${status} clears progress notice and keeps recovery`, async () => {
    let checks = 0;
    const terminal = { status, retryable: true, error: "需要管理员权限", installerStatus: {
      platform: "windows", steps: [], packageRecovery: {
        packagePath: "C:\\package.msix", sha256: "a".repeat(64), requiresElevation: true,
      },
    } };
    const page = await failedInstallerPage(undefined, {
      setupState: () => ++checks === 1 ? { status: "running" } : terminal,
      ensureCodexReady: () => ({ readiness: "initializing", setup: { status: "running" } }),
    });
    assert.equal(page.elements.get("message").hidden, false);
    page.timers.shift()();
    await new Promise(setImmediate);
    assert.equal(page.elements.get("message").hidden, true);
    assert.equal(page.elements.get("message").textContent, "");
    assert.equal(page.elements.get("error").hidden, false);
    assert.equal(page.elements.get("setup-action-button").textContent, "以管理员权限重试安装");
    assert.equal(page.elements.get("setup-reveal-package-button").hidden, false);
    assert.equal(page.elements.get("setup-action-button").disabled, false);
  });
}

test("retry returning an immediate failure never displays started or UAC progress", async () => {
  const { elements } = await failedInstallerPage(undefined, {
    setupRetry: () => ({ status: "failed", retryable: true, error: "授权被取消" }),
  });
  await elements.get("error-retry-button").listeners.click();
  await new Promise(setImmediate);
  assert.equal(elements.get("message").hidden, true);
  assert.match(elements.get("error-text").textContent, /授权被取消/);
});

test("poll failure clears progress and exposes recheck instead of stale install progress", async () => {
  const page = await failedInstallerPage();
  await page.elements.get("error-retry-button").listeners.click();
  await new Promise(setImmediate);
  vm.runInContext('invokeManagement = async () => { throw new Error("连接已断开"); }', page.context);
  page.timers.shift()();
  await new Promise(setImmediate);
  assert.equal(page.elements.get("message").hidden, true);
  assert.equal(page.elements.get("error-retry-button").textContent, "重新检查");
});

for (const status of ["failed", "interrupted", "running"]) {
  test(`${status} setup renders persisted running and pending steps truthfully`, async () => {
    const setup = { status, retryable: true, message: "安装任务状态", installerStatus: { steps: [
      { index: 1, name: "下载", state: "completed", detail: "已下载" },
      { index: 2, name: "安装", state: "running", detail: "正在安装" },
      { index: 3, name: "配置", state: "pending" },
    ] } };
    const page = await failedInstallerPage(undefined, {
      setupState: () => setup,
      ensureCodexReady: () => ({ readiness: status === "running" ? "initializing" : "failed", setup }),
    });
    const rows = page.elements.get("setup-step-list").children;
    assert.equal(rows[0].querySelector("em").textContent, "完成");
    assert.equal(rows[1].querySelector("em").textContent,
      status === "running" ? "进行中" : status === "interrupted" ? "已中断" : "已停止");
    assert.equal(rows[1].className, status === "running" ? "setup-step running" : "setup-step failed");
    assert.equal(rows[1].querySelector("small").textContent,
      status === "running" ? "正在安装" : "停止前进度：正在安装");
    assert.equal(rows[2].querySelector("em").textContent, status === "running" ? "等待" : "未执行");
    assert.equal(setup.installerStatus.steps[1].state, "running");
  });
}


test("opening and refreshing a completed legacy setup never invokes a model probe or reinstall", async () => {
  const setup = {
    schemaVersion: 2, status: "succeeded", retryable: true,
    completedAtEpochSeconds: null,
    lastError: "百积木路由验证已尝试 1 次但暂未通过：HTTP 400",
  };
  const { elements, calls, timers } = await failedInstallerPage(undefined, {
    setupState: async () => setup,
    ensureCodexReady: async () => ({ readiness: "ready", setup }),
  });
  elements.get("refresh-button").listeners.click();
  await new Promise(setImmediate);
  assert.equal(elements.get("management-workspace-panel").hidden, false);
  assert.equal(elements.get("setup-panel").hidden, true);
  assert.equal(elements.get("setup-action-button").hidden, true);
  assert.equal(elements.get("runtime-status-badge").textContent, "可用");
  assert.equal(elements.get("error").hidden, true);
  assert.equal(elements.has("workspace-route-notice"), false);
  assert.equal(timers.length, 0);
  assert.ok(calls.every(({ operation }) => [
    "credentialState", "setupState", "discoverWorkspaces", "ensureCodexReady",
  ].includes(operation)), JSON.stringify(calls));
});
