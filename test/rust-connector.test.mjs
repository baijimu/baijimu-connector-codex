import assert from "node:assert/strict";
import { once } from "node:events";
import { execFileSync, spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdirSync, mkdtempSync, rmSync, symlinkSync } from "node:fs";
import { createServer as createHttpServer } from "node:http";
import { chmod, mkdir, mkdtemp, readFile, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { test } from "node:test";
import { setTimeout as delay } from "node:timers/promises";
import { fileURLToPath } from "node:url";
import { delimiter, dirname, join, resolve } from "node:path";

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);
const root = resolve(__dirname, "..");
const cli = join(root, "target", "debug", "baijimu-connector-codex");
const fakeCodex = join(__dirname, "fake-codex-app-server.mjs");
const originalHome = process.env.HOME;
const fakeCodexHome = mkdtempSync(join(tmpdir(), "codex-rust-test-home-"));
const fakeCodexBin = join(fakeCodexHome, ".local", "bin");
mkdirSync(fakeCodexBin, { recursive: true });
symlinkSync(process.execPath, join(fakeCodexBin, "codex"));
if (originalHome) {
  process.env.CARGO_HOME ||= join(originalHome, ".cargo");
  process.env.RUSTUP_HOME ||= join(originalHome, ".rustup");
}
process.env.HOME = fakeCodexHome;
process.env.PATH = `${fakeCodexBin}${delimiter}${process.env.PATH || ""}`;
process.on("exit", () => rmSync(fakeCodexHome, { recursive: true, force: true }));

async function freePort() {
  const { createServer } = await import("node:net");
  const server = createServer();
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const port = server.address().port;
  server.close();
  await once(server, "close");
  return port;
}

async function postJson(port, path, body = {}) {
  const response = await fetch(`http://127.0.0.1:${port}${path}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
  const payload = await response.json();
  assert.equal(response.status, 200, JSON.stringify(payload));
  return payload;
}

async function postManagementJson(port, token, path, body = {}) {
  const response = await fetch(`http://127.0.0.1:${port}${path}`, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      authorization: `Bearer ${token}`,
    },
    body: JSON.stringify(body),
  });
  const payload = await response.json();
  assert.equal(response.status, 200, JSON.stringify(payload));
  assert.equal(payload.ok, true, JSON.stringify(payload));
  return payload.data;
}

async function waitForHealth(port) {
  const deadline = Date.now() + 30_000;
  while (Date.now() < deadline) {
    try {
      const response = await fetch(`http://127.0.0.1:${port}/healthz`, {
        signal: AbortSignal.timeout(1_000),
      });
      if (response.ok) return;
    } catch {
      // Keep polling until the server is ready.
    }
    await delay(100);
  }
  throw new Error("connector did not become healthy");
}

async function waitForReadiness(port) {
  const deadline = Date.now() + 30_000;
  while (Date.now() < deadline) {
    try {
      const response = await fetch(`http://127.0.0.1:${port}/readyz`, {
        signal: AbortSignal.timeout(1_000),
      });
      const payload = await response.json();
      if (response.ok) return payload;
    } catch {
      // Keep polling until initialization completes.
    }
    await delay(100);
  }
  throw new Error("connector did not become ready");
}

async function stopConnector(proc, port) {
  if (proc.exitCode !== null || proc.signalCode !== null) return;
  try {
    await fetch(`http://127.0.0.1:${port}/__shutdown`, { method: "POST" });
  } catch {
    proc.kill("SIGTERM");
  }
  const exited = once(proc, "exit");
  await Promise.race([
    exited,
    delay(1000).then(() => proc.kill("SIGKILL")),
  ]);
}

test("host-managed foreground runtime records and safely stops its verified PID", async () => {
  execFileSync("cargo", ["build"], { cwd: root, stdio: "inherit" });
  const port = await freePort();
  const connectorHome = await mkdtemp(join(tmpdir(), "codex-host-managed-"));
  const env = {
    ...process.env,
    BAIJIMU_CONNECTOR_DATA_DIR: connectorHome,
    CODEX_CONNECTOR_ENABLE_TEST_SHUTDOWN: "1",
    CODEX_CONNECTOR_TEST_STARTUP_DELAY_MS: "1500",
  };
  const proc = spawn(cli, [
    "start",
    "--port",
    String(port),
  ], {
    cwd: root,
    stdio: ["ignore", "pipe", "pipe"],
    env,
  });

  try {
    await waitForHealth(port);
    const managementToken = (await readFile(
      join(connectorHome, "management-token"),
      "utf8",
    )).trim();
    assert.ok(managementToken.length >= 32);
    const liveness = await (await fetch(`http://127.0.0.1:${port}/healthz`)).json();
    assert.equal(liveness.ok, true);
    assert.equal(liveness.status.startup.status, "initializing");

    const initializing = await fetch(`http://127.0.0.1:${port}/readyz`);
    assert.equal(initializing.status, 503);
    assert.equal((await initializing.json()).error.code, "connector_initializing");

    const ready = await waitForReadiness(port);
    assert.equal(ready.status.startup.status, "ready");
    const recordedPid = Number(
      (await readFile(join(connectorHome, "connector.pid"), "utf8")).trim(),
    );
    assert.equal(recordedPid, proc.pid);

    const stopped = JSON.parse(execFileSync(cli, [
      "stop",
      "--port",
      String(port),
    ], {
      cwd: root,
      encoding: "utf8",
      env,
    }));
    assert.equal(stopped.ok, true);
    assert.equal(stopped.stopped, true);
    assert.equal(stopped.pid, proc.pid);

    if (proc.exitCode === null && proc.signalCode === null) {
      await Promise.race([
        once(proc, "exit"),
        delay(2000).then(() => {
          throw new Error("verified connector process did not exit");
        }),
      ]);
    }
  } finally {
    await stopConnector(proc, port);
    await rm(connectorHome, { recursive: true, force: true });
  }
});

test("a competing startup cannot rotate management state before acquiring the port", async () => {
  execFileSync("cargo", ["build"], { cwd: root, stdio: "inherit" });
  const port = await freePort();
  const activeHome = await mkdtemp(join(tmpdir(), "codex-active-instance-"));
  const competingHome = await mkdtemp(join(tmpdir(), "codex-competing-instance-"));
  const env = {
    ...process.env,
    BAIJIMU_CONNECTOR_DATA_DIR: activeHome,
    CODEX_CONNECTOR_ENABLE_TEST_SHUTDOWN: "1",
  };
  const proc = spawn(cli, ["start", "--port", String(port)], {
    cwd: root,
    stdio: ["ignore", "pipe", "pipe"],
    env,
  });

  try {
    await waitForHealth(port);
    const result = spawnSync(cli, ["start", "--port", String(port)], {
      cwd: root,
      encoding: "utf8",
      env: {
        ...env,
        BAIJIMU_CONNECTOR_DATA_DIR: competingHome,
      },
    });
    assert.notEqual(result.status, 0);
    assert.ok(result.stderr.trim().length > 0);
    await assert.rejects(
      readFile(join(competingHome, "management-token"), "utf8"),
      { code: "ENOENT" },
    );
  } finally {
    await stopConnector(proc, port);
    await rm(activeHome, { recursive: true, force: true });
    await rm(competingHome, { recursive: true, force: true });
  }
});

test("startup failures keep liveness online and expose the readiness root cause", async () => {
  execFileSync("cargo", ["build"], { cwd: root, stdio: "inherit" });
  const port = await freePort();
  const connectorHome = await mkdtemp(join(tmpdir(), "codex-startup-failure-"));
  const expectedError = "injected CODEX_HOME synchronization failure";
  const env = {
    ...process.env,
    BAIJIMU_CONNECTOR_DATA_DIR: connectorHome,
    CODEX_CONNECTOR_ENABLE_TEST_SHUTDOWN: "1",
    CODEX_CONNECTOR_TEST_STARTUP_FAILURE: expectedError,
  };
  const proc = spawn(cli, ["start", "--port", String(port)], {
    cwd: root,
    stdio: ["ignore", "pipe", "pipe"],
    env,
  });

  try {
    await waitForHealth(port);
    const managementToken = (await readFile(
      join(connectorHome, "management-token"),
      "utf8",
    )).trim();
    assert.ok(managementToken.length >= 32);
    const liveness = await fetch(`http://127.0.0.1:${port}/healthz`);
    assert.equal(liveness.status, 200);

    const readiness = await fetch(`http://127.0.0.1:${port}/readyz`);
    const payload = await readiness.json();
    assert.equal(readiness.status, 503);
    assert.equal(payload.status.startup.status, "failed");
    assert.equal(payload.error.code, "connector_initialization_failed");
    assert.equal(payload.error.message, expectedError);
  } finally {
    await stopConnector(proc, port);
    await rm(connectorHome, { recursive: true, force: true });
  }
});

test("rust connector forwards Codex app-server calls", async () => {
  execFileSync("cargo", ["build"], { cwd: root, stdio: "inherit" });
  const port = await freePort();
  const connectorHome = await mkdtemp(join(tmpdir(), "codex-app-data-"));
  const configHome = join(connectorHome, "config");
  await mkdir(configHome, { recursive: true });
  const eventTokenPath = join(connectorHome, "event-token");
  await writeFile(eventTokenPath, "test-event-token\n", { mode: 0o600 });
  const emittedEvents = [];
  let turnCompletedAttempts = 0;
  const eventServer = createHttpServer(async (request, response) => {
    let body = "";
    for await (const chunk of request) body += chunk;
    const event = JSON.parse(body);
    emittedEvents.push(event);
    if (event.event === "codexTurnCompleted") {
      turnCompletedAttempts += 1;
      if (turnCompletedAttempts === 1) {
        response.writeHead(503, { "content-type": "application/json" });
        response.end(JSON.stringify({ accepted: false }));
        return;
      }
    }
    response.writeHead(202, { "content-type": "application/json" });
    response.end(JSON.stringify({ accepted: true, durable: true, eventId: event.eventId }));
  });
  eventServer.listen(0, "127.0.0.1");
  await once(eventServer, "listening");
  const eventPort = eventServer.address().port;
  const proc = spawn(cli, [
    "start",
    "--port",
    String(port),
    "--codex-args",
    JSON.stringify([fakeCodex]),
  ], {
    cwd: root,
    stdio: ["ignore", "pipe", "pipe"],
    env: {
      ...process.env,
      BAIJIMU_CONFIG_HOME: configHome,
      BAIJIMU_CONNECTOR_DATA_DIR: connectorHome,
      BAIJIMU_CONNECTOR_EVENT_ENDPOINT: `http://127.0.0.1:${eventPort}/events`,
      BAIJIMU_CONNECTOR_EVENT_TOKEN_FILE: eventTokenPath,
      CODEX_CONNECTOR_ENABLE_TEST_SHUTDOWN: "1",
    },
  });

  try {
    await waitForHealth(port);

    const unauthorized = await fetch(`http://127.0.0.1:${port}/management/v1/credential-state`);
    assert.equal(unauthorized.status, 401);
    const managementToken = (await readFile(join(connectorHome, "management-token"), "utf8")).trim();
    assert.ok(managementToken.length >= 32);
    const readiness = await postManagementJson(
      port,
      managementToken,
      "/management/v1/setup/ensure-ready",
    );
    assert.equal(readiness.readiness, "needs_workspace");
    const authorizedUnknown = await fetch(`http://127.0.0.1:${port}/management/v1/unknown`, {
      headers: { authorization: `Bearer ${managementToken}` },
    });
    assert.equal(authorizedUnknown.status, 404);

    const managedSessions = await postManagementJson(
      port,
      managementToken,
      "/management/v1/codex/sessions",
      { limit: 5, sortKey: "updated_at", sortDirection: "desc" },
    );
    assert.equal(managedSessions.result.data[0].id, "thr_listed");
    assert.equal(managedSessions.result.data[0].requestParams.sortKey, "updated_at");
    assert.equal(managedSessions.result.data[0].threadRuntimeStatus.type, "idle");
    assert.equal(managedSessions.result.data[0].isInProgress, false);
    assert.equal(managedSessions.result.data[0].hasUnreadTurn, false);

    const managedThread = await postManagementJson(
      port,
      managementToken,
      "/management/v1/codex/sessions/start",
      { model: "gpt-test", cwd: "/tmp/project" },
    );
    assert.equal(managedThread.result.thread.id, "thr_test");

    const managedTurn = await postManagementJson(
      port,
      managementToken,
      "/management/v1/codex/turns/start",
      { threadId: "thr_test", input: "Say hello" },
    );
    assert.equal(managedTurn.result.turn.id, "turn_test");

    const unreadSessions = await postManagementJson(
      port,
      managementToken,
      "/management/v1/codex/sessions",
      { limit: 5 },
    );
    assert.equal(unreadSessions.result.data[0].hasUnreadTurn, true);

    const readState = await postJson(port, "/invoke/setThreadReadState", {
      threadId: "thr_test",
      hasUnreadTurn: false,
      observedUpdatedAt: unreadSessions.result.data[0].updatedAt,
    });
    assert.equal(readState.data.result.threadId, "thr_test");
    assert.equal(readState.data.result.hasUnreadTurn, false);

    const thread = await postJson(port, "/invoke/startThread", { model: "gpt-test" });
    assert.equal(thread.data.result.thread.id, "thr_test");
    assert.equal(thread.data.status, undefined);

    const threads = await postJson(port, "/invoke/listThreads", { limit: 5 });
    assert.equal(threads.data.result.data[0].id, "thr_test");
    assert.equal(threads.data.result.data[0].hasUnreadTurn, false);
    assert.equal(threads.data.result.data[0].requestParams.sortKey, "updated_at");

    const turns = await postJson(port, "/invoke/listThreadTurns", {
      threadId: "thr_read",
      limit: 8,
      sortDirection: "desc",
      itemsView: "full",
    });
    assert.equal(turns.data.result.data[0].id, "turn_recent");

    const turn = await postJson(port, "/invoke/startTurn", {
      threadId: "thr_test",
      input: "Say hello",
    });
    assert.equal(turn.data.result.turn.id, "turn_test");

    const events = await postJson(port, "/invoke/recentEvents", {
      afterSequence: 0,
      limit: 20,
    });
    assert.ok(events.data.events.some((event) => event.method === "item/agentMessage/delta"));

    const eventDeadline = Date.now() + 5_000;
    while (turnCompletedAttempts < 2 && Date.now() < eventDeadline) {
      await delay(25);
    }
    assert.equal(turnCompletedAttempts, 2, JSON.stringify(emittedEvents));
    const domainAttempts = emittedEvents.filter(
      (event) => event.event === "codexTurnCompleted",
    );
    assert.equal(domainAttempts[0].eventId, domainAttempts[1].eventId);
    assert.deepEqual(domainAttempts[1].payload, {
      schemaVersion: 1,
      threadId: "thr_test",
      turnId: "turn_test",
      status: "completed",
      completedAt: 1786400000,
      durationMs: 25,
      error: null,
      occurredAt: domainAttempts[1].occurredAt,
      source: "codex-app-server",
      sourceMethod: "turn/completed",
      connectorVersion: "1.2.62",
    });
    assert.ok(emittedEvents.some((event) => event.event === "codexNotification"));
  } finally {
    await stopConnector(proc, port);
    await new Promise((resolvePromise) => eventServer.close(resolvePromise));
    await rm(connectorHome, { recursive: true, force: true });
  }
});

test("rust connector finds the installer-managed Codex binary without a GUI PATH entry", {
  skip: process.platform === "win32",
}, async () => {
  execFileSync("cargo", ["build"], { cwd: root, stdio: "inherit" });
  const port = await freePort();
  const rootHome = await mkdtemp(join(tmpdir(), "codex-binary-resolution-"));
  const connectorHome = join(rootHome, "connector-data");
  const codexHome = join(rootHome, "codex-home");
  const localBin = join(rootHome, ".local", "bin");
  const installedCodex = join(localBin, "codex");
  await mkdir(localBin, { recursive: true });
  await mkdir(codexHome, { recursive: true });
  await symlink(process.execPath, installedCodex);

  const proc = spawn(cli, [
    "start",
    "--port",
    String(port),
    "--codex-args",
    JSON.stringify([fakeCodex]),
  ], {
    cwd: root,
    stdio: ["ignore", "pipe", "pipe"],
    env: {
      ...process.env,
      HOME: rootHome,
      PATH: "/usr/bin:/bin",
      CODEX_HOME: codexHome,
      BAIJIMU_CONNECTOR_DATA_DIR: connectorHome,
      CODEX_CONNECTOR_ENABLE_TEST_SHUTDOWN: "1",
    },
  });

  try {
    await waitForHealth(port);
    const threads = await postJson(port, "/invoke/listThreads", { limit: 1 });
    assert.equal(threads.data.result.data[0].id, "thr_listed");

    const status = await postJson(port, "/invoke/status");
    assert.equal(status.data.appServer.requestedCodexBinary, undefined);
    assert.equal(status.data.appServer.codexBinary, installedCodex);
    assert.equal(status.data.appServer.codexBinaryResolution.resolved, installedCodex);
    assert.equal(
      status.data.appServer.codexBinaryResolution.source,
      "official_user_install",
    );
    assert.equal(status.data.appServer.codexBinaryResolution.mode, "auto");
    assert.equal(status.data.appServer.codexBinaryResolution.error, null);
  } finally {
    await stopConnector(proc, port);
    await rm(rootHome, { recursive: true, force: true });
  }
});

test("rust connector rejects the removed Codex binary override option", () => {
  execFileSync("cargo", ["build"], { cwd: root, stdio: "inherit" });
  const result = spawnSync(cli, [
    "start",
    "--codex-binary",
    join(tmpdir(), "removed-codex-override"),
  ], {
    cwd: root,
    encoding: "utf8",
    env: process.env,
  });
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /no longer supported/);
});

test("rust connector lists Codex projects and falls back for turns", async () => {
  execFileSync("cargo", ["build"], { cwd: root, stdio: "inherit" });
  const port = await freePort();
  const codexHome = await mkdtemp(join(tmpdir(), "codex-home-"));
  const connectorHome = await mkdtemp(join(tmpdir(), "codex-app-data-"));
  const savedProject = join(codexHome, "saved-project");
  const activeProject = join(codexHome, "active-project");
  const trustedProject = join(codexHome, "trusted-project");
  await mkdir(savedProject, { recursive: true });
  await mkdir(activeProject, { recursive: true });
  await mkdir(trustedProject, { recursive: true });
  await writeFile(join(codexHome, ".codex-global-state.json"), JSON.stringify({
    "project-order": [savedProject],
    "electron-saved-workspace-roots": [savedProject, activeProject],
    "active-workspace-roots": [activeProject],
    "pinned-project-ids": [savedProject],
  }));
  await writeFile(join(codexHome, "config.toml"), `[projects."${trustedProject}"]\ntrust_level = "trusted"\n`);

  const proc = spawn(cli, [
    "start",
    "--port",
    String(port),
    "--codex-args",
    JSON.stringify([fakeCodex]),
  ], {
    cwd: root,
    stdio: ["ignore", "pipe", "pipe"],
    env: {
      ...process.env,
      CODEX_HOME: codexHome,
      BAIJIMU_CONNECTOR_DATA_DIR: connectorHome,
      CODEX_CONNECTOR_ENABLE_TEST_SHUTDOWN: "1",
      CODEX_FAKE_DISABLE_TURNS_LIST: "1",
    },
  });

  try {
    await waitForHealth(port);

    const response = await postJson(port, "/invoke/listProjects", { limit: 20 });
    const byPath = new Map(response.data.result.projects.map((project) => [project.path, project]));
    assert.equal(response.data.result.total, 4);
    assert.equal(byPath.get(savedProject).pinned, true);
    assert.equal(byPath.get(activeProject).active, true);
    assert.equal(byPath.get(trustedProject).trustLevel, "trusted");
    assert.ok(byPath.get("/tmp/listed").sources.includes("threads"));

    const turns = await postJson(port, "/invoke/listThreadTurns", {
      threadId: "thr_read",
      limit: 8,
      itemsView: "full",
    });
    assert.equal(turns.data.result.data[0].id, "turn_read");
    assert.equal(turns.data.result.fallback, "thread/read");

    const events = await postJson(port, "/invoke/recentEvents", {
      afterSequence: 0,
      limit: 20,
    });
    assert.ok(events.data.events.some((event) => event.method === "connector/threadTurnsListFallback"));
  } finally {
    await stopConnector(proc, port);
    await rm(codexHome, { recursive: true, force: true });
    await rm(connectorHome, { recursive: true, force: true });
  }
});

test("rust connector resolves current Codex project IDs to their real roots", async () => {
  execFileSync("cargo", ["build"], { cwd: root, stdio: "inherit" });
  const port = await freePort();
  const codexHome = await mkdtemp(join(tmpdir(), "codex-home-current-projects-"));
  const connectorHome = await mkdtemp(join(tmpdir(), "codex-app-data-"));
  const projectRoot = "/tmp/listed";
  await writeFile(join(codexHome, ".codex-global-state.json"), JSON.stringify({
    "project-order": ["local-listed", "local-unresolved"],
    "pinned-project-ids": ["local-listed"],
    "local-projects": {
      "local-listed": {
        id: "local-listed",
        name: "Listed Project",
        rootPaths: [projectRoot],
      },
    },
  }));

  const proc = spawn(cli, [
    "start",
    "--port",
    String(port),
    "--codex-args",
    JSON.stringify([fakeCodex]),
  ], {
    cwd: root,
    stdio: ["ignore", "pipe", "pipe"],
    env: {
      ...process.env,
      CODEX_HOME: codexHome,
      BAIJIMU_CONNECTOR_DATA_DIR: connectorHome,
      CODEX_CONNECTOR_ENABLE_TEST_SHUTDOWN: "1",
    },
  });

  try {
    await waitForHealth(port);
    const response = await postJson(port, "/invoke/listProjects", { limit: 20 });
    assert.equal(response.data.result.total, 1);
    const [project] = response.data.result.projects;
    assert.equal(project.id, projectRoot);
    assert.equal(project.path, projectRoot);
    assert.equal(project.projectId, "local-listed");
    assert.equal(project.projectName, "Listed Project");
    assert.equal(project.title, "Listed Project");
    assert.deepEqual(project.rootPaths, [projectRoot]);
    assert.equal(project.pinned, true);
    assert.equal(project.sessionCount, 1);
    assert.deepEqual(project.sources, ["saved", "pinned", "threads"]);
    assert.ok(!project.path.includes("local-unresolved"));
  } finally {
    await stopConnector(proc, port);
    await rm(codexHome, { recursive: true, force: true });
    await rm(connectorHome, { recursive: true, force: true });
  }
});

test("rust connector launches an isolated Baijimu workspace and can launch the personal profile again", async () => {
  execFileSync("cargo", ["build"], { cwd: root, stdio: "inherit" });
  const port = await freePort();
  const rootHome = await mkdtemp(join(tmpdir(), "codex-auth-switch-"));
  const personalHome = join(rootHome, "personal-codex");
  const connectorHome = join(rootHome, "connector-data");
  const fakeCliLog = join(rootHome, "baijimu-cli.log");
  const fakeCliScript = join(rootHome, "fake-baijimu.mjs");
  const fakeCli = process.platform === "win32"
    ? join(rootHome, "fake-baijimu.cmd")
    : join(rootHome, "fake-baijimu");
  const profileId = "test:user-25:client-device-test:workspace-1390";
  const legacyWorkspaceHome = join(connectorHome, "codex-profiles", "baijimu", "test", "user-25", "client-device-test", "workspace-1390");
  const profileKey = createHash("sha256").update(profileId).digest("hex").slice(0, 24);
  const workspaceHome = join(fakeCodexHome, ".baijimu", "codex", "p", profileKey);
  await mkdir(personalHome, { recursive: true });
  await mkdir(legacyWorkspaceHome, { recursive: true });
  await writeFile(fakeCliScript, `
import { appendFileSync } from "node:fs";
const args = process.argv.slice(2);
appendFileSync(process.env.BAIJIMU_FAKE_LOG, JSON.stringify(args) + "\\n");
let output;
if (args.join(" ") === "auth status") {
  output = {
    authenticated: true,
    baseUrl: "https://api.baijimu.test",
    configuredCurrentWorkspaceId: 1390,
    credentialCount: 1,
    currentWorkspaceId: 1390,
    sharedAuthPath: "owned-by-baijimu-cli",
    verification: null,
    workspaceIds: [1390],
  };
} else if (args[0] === "workspace" && args[1] === "list") {
  output = {
    currentWorkspaceId: 1390,
    data: { list: [{ id: 1390, name: "研发工作区" }], total: 1, totalPages: 1 },
    errorCode: "0",
    systemCurrentTime: 1,
    value: "成功",
  };
} else if (args.join(" ") === "workspace get 1390 --json") {
  output = {
    data: { id: 1390, name: "研发工作区" },
    errorCode: "0",
    systemCurrentTime: 1,
    value: "成功",
  };
} else if (args.join(" ") === "llm-credential create --json --workspace-id 1390 --show-secret") {
  output = {
    created: true,
    keyType: "llmCredential",
    workspaceId: 1390,
    projectId: null,
    agentConfigId: null,
    agentSessionId: null,
    sessionId: null,
    maskedLlmCredential: "worksp****test",
    credential: "workspace-key",
    llmCredential: "workspace-key",
    apiKey: "workspace-key",
  };
} else {
  process.stderr.write("unexpected fake baijimu CLI arguments: " + args.join(" "));
  process.exit(2);
}
process.stdout.write(JSON.stringify(output));
`);
  if (process.platform === "win32") {
    await writeFile(fakeCli, `@echo off\r\n"${process.execPath}" "${fakeCliScript}" %*\r\n`);
  } else {
    await writeFile(fakeCli, `#!/bin/sh\nexec "${process.execPath}" "${fakeCliScript}" "$@"\n`);
    await chmod(fakeCli, 0o755);
  }
  await writeFile(join(personalHome, "auth.json"), JSON.stringify({
    auth_mode: "chatgpt",
    tokens: { access_token: "chatgpt-test-access", account_id: "acct-test" },
  }));
  await writeFile(join(personalHome, "config.toml"), 'model = "gpt-test"\nmodel_provider = "openai"\n');
  await writeFile(join(legacyWorkspaceHome, "auth.json"), JSON.stringify({ OPENAI_API_KEY: "workspace-key", auth_mode: "apikey" }));
  await writeFile(join(legacyWorkspaceHome, "config.toml"), `model_provider = "baijimu-router"\n[model_providers.baijimu-router]\nbase_url = "https://router.baijimu.com/api/claudecode/v1"\n`);
  await writeFile(join(connectorHome, "codex-credentials.json"), JSON.stringify({
    version: 2,
    activeMode: "chatgpt",
    activeProfileId: null,
    activeWorkspaceId: null,
    profiles: [{
      profileId,
      environment: "test",
      userId: 25,
      clientId: "device-test",
      workspaceId: 1390,
      workspaceName: "研发工作区",
      model: "gpt-5.6-sol",
      activatedAtEpochSeconds: 0,
      codexHome: legacyWorkspaceHome,
    }],
  }));
  await writeFile(join(connectorHome, "setup-status.json"), JSON.stringify({
    status: "succeeded",
    workspaceId: 1390,
    message: "Codex 应用初始化已完成",
    error: null,
    startedAtEpochSeconds: 1,
    completedAtEpochSeconds: 2,
    installerStatus: null,
  }));
  const proc = spawn(cli, [
    "start", "--port", String(port),
    "--codex-args", JSON.stringify([fakeCodex]),
  ], {
    cwd: root,
    stdio: ["ignore", "pipe", "pipe"],
    env: {
      ...process.env,
      CODEX_HOME: personalHome,
      BAIJIMU_CONNECTOR_DATA_DIR: connectorHome,
      BAIJIMU_FAKE_LOG: fakeCliLog,
      CODEX_CONNECTOR_BAIJIMU_BINARY: fakeCli,
      CODEX_CONNECTOR_ENABLE_TEST_SHUTDOWN: "1",
    },
  });

  try {
    await waitForHealth(port);
    const managementToken = (await readFile(join(connectorHome, "management-token"), "utf8")).trim();
    const readiness = await postManagementJson(
      port,
      managementToken,
      "/management/v1/setup/ensure-ready",
    );
    assert.equal(readiness.readiness, "ready");
    assert.equal(readiness.setup.workspaceId, 1390);
    const workspaceState = await postManagementJson(port, managementToken, "/management/v1/codex/launch", {
      mode: "baijimu", workspaceId: 1390,
    });
    assert.equal(workspaceState.activeMode, "baijimu");
    assert.equal(workspaceState.activeWorkspaceId, 1390);
    assert.equal(workspaceState.activeCodexHome, workspaceHome);
    await assert.rejects(readFile(join(legacyWorkspaceHome, "auth.json")), { code: "ENOENT" });
    assert.equal(workspaceState.externalCodexHome, personalHome);
    assert.equal(workspaceState.legacyGlobalCodexHome.restoreRequired, false);
    const workspaceStatus = await postJson(port, "/invoke/status");
    assert.equal(workspaceStatus.data.appServer.codexHome, workspaceHome);

    const personalState = await postManagementJson(port, managementToken, "/management/v1/codex/launch", { mode: "chatgpt" });
    assert.equal(personalState.activeMode, "chatgpt");
    assert.equal(personalState.activeCodexHome, personalHome);
    const personalStatus = await postJson(port, "/invoke/status");
    assert.equal(personalStatus.data.appServer.codexHome, personalHome);
    assert.equal(JSON.parse(await readFile(join(personalHome, "auth.json"), "utf8")).auth_mode, "chatgpt");
    const cliCalls = (await readFile(fakeCliLog, "utf8"))
      .trim()
      .split("\n")
      .map((line) => JSON.parse(line));
    assert.ok(cliCalls.some((args) => args.join(" ") === "auth status"));
    assert.ok(cliCalls.some((args) => args[0] === "workspace" && args[1] === "list"));
    assert.ok(cliCalls.some((args) => args.join(" ") === "workspace get 1390 --json"));
    assert.ok(cliCalls.some((args) => (
      args[0] === "llm-credential"
      && args[1] === "create"
      && args.includes("--json")
      && args.includes("--workspace-id")
      && args.includes("1390")
      && args.includes("--show-secret")
    )));
  } finally {
    await stopConnector(proc, port);
    await rm(rootHome, { recursive: true, force: true });
  }
});

test("rust connector daemon mode writes pid file", async () => {
  execFileSync("cargo", ["build"], { cwd: root, stdio: "inherit" });
  const port = await freePort();
  const home = await mkdtemp(join(tmpdir(), "baijimu-connector-codex-"));

  try {
    const output = execFileSync(cli, [
      "start",
      "--daemon",
      "--port",
      String(port),
      "--codex-args",
      JSON.stringify([fakeCodex]),
    ], {
      cwd: root,
      encoding: "utf8",
      env: {
        ...process.env,
        BAIJIMU_CONNECTOR_DATA_DIR: home,
        CODEX_CONNECTOR_ENABLE_TEST_SHUTDOWN: "1",
      },
    });
    const started = JSON.parse(output);
    assert.equal(started.ok, true);
    assert.equal(started.url, `http://127.0.0.1:${port}`);

    await waitForHealth(port);
    await delay(750);
    await waitForHealth(port);
    const pid = Number((await readFile(join(home, "connector.pid"), "utf8")).trim());
    assert.equal(pid, started.pid);

    await fetch(`http://127.0.0.1:${port}/__shutdown`, { method: "POST" });
  } finally {
    await rm(home, { recursive: true, force: true });
  }
});
