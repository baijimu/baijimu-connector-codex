import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { test } from "node:test";
import vm from "node:vm";
import * as stateFunctions from "../ui/state.mjs";

class Element {
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
}

async function failedInstallerPage() {
  const elements = new Map();
  const calls = [];
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
      createElement: () => new Element(),
      addEventListener: () => {},
      querySelectorAll: () => [],
    },
    window: {
      setTimeout: () => {},
      baijimuLocalApp: { version: 1, async invoke(operation, args) {
        calls.push({ operation, args });
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
  return { elements, calls };
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
