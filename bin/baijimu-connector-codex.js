#!/usr/bin/env node

import { createServer } from "node:http";
import { spawn } from "node:child_process";
import { createInterface } from "node:readline";
import { accessSync, chmodSync, closeSync, constants, existsSync, mkdirSync, openSync, readFileSync, realpathSync, renameSync, statSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { basename, delimiter, dirname, isAbsolute, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const VERSION = "1.2.62";
const DEFAULT_HOST = "127.0.0.1";
const DEFAULT_PORT = 18110;
const DEFAULT_LISTEN = "stdio://";
const DEFAULT_REQUEST_TIMEOUT_MS = 120000;
const MAX_EVENTS = 1000;
const DEFAULT_PROJECT_LIMIT = 100;
const DEFAULT_PROJECT_THREAD_PAGE_LIMIT = 100;
const DEFAULT_PROJECT_THREAD_MAX_PAGES = 100;
const DEFAULT_THREAD_SORT_KEY = "updated_at";
const DEFAULT_THREAD_SORT_DIRECTION = "desc";

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);
const packageRoot = resolve(__dirname, "..");

function isDesktopInternalCodex(path) {
  const matches = (value) => {
    const normalized = value.replaceAll("\\", "/").toLowerCase();
    return normalized.includes("/windowsapps/")
      || normalized.endsWith("/app/resources/codex.exe")
      || normalized.endsWith(".app/contents/resources/codex")
      || normalized.includes("/baijimu-appserver-login/codex.exe");
  };
  if (matches(path)) return true;
  try {
    return matches(realpathSync(path));
  } catch {
    return false;
  }
}

function isLaunchable(path) {
  try {
    if (!statSync(path).isFile() || isDesktopInternalCodex(path)) return false;
    if (process.platform !== "win32") accessSync(path, constants.X_OK);
    return true;
  } catch {
    return false;
  }
}

function resolveCodexBinary() {
  const userHome = process.env.HOME || process.env.USERPROFILE || homedir();
  const binaryName = process.platform === "win32" ? "codex.exe" : "codex";
  const candidates = [[join(userHome, ".local", "bin", binaryName), "official user install"]];
  if (process.platform === "darwin") {
    candidates.push(["/opt/homebrew/bin/codex", "official system install"]);
    candidates.push(["/usr/local/bin/codex", "official system install"]);
  } else if (process.platform === "linux") {
    candidates.push(["/usr/local/bin/codex", "official system install"]);
    candidates.push(["/usr/bin/codex", "official system install"]);
    candidates.push(["/snap/bin/codex", "official system install"]);
  } else if (process.platform === "win32" && process.env.LOCALAPPDATA) {
    const statePath = join(process.env.LOCALAPPDATA, "OpenAI", "Codex", "cli", "current.json");
    try {
      const managed = JSON.parse(readFileSync(statePath, "utf8").replace(/^\uFEFF/, ""));
      if (isAbsolute(managed.binaryPath || "")) {
        candidates.push([managed.binaryPath, "Connector-managed official install"]);
      }
    } catch {
      // Continue with PATH discovery when the managed state does not exist.
    }
  }
  const pathExtensions = process.platform === "win32"
    ? (process.env.PATHEXT || ".COM;.EXE;.BAT;.CMD").split(";")
    : [""];
  for (const directory of (process.env.PATH || "").split(delimiter).filter(Boolean)) {
    for (const extension of pathExtensions) {
      candidates.push([join(directory, `codex${extension}`), "process PATH"]);
    }
  }
  for (const [candidate] of candidates) {
    if (isLaunchable(candidate)) return candidate;
  }
  throw new Error("Codex CLI was not found or is not executable. Install the official Codex CLI in a standard install location or the user login environment.");
}

class HttpError extends Error {
  constructor(statusCode, message) {
    super(message);
    this.statusCode = statusCode;
  }
}

class CodexAppServerClient {
  constructor(options = {}) {
    this.codexBinary = resolveCodexBinary();
    this.listen = options.listen || DEFAULT_LISTEN;
    this.extraArgs = options.extraArgs || [];
    this.requestTimeoutMs = options.requestTimeoutMs || DEFAULT_REQUEST_TIMEOUT_MS;
    this.clientInfo = options.clientInfo || {
      name: "baijimu_connector_codex",
      title: "Baijimu Codex Connector",
      version: VERSION,
    };
    this.experimentalApi = options.experimentalApi ?? true;
    this.proc = null;
    this.rl = null;
    this.initialized = false;
    this.nextId = 1;
    this.pending = new Map();
    this.events = [];
    this.eventSequence = 0;
    this.startedAt = null;
    this.lastExit = null;
    this.initializing = null;
  }

  status() {
    return {
      connector: {
        name: "@baijimu/connector-codex",
        version: VERSION,
        pid: process.pid,
      },
      appServer: {
        running: Boolean(this.proc && this.proc.exitCode === null && !this.proc.killed),
        initialized: this.initialized,
        pid: this.proc?.pid ?? null,
        codexBinary: this.codexBinary,
        listen: this.listen,
        startedAt: this.startedAt,
        lastExit: this.lastExit,
      },
      events: {
        latestSequence: this.eventSequence,
        retained: this.events.length,
      },
    };
  }

  async ensureStarted() {
    if (this.proc && this.proc.exitCode === null && !this.proc.killed && this.initialized) {
      return;
    }

    if (!this.proc || this.proc.exitCode !== null || this.proc.killed) {
      this.startProcess();
    }

    if (!this.initialized) {
      if (!this.initializing) {
        this.initializing = this.initialize().finally(() => {
          this.initializing = null;
        });
      }
      await this.initializing;
    }
  }

  startProcess() {
    const args = this.extraArgs.length > 0
      ? this.extraArgs
      : ["app-server", "--listen", this.listen];
    this.proc = spawn(this.codexBinary, args, {
      cwd: process.cwd(),
      stdio: ["pipe", "pipe", "pipe"],
      env: process.env,
      detached: process.platform !== "win32",
    });
    this.startedAt = new Date().toISOString();
    this.lastExit = null;
    this.initialized = false;
    this.initializing = null;

    this.rl = createInterface({ input: this.proc.stdout });
    this.rl.on("line", (line) => this.handleLine(line));
    this.proc.stderr.on("data", (chunk) => {
      this.pushEvent("connector/codexStderr", {
        text: chunk.toString("utf8"),
      });
    });
    this.proc.on("exit", (code, signal) => {
      this.lastExit = { code, signal, at: new Date().toISOString() };
      this.initialized = false;
      this.rejectPending(new Error(`codex app-server exited: code=${code} signal=${signal}`));
    });
    this.proc.on("error", (error) => {
      this.lastExit = { error: error.message, at: new Date().toISOString() };
      this.initialized = false;
      this.rejectPending(error);
    });
  }

  async initialize() {
    const params = {
      clientInfo: this.clientInfo,
      capabilities: {
        experimentalApi: this.experimentalApi,
      },
    };
    const result = await this.request("initialize", params, 30000, { skipEnsureStarted: true });
    this.sendNotification("initialized", {});
    this.initialized = true;
    return result;
  }

  async request(method, params = {}, timeoutMs = this.requestTimeoutMs, options = {}) {
    if (!options.skipEnsureStarted) {
      await this.ensureStarted();
    }
    if (!this.proc || !this.proc.stdin.writable) {
      throw new Error("codex app-server is not writable");
    }

    const id = this.nextId++;
    const message = { method, id, params };
    const result = await new Promise((resolvePromise, rejectPromise) => {
      const timeout = setTimeout(() => {
        this.pending.delete(id);
        rejectPromise(new Error(`codex app-server request timed out: ${method}`));
      }, timeoutMs);

      this.pending.set(id, {
        method,
        resolve: resolvePromise,
        reject: rejectPromise,
        timeout,
      });

      this.proc.stdin.write(`${JSON.stringify(message)}\n`, (error) => {
        if (error) {
          clearTimeout(timeout);
          this.pending.delete(id);
          rejectPromise(error);
        }
      });
    });
    return result;
  }

  sendNotification(method, params = {}) {
    if (!this.proc || !this.proc.stdin.writable) {
      throw new Error("codex app-server is not writable");
    }
    this.proc.stdin.write(`${JSON.stringify({ method, params })}\n`);
  }

  handleLine(line) {
    if (!line.trim()) {
      return;
    }

    let message;
    try {
      message = JSON.parse(line);
    } catch (error) {
      this.pushEvent("connector/parseError", {
        line,
        error: error.message,
      });
      return;
    }

    if (Object.prototype.hasOwnProperty.call(message, "id")) {
      const pending = this.pending.get(message.id);
      if (!pending) {
        this.pushEvent("connector/unmatchedResponse", message);
        return;
      }
      this.pending.delete(message.id);
      clearTimeout(pending.timeout);
      if (message.error) {
        const error = new Error(message.error.message || `codex app-server error for ${pending.method}`);
        error.code = message.error.code;
        error.data = message.error.data;
        pending.reject(error);
      } else {
        pending.resolve(message.result ?? null);
      }
      return;
    }

    this.pushEvent(message.method || "codex/notification", message.params ?? message);
  }

  pushEvent(method, params) {
    if (method === "turn/completed" && typeof params?.threadId === "string" && params.threadId) {
      markThreadUnread(params.threadId);
    }
    const event = {
      sequence: ++this.eventSequence,
      receivedAt: new Date().toISOString(),
      method,
      params,
    };
    this.events.push(event);
    if (this.events.length > MAX_EVENTS) {
      this.events.splice(0, this.events.length - MAX_EVENTS);
    }
  }

  recentEvents({ afterSequence = 0, limit = 100 } = {}) {
    const boundedLimit = Math.max(1, Math.min(Number(limit) || 100, 500));
    return {
      latestSequence: this.eventSequence,
      events: this.events
        .filter((event) => event.sequence > Number(afterSequence || 0))
        .slice(-boundedLimit),
    };
  }

  rejectPending(error) {
    for (const [id, pending] of this.pending.entries()) {
      clearTimeout(pending.timeout);
      pending.reject(error);
      this.pending.delete(id);
    }
  }

  async shutdown() {
    const child = this.proc;
    if (!child || child.exitCode !== null) {
      return;
    }
    child.stdin?.end();
    const exited = new Promise((resolvePromise) => child.once("exit", resolvePromise));
    killChildProcess(child, "SIGTERM");
    await Promise.race([
      exited,
      new Promise((resolvePromise) => setTimeout(resolvePromise, 500)).then(() => {
        if (child.exitCode === null) {
          killChildProcess(child, "SIGKILL");
        }
      }),
    ]);
    if (child.exitCode === null) {
      await Promise.race([
        exited,
        new Promise((resolvePromise) => setTimeout(resolvePromise, 500)),
      ]);
    }
  }
}

function killChildProcess(child, signal) {
  if (process.platform !== "win32" && child.pid) {
    try {
      process.kill(-child.pid, signal);
      return;
    } catch {
      // Fall back to direct child signaling below.
    }
  }
  child.kill(signal);
}

function parseArgs(argv) {
  const [command = "help", ...rest] = argv;
  const options = { command, positional: [] };
  for (let index = 0; index < rest.length; index += 1) {
    const arg = rest[index];
    if (!arg.startsWith("--")) {
      options.positional.push(arg);
      continue;
    }
    const [key, inlineValue] = arg.slice(2).split("=", 2);
    if (key === "codex-binary") {
      throw new HttpError(2, "--codex-binary is no longer supported; Codex CLI discovery is automatic");
    }
    if (["daemon", "help", "version"].includes(key)) {
      options[key] = true;
      continue;
    }
    const value = inlineValue ?? rest[++index];
    if (value === undefined) {
      throw new HttpError(2, `missing value for --${key}`);
    }
    options[toCamelCase(key)] = value;
  }
  return options;
}

function toCamelCase(value) {
  return value.replace(/-([a-z])/g, (_, letter) => letter.toUpperCase());
}

function connectorHome() {
  return process.env.CODEX_CONNECTOR_HOME || join(homedir(), ".baijimu-connector-codex");
}

function codexHome() {
  return process.env.CODEX_HOME || join(homedir(), ".codex");
}

function pidPath() {
  return join(connectorHome(), "connector.pid");
}

function logPath() {
  return join(connectorHome(), "connector.log");
}

function threadReadStatePath() {
  return join(connectorHome(), "thread-read-state.json");
}

function ensureConnectorHome() {
  mkdirSync(connectorHome(), { recursive: true });
}

function readJsonEnv(name, fallback) {
  const raw = process.env[name];
  if (!raw) {
    return fallback;
  }
  return JSON.parse(raw);
}

function serverOptions(options) {
  return {
    host: options.host || process.env.CODEX_CONNECTOR_HOST || DEFAULT_HOST,
    port: Number(options.port || process.env.CODEX_CONNECTOR_PORT || DEFAULT_PORT),
    listen: options.listen || process.env.CODEX_CONNECTOR_LISTEN || DEFAULT_LISTEN,
    extraArgs: options.codexArgs
      ? JSON.parse(options.codexArgs)
      : readJsonEnv("CODEX_CONNECTOR_CODEX_ARGS", []),
    requestTimeoutMs: Number(options.requestTimeoutMs || process.env.CODEX_CONNECTOR_REQUEST_TIMEOUT_MS || DEFAULT_REQUEST_TIMEOUT_MS),
  };
}

async function connectorHealth(options, timeoutMs = 1000) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const response = await fetch(`http://${options.host}:${options.port}/healthz`, {
      signal: controller.signal,
    });
    if (!response.ok) {
      return null;
    }
    return await response.json();
  } catch {
    return null;
  } finally {
    clearTimeout(timer);
  }
}

async function waitForConnectorHealth(options, expectedPid) {
  for (let attempt = 0; attempt < 50; attempt += 1) {
    const health = await connectorHealth(options, 500);
    if (health?.ok) {
      if (!expectedPid || health.status?.connector?.pid === expectedPid) {
        return health;
      }
    }
    await new Promise((resolvePromise) => setTimeout(resolvePromise, 100));
  }
  return null;
}

async function daemonize(options) {
  ensureConnectorHome();
  const existingHealth = await connectorHealth(options);
  if (existingHealth?.ok) {
    const pid = existingHealth.status?.connector?.pid ?? null;
    if (pid) {
      writeFileSync(pidPath(), `${pid}\n`);
    }
    console.log(JSON.stringify({
      ok: true,
      pid,
      existing: true,
      url: `http://${options.host}:${options.port}`,
      logPath: logPath(),
    }));
    return;
  }

  const childArgs = [
    __filename,
    "start",
    "--host",
    options.host,
    "--port",
    String(options.port),
    "--listen",
    options.listen,
  ];
  if (options.extraArgs?.length) {
    childArgs.push("--codex-args", JSON.stringify(options.extraArgs));
  }

  const stdoutFd = openSync(logPath(), "a");
  const stderrFd = openSync(logPath(), "a");
  let child;
  try {
    child = spawn(process.execPath, childArgs, {
      cwd: packageRoot,
      detached: true,
      stdio: ["ignore", stdoutFd, stderrFd],
      env: process.env,
    });
  } finally {
    closeSync(stdoutFd);
    closeSync(stderrFd);
  }
  child.unref();
  const health = await waitForConnectorHealth(options, child.pid);
  if (!health?.ok) {
    throw new Error(`connector daemon did not become healthy at http://${options.host}:${options.port}`);
  }
  const pid = health.status?.connector?.pid ?? child.pid;
  writeFileSync(pidPath(), `${pid}\n`);
  console.log(JSON.stringify({
    ok: true,
    pid,
    url: `http://${options.host}:${options.port}`,
    logPath: logPath(),
  }));
}

async function readJsonRequest(request) {
  const chunks = [];
  for await (const chunk of request) {
    chunks.push(chunk);
  }
  if (chunks.length === 0) {
    return {};
  }
  const body = Buffer.concat(chunks).toString("utf8").trim();
  return body ? JSON.parse(body) : {};
}

function writeJson(response, statusCode, payload) {
  response.writeHead(statusCode, {
    "content-type": "application/json; charset=utf-8",
  });
  response.end(`${JSON.stringify(payload)}\n`);
}

function normalizeInput(input) {
  if (typeof input === "string") {
    return [{ type: "text", text: input }];
  }
  if (Array.isArray(input)) {
    return input;
  }
  return input;
}

function mergeParams(body, base = {}) {
  const params = body.params && typeof body.params === "object" ? body.params : {};
  return { ...base, ...params };
}

function pickParams(body, keys) {
  const params = {};
  for (const key of keys) {
    if (body[key] !== undefined) {
      params[key] = body[key];
    }
  }
  return params;
}

function readJsonFile(path, fallback) {
  if (!existsSync(path)) {
    return fallback;
  }
  try {
    return JSON.parse(readFileSync(path, "utf8"));
  } catch {
    return fallback;
  }
}

function readThreadState() {
  const path = threadReadStatePath();
  if (!existsSync(path)) return { version: 1, threads: {} };
  const value = JSON.parse(readFileSync(path, "utf8"));
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new Error(`invalid Connector thread state: ${path}`);
  }
  return {
    version: 1,
    threads: value.threads && typeof value.threads === "object" && !Array.isArray(value.threads)
      ? value.threads
      : {},
  };
}

function writeThreadState(state) {
  ensureConnectorHome();
  const path = threadReadStatePath();
  const temporaryPath = `${path}.${process.pid}.${Date.now()}.tmp`;
  writeFileSync(temporaryPath, `${JSON.stringify(state, null, 2)}\n`, { mode: 0o600 });
  if (process.platform !== "win32") chmodSync(temporaryPath, 0o600);
  renameSync(temporaryPath, path);
}

function desktopUnreadThreadIds() {
  const globalStatePath = join(codexHome(), ".codex-global-state.json");
  if (!existsSync(globalStatePath)) return new Set();
  const globalState = JSON.parse(readFileSync(globalStatePath, "utf8"));
  const byHost = globalState?.["unread-thread-ids-by-host-v1"];
  if (byHost === undefined) return new Set();
  if (!byHost || typeof byHost !== "object" || Array.isArray(byHost)) {
    throw new Error(`invalid Codex desktop unread state: ${globalStatePath}`);
  }
  return new Set(uniqueStrings(byHost.local));
}

function normalizeRuntimeStatus(status) {
  if (status && typeof status === "object" && !Array.isArray(status)) return { ...status };
  if (typeof status === "string" && status) return { type: status };
  return { type: "notLoaded" };
}

function latestTurnStatus(thread, runtimeStatus) {
  const latest = Array.isArray(thread.turns) ? thread.turns.at(-1) : null;
  const status = latest?.turn?.status ?? latest?.status;
  if (typeof status === "string" && status) return status;
  return runtimeStatus.type === "active" ? "inProgress" : null;
}

function enrichThreads(items) {
  const state = readThreadState();
  const desktopUnread = desktopUnreadThreadIds();
  let changed = false;
  const data = items.map((raw) => {
    const thread = normalizeThreadListItem(raw);
    if (!thread || typeof thread !== "object" || Array.isArray(thread)) return thread;
    const threadId = thread.threadId ?? thread.sessionId ?? thread.id;
    if (typeof threadId !== "string" || !threadId) return thread;
    const updatedAt = thread.updatedAt ?? null;
    const isDesktopUnread = desktopUnread.has(threadId);
    const threadRuntimeStatus = normalizeRuntimeStatus(thread.status);
    const turnStatus = latestTurnStatus(thread, threadRuntimeStatus);
    const existing = state.threads[threadId];
    const entry = existing && typeof existing === "object" && !Array.isArray(existing)
      ? { ...existing }
      : {
          hasUnreadTurn: isDesktopUnread,
          observedUpdatedAt: updatedAt,
          observedRuntimeStatusType: threadRuntimeStatus.type,
          observedLatestTurnStatus: turnStatus,
          observedDesktopUnread: isDesktopUnread,
        };
    if (existing === undefined) changed = true;
    const revisionAdvanced = entry.observedUpdatedAt !== null
      && entry.observedUpdatedAt !== undefined
      && updatedAt !== null
      && entry.observedUpdatedAt !== updatedAt;
    const activityFinished = entry.observedRuntimeStatusType === "active" && threadRuntimeStatus.type !== "active"
      || entry.observedLatestTurnStatus === "inProgress" && turnStatus !== "inProgress";
    if (revisionAdvanced && activityFinished) {
      entry.hasUnreadTurn = true;
    }
    if (!entry.observedDesktopUnread && isDesktopUnread) entry.hasUnreadTurn = true;
    if (updatedAt !== null) entry.observedUpdatedAt = updatedAt;
    entry.observedRuntimeStatusType = threadRuntimeStatus.type;
    entry.observedLatestTurnStatus = turnStatus;
    entry.observedDesktopUnread = isDesktopUnread;
    if (JSON.stringify(existing) !== JSON.stringify(entry)) changed = true;
    state.threads[threadId] = entry;

    const activeFlags = Array.isArray(threadRuntimeStatus.activeFlags) ? threadRuntimeStatus.activeFlags : [];
    return {
      ...thread,
      threadRuntimeStatus,
      activeFlags,
      isInProgress: threadRuntimeStatus.type === "active" || turnStatus === "inProgress",
      latestTurnStatus: turnStatus,
      hasUnreadTurn: entry.hasUnreadTurn === true,
    };
  });
  if (changed) writeThreadState(state);
  return data;
}

function setThreadReadState(threadId, hasUnreadTurn, observedUpdatedAt) {
  const state = readThreadState();
  const existing = state.threads[threadId] && typeof state.threads[threadId] === "object"
    ? state.threads[threadId]
    : {};
  const entry = {
    ...existing,
    hasUnreadTurn,
    observedDesktopUnread: desktopUnreadThreadIds().has(threadId),
    ...(observedUpdatedAt !== undefined ? { observedUpdatedAt } : {}),
  };
  state.threads[threadId] = entry;
  writeThreadState(state);
  return { threadId, hasUnreadTurn, observedUpdatedAt: entry.observedUpdatedAt ?? null };
}

function markThreadUnread(threadId) {
  const state = readThreadState();
  state.threads[threadId] = {
    ...(state.threads[threadId] || {}),
    hasUnreadTurn: true,
  };
  writeThreadState(state);
}

function uniqueStrings(values) {
  return [...new Set((Array.isArray(values) ? values : []).filter((value) => typeof value === "string" && value.trim()))];
}

function normalizeProjectPath(value) {
  if (typeof value !== "string") {
    return null;
  }
  const trimmed = value.trim();
  if (!trimmed) {
    return null;
  }
  const expanded = trimmed === "~" ? homedir() : trimmed.replace(/^~(?=\/|\\)/, homedir());
  return resolve(expanded);
}

function normalizeStateProjectPath(value) {
  if (typeof value !== "string") {
    return null;
  }
  const trimmed = value.trim();
  if (!trimmed) {
    return null;
  }
  const expanded = trimmed === "~" ? homedir() : trimmed.replace(/^~(?=\/|\\)/u, homedir());
  return isAbsolute(expanded) ? resolve(expanded) : null;
}

function displayProjectTitle(path) {
  const name = basename(path);
  return name || path;
}

function parseCodexProjectConfig(path) {
  if (!existsSync(path)) {
    return new Map();
  }
  const projects = new Map();
  let currentProject = null;
  for (const line of readFileSync(path, "utf8").split(/\r?\n/u)) {
    const section = line.match(/^\s*\[projects\."((?:\\"|[^"])*)"\]\s*$/u);
    if (section) {
      currentProject = normalizeProjectPath(section[1].replace(/\\"/gu, "\""));
      if (currentProject && !projects.has(currentProject)) {
        projects.set(currentProject, {});
      }
      continue;
    }
    if (!currentProject) {
      continue;
    }
    const trustLevel = line.match(/^\s*trust_level\s*=\s*"([^"]+)"\s*$/u);
    if (trustLevel) {
      projects.get(currentProject).trustLevel = trustLevel[1];
    }
  }
  return projects;
}

function upsertProject(projects, projectPath, source, fields = {}) {
  const normalizedPath = normalizeProjectPath(projectPath);
  if (!normalizedPath) {
    return null;
  }
  let project = projects.get(normalizedPath);
  if (!project) {
    project = {
      id: normalizedPath,
      path: normalizedPath,
      cwd: normalizedPath,
      title: displayProjectTitle(normalizedPath),
      exists: existsSync(normalizedPath),
      pinned: false,
      active: false,
      trustLevel: null,
      projectId: null,
      projectName: null,
      rootPaths: [normalizedPath],
      sessionCount: 0,
      lastActiveAt: null,
      gitBranch: null,
      gitOriginUrl: null,
      sources: [],
    };
    projects.set(normalizedPath, project);
  }
  if (source && !project.sources.includes(source)) {
    project.sources.push(source);
  }
  Object.assign(project, fields);
  return project;
}

function uniqueStateProjectRoots(projects) {
  const seen = new Set();
  return projects.filter((project) => {
    if (seen.has(project.path)) {
      return false;
    }
    seen.add(project.path);
    return true;
  });
}

function stateProjectRoots(globalState) {
  const projects = [];
  const localProjects = globalState?.["local-projects"];
  if (localProjects && typeof localProjects === "object" && !Array.isArray(localProjects)) {
    for (const [fallbackId, value] of Object.entries(localProjects)) {
      if (!value || typeof value !== "object" || Array.isArray(value)) {
        continue;
      }
      const projectId = typeof value.id === "string" && value.id.trim() ? value.id : fallbackId;
      const projectName = typeof value.name === "string" && value.name.trim() ? value.name : null;
      const rootPaths = uniqueStrings(value.rootPaths)
        .map(normalizeStateProjectPath)
        .filter(Boolean);
      for (const path of rootPaths) {
        projects.push({ path, projectId, projectName, rootPaths });
      }
    }
  }

  const assignmentRoots = new Map();
  const assignments = globalState?.["thread-project-assignments"];
  if (assignments && typeof assignments === "object" && !Array.isArray(assignments)) {
    for (const assignment of Object.values(assignments)) {
      const projectId = typeof assignment?.projectId === "string" ? assignment.projectId.trim() : "";
      const path = normalizeStateProjectPath(assignment?.cwd);
      if (!projectId || !path) {
        continue;
      }
      const roots = assignmentRoots.get(projectId) ?? [];
      if (!roots.includes(path)) {
        roots.push(path);
      }
      assignmentRoots.set(projectId, roots);
    }
  }
  for (const [projectId, rootPaths] of assignmentRoots) {
    if (projects.some((project) => project.projectId === projectId)) {
      continue;
    }
    for (const path of rootPaths) {
      projects.push({ path, projectId, projectName: null, rootPaths });
    }
  }
  return uniqueStateProjectRoots(projects);
}

function resolveStateProjectReferences(globalState, references) {
  const knownProjects = stateProjectRoots(globalState);
  const resolved = [];
  for (const reference of uniqueStrings(references)) {
    const matches = knownProjects.filter((project) => project.projectId === reference);
    if (matches.length > 0) {
      resolved.push(...matches);
      continue;
    }
    const path = normalizeStateProjectPath(reference);
    if (!path) {
      continue;
    }
    resolved.push(knownProjects.find((project) => project.path === path) ?? {
      path,
      projectId: null,
      projectName: null,
      rootPaths: [path],
    });
  }
  return uniqueStateProjectRoots(resolved);
}

function stateProjectFields(project) {
  return {
    ...(project.projectId ? { projectId: project.projectId } : {}),
    ...(project.projectName ? { projectName: project.projectName, title: project.projectName } : {}),
    rootPaths: project.rootPaths,
  };
}

function threadTimestamp(thread) {
  return thread?.recencyAt
    ?? thread?.recency_at
    ?? thread?.updatedAt
    ?? thread?.updated_at
    ?? thread?.createdAt
    ?? thread?.created_at
    ?? null;
}

function timestampEpochMs(value) {
  if (value === null || value === undefined || value === "") {
    return null;
  }
  if (typeof value === "number" && Number.isFinite(value)) {
    return value < 10_000_000_000 ? value * 1000 : value;
  }
  if (typeof value === "string") {
    const trimmed = value.trim();
    if (!trimmed) {
      return null;
    }
    if (/^-?\d+(\.\d+)?$/.test(trimmed)) {
      const numeric = Number(trimmed);
      return Number.isFinite(numeric) ? timestampEpochMs(numeric) : null;
    }
    const parsed = Date.parse(trimmed);
    return Number.isFinite(parsed) ? parsed : null;
  }
  return null;
}

function newerTimestamp(left, right) {
  if (!left) {
    return right ?? null;
  }
  if (!right) {
    return left;
  }
  const leftMs = timestampEpochMs(left);
  const rightMs = timestampEpochMs(right);
  if (leftMs === null) {
    return right;
  }
  if (rightMs === null) {
    return left;
  }
  return rightMs > leftMs ? right : left;
}

function compareTimestampsDesc(left, right) {
  const leftMs = timestampEpochMs(left) ?? 0;
  const rightMs = timestampEpochMs(right) ?? 0;
  return rightMs - leftMs;
}

function withThreadSortDefaults(params) {
  return {
    sortKey: DEFAULT_THREAD_SORT_KEY,
    sortDirection: DEFAULT_THREAD_SORT_DIRECTION,
    ...params,
  };
}

function applyThreadToProject(project, thread) {
  project.sessionCount += 1;
  project.lastActiveAt = newerTimestamp(project.lastActiveAt, threadTimestamp(thread));
  const gitInfo = thread.gitInfo ?? thread.git_info ?? null;
  if (gitInfo && typeof gitInfo === "object") {
    project.gitBranch ??= gitInfo.branch ?? gitInfo.gitBranch ?? null;
    project.gitOriginUrl ??= gitInfo.originUrl ?? gitInfo.origin_url ?? gitInfo.remoteUrl ?? null;
  }
}

function normalizeThreadListItem(item) {
  if (!item || typeof item !== "object" || Array.isArray(item)) {
    return item;
  }
  if (!item.thread || typeof item.thread !== "object" || Array.isArray(item.thread)) {
    return item;
  }
  return {
    ...item,
    ...item.thread,
    thread: item.thread,
  };
}

async function readThreadProjects(body, client, projects) {
  const includeThreadStats = body.includeThreadStats ?? body.params?.includeThreadStats ?? true;
  if (!includeThreadStats) {
    return;
  }

  let cursor = body.threadCursor ?? body.params?.threadCursor ?? null;
  const maxPages = Math.max(1, Math.min(
    Number(body.maxThreadPages ?? body.params?.maxThreadPages ?? DEFAULT_PROJECT_THREAD_MAX_PAGES) || DEFAULT_PROJECT_THREAD_MAX_PAGES,
    500,
  ));
  const limit = Math.max(1, Math.min(
    Number(body.threadPageLimit ?? body.params?.threadPageLimit ?? DEFAULT_PROJECT_THREAD_PAGE_LIMIT) || DEFAULT_PROJECT_THREAD_PAGE_LIMIT,
    100,
  ));

  for (let page = 0; page < maxPages; page += 1) {
    const result = await client.request("thread/list", {
      cursor,
      limit,
      sortKey: body.sortKey ?? body.params?.sortKey ?? DEFAULT_THREAD_SORT_KEY,
      sortDirection: body.sortDirection ?? body.params?.sortDirection ?? DEFAULT_THREAD_SORT_DIRECTION,
      archived: body.archived ?? body.params?.archived,
      useStateDbOnly: body.useStateDbOnly ?? body.params?.useStateDbOnly,
    }, body.timeoutMs);
    const threads = Array.isArray(result?.data) ? result.data.map(normalizeThreadListItem) : [];
    for (const thread of threads) {
      const project = upsertProject(projects, thread.cwd, "threads");
      if (project) {
        applyThreadToProject(project, thread);
      }
    }
    cursor = result?.nextCursor ?? null;
    if (!cursor || threads.length === 0) {
      return;
    }
  }
}

function projectSortIndex(projectPath, order) {
  const index = order.indexOf(projectPath);
  return index === -1 ? Number.MAX_SAFE_INTEGER : index;
}

async function listProjects(body, client) {
  const home = codexHome();
  const globalState = readJsonFile(join(home, ".codex-global-state.json"), {});
  const configuredProjects = parseCodexProjectConfig(join(home, "config.toml"));
  const projects = new Map();

  const savedRoots = uniqueStateProjectRoots([
    ...resolveStateProjectReferences(globalState, globalState["project-order"]),
    ...resolveStateProjectReferences(globalState, globalState["electron-saved-workspace-roots"]),
  ]);
  const savedOrder = savedRoots.map((project) => project.path);

  if (body.includeSaved ?? body.params?.includeSaved ?? true) {
    for (const project of savedRoots) {
      upsertProject(projects, project.path, "saved", stateProjectFields(project));
    }
  }

  const activeRoots = resolveStateProjectReferences(globalState, globalState["active-workspace-roots"]);
  for (const project of activeRoots) {
    upsertProject(projects, project.path, "active", {
      ...stateProjectFields(project),
      active: true,
    });
  }

  const pinnedRoots = resolveStateProjectReferences(globalState, globalState["pinned-project-ids"]);
  for (const project of pinnedRoots) {
    upsertProject(projects, project.path, "pinned", {
      ...stateProjectFields(project),
      pinned: true,
    });
  }

  if (body.includeTrusted ?? body.params?.includeTrusted ?? true) {
    for (const [path, metadata] of configuredProjects.entries()) {
      upsertProject(projects, path, "trusted", {
        trustLevel: metadata.trustLevel ?? null,
      });
    }
  }

  await readThreadProjects(body, client, projects);

  const searchTerm = (body.searchTerm ?? body.params?.searchTerm ?? "").trim().toLowerCase();
  const existsOnly = body.existsOnly ?? body.params?.existsOnly ?? false;
  let items = [...projects.values()].filter((project) => {
    if (existsOnly && !project.exists) {
      return false;
    }
    if (!searchTerm) {
      return true;
    }
    return project.title.toLowerCase().includes(searchTerm)
      || project.path.toLowerCase().includes(searchTerm);
  });

  items.sort((left, right) => {
    if (left.pinned !== right.pinned) {
      return left.pinned ? -1 : 1;
    }
    const leftIndex = projectSortIndex(left.path, savedOrder);
    const rightIndex = projectSortIndex(right.path, savedOrder);
    if (leftIndex !== rightIndex) {
      return leftIndex - rightIndex;
    }
    if (left.lastActiveAt !== right.lastActiveAt) {
      return compareTimestampsDesc(left.lastActiveAt, right.lastActiveAt);
    }
    return left.title.localeCompare(right.title);
  });

  const total = items.length;
  const cursor = Math.max(0, Number(body.cursor ?? body.params?.cursor ?? 0) || 0);
  const limit = Math.max(1, Math.min(Number(body.limit ?? body.params?.limit ?? DEFAULT_PROJECT_LIMIT) || DEFAULT_PROJECT_LIMIT, 500));
  items = items.slice(cursor, cursor + limit);
  const nextCursor = cursor + limit < total ? String(cursor + limit) : null;

  return {
    result: {
      projects: items,
      items,
      total,
      nextCursor,
      codexHome: home,
    },
  };
}

async function listThreads(body, client) {
  const params = withThreadSortDefaults(mergeParams(body, pickParams(body, [
    "cursor",
    "limit",
    "sortKey",
    "sortDirection",
    "modelProviders",
    "sourceKinds",
    "archived",
    "cwd",
    "useStateDbOnly",
    "searchTerm",
  ])));
  const result = await client.request("thread/list", params, body.timeoutMs);
  if (Array.isArray(result?.data)) {
    return {
      result: {
        ...result,
        data: enrichThreads(result.data),
      },
    };
  }
  return { result };
}

function emptyTurnsPage() {
  return {
    data: [],
    nextCursor: null,
    backwardsCursor: null,
  };
}

async function listThreadTurns(body, client) {
  const threadId = body.threadId ?? body.params?.threadId;
  if (!threadId) {
    return { result: emptyTurnsPage() };
  }

  const params = mergeParams(body, pickParams(body, [
    "threadId",
    "cursor",
    "limit",
    "sortDirection",
    "itemsView",
  ]));

  try {
    const result = await client.request("thread/turns/list", params, body.timeoutMs);
    return { result };
  } catch (error) {
    client.pushEvent("connector/threadTurnsListFallback", {
      threadId,
      error: error.message,
      code: error.code,
    });
    const result = await client.request("thread/read", {
      threadId,
      includeTurns: true,
    }, body.timeoutMs);
    const turns = Array.isArray(result?.thread?.turns) ? result.thread.turns : [];
    return {
      result: {
        data: turns,
        nextCursor: null,
        backwardsCursor: null,
        fallback: "thread/read",
      },
    };
  }
}

async function handleInvoke(pathname, body, client) {
  switch (pathname) {
    case "/invoke/status":
      return client.status();
    case "/invoke/listThreads":
    case "/invoke/listSessions":
      return listThreads(body, client);
    case "/invoke/listProjects":
      return listProjects(body, client);
    case "/invoke/searchThreads": {
      if (!body.searchTerm && !(body.params && body.params.searchTerm)) {
        throw new HttpError(400, "searchTerm is required");
      }
      const params = mergeParams(body, pickParams(body, [
        "cursor",
        "limit",
        "sortKey",
        "sortDirection",
        "sourceKinds",
        "archived",
        "searchTerm",
      ]));
      const result = await client.request("thread/search", params, body.timeoutMs);
      return { result };
    }
    case "/invoke/readThread": {
      const threadId = body.threadId ?? body.params?.threadId;
      if (!threadId) {
        throw new HttpError(400, "threadId is required");
      }
      const params = mergeParams(body, pickParams(body, ["threadId", "includeTurns"]));
      const result = await client.request("thread/read", params, body.timeoutMs);
      return { result };
    }
    case "/invoke/setThreadReadState": {
      const threadId = body.threadId ?? body.params?.threadId;
      const hasUnreadTurn = body.hasUnreadTurn ?? body.params?.hasUnreadTurn;
      if (!threadId) throw new HttpError(400, "threadId is required");
      if (typeof hasUnreadTurn !== "boolean") throw new HttpError(400, "hasUnreadTurn is required");
      let observedUpdatedAt = body.observedUpdatedAt ?? body.params?.observedUpdatedAt;
      if (observedUpdatedAt === undefined) {
        const detail = await client.request("thread/read", { threadId, includeTurns: false }, body.timeoutMs);
        observedUpdatedAt = detail?.thread?.updatedAt;
      }
      return { result: setThreadReadState(threadId, hasUnreadTurn, observedUpdatedAt) };
    }
    case "/invoke/listThreadTurns":
      return listThreadTurns(body, client);
    case "/invoke/listApps": {
      const params = mergeParams(body, pickParams(body, [
        "cursor",
        "limit",
        "threadId",
        "forceRefetch",
      ]));
      const result = await client.request("app/list", params, body.timeoutMs);
      return { result };
    }
    case "/invoke/startThread": {
      const params = mergeParams(body, {
        ...(body.model ? { model: body.model } : {}),
        ...(body.cwd ? { cwd: body.cwd } : {}),
      });
      const result = await client.request("thread/start", params, body.timeoutMs);
      return { result };
    }
    case "/invoke/resumeThread": {
      if (!body.threadId) {
        throw new HttpError(400, "threadId is required");
      }
      const params = mergeParams(body, {
        threadId: body.threadId,
        ...(body.excludeTurns !== undefined ? { excludeTurns: body.excludeTurns } : {}),
        ...(body.initialTurnsPage !== undefined ? { initialTurnsPage: body.initialTurnsPage } : {}),
      });
      const result = await client.request("thread/resume", params, body.timeoutMs);
      return { result };
    }
    case "/invoke/startTurn": {
      if (!body.threadId) {
        throw new HttpError(400, "threadId is required");
      }
      if (body.input === undefined) {
        throw new HttpError(400, "input is required");
      }
      const params = mergeParams(body, {
        threadId: body.threadId,
        input: normalizeInput(body.input),
        ...(body.model ? { model: body.model } : {}),
        ...(body.cwd ? { cwd: body.cwd } : {}),
      });
      const result = await client.request("turn/start", params, body.timeoutMs);
      return { result, recentEvents: client.recentEvents({ limit: 50 }) };
    }
    case "/invoke/steerTurn": {
      if (body.input === undefined) {
        throw new HttpError(400, "input is required");
      }
      const params = mergeParams(body, {
        ...(body.threadId ? { threadId: body.threadId } : {}),
        ...(body.turnId ? { turnId: body.turnId } : {}),
        input: normalizeInput(body.input),
      });
      const result = await client.request("turn/steer", params, body.timeoutMs);
      return { result, recentEvents: client.recentEvents({ limit: 50 }) };
    }
    case "/invoke/interruptTurn": {
      const params = mergeParams(body, {
        ...(body.threadId ? { threadId: body.threadId } : {}),
        ...(body.turnId ? { turnId: body.turnId } : {}),
      });
      const result = await client.request("turn/interrupt", params, body.timeoutMs);
      return { result, recentEvents: client.recentEvents({ limit: 50 }) };
    }
    case "/invoke/recentEvents":
      return client.recentEvents(body);
    case "/invoke/request": {
      if (!body.method) {
        throw new HttpError(400, "method is required");
      }
      const result = await client.request(body.method, body.params || {}, body.timeoutMs);
      return { result, recentEvents: client.recentEvents({ limit: 50 }) };
    }
    default:
      throw new HttpError(404, `unknown invoke path: ${pathname}`);
  }
}

async function startServer(options) {
  const resolved = serverOptions(options);
  if (options.daemon) {
    await daemonize(resolved);
    return;
  }

  const client = new CodexAppServerClient(resolved);
  let shuttingDown = false;
  const server = createServer(async (request, response) => {
    const url = new URL(request.url || "/", `http://${request.headers.host || `${resolved.host}:${resolved.port}`}`);
    try {
      if (request.method === "GET" && url.pathname === "/healthz") {
        writeJson(response, 200, { ok: true, status: client.status() });
        return;
      }
      if (
        request.method === "POST"
        && url.pathname === "/__shutdown"
        && process.env.CODEX_CONNECTOR_ENABLE_TEST_SHUTDOWN === "1"
      ) {
        writeJson(response, 200, { ok: true });
        setImmediate(shutdown);
        return;
      }
      if (request.method === "POST" && url.pathname.startsWith("/invoke/")) {
        const body = await readJsonRequest(request);
        const data = await handleInvoke(url.pathname, body, client);
        writeJson(response, 200, { ok: true, data });
        return;
      }
      throw new HttpError(404, "not found");
    } catch (error) {
      const statusCode = error.statusCode || 500;
      writeJson(response, statusCode, {
        ok: false,
        error: {
          message: error.message,
          code: error.code,
          data: error.data,
        },
      });
    }
  });

  await new Promise((resolvePromise, rejectPromise) => {
    server.once("error", rejectPromise);
    server.listen(resolved.port, resolved.host, resolvePromise);
  });
  console.log(JSON.stringify({
    ok: true,
    url: `http://${resolved.host}:${resolved.port}`,
    pid: process.pid,
  }));

  const shutdown = async () => {
    if (shuttingDown) {
      return;
    }
    shuttingDown = true;
    await client.shutdown();
    server.closeAllConnections?.();
    server.close(() => process.exit(0));
    setTimeout(() => process.exit(0), 1000).unref();
  };
  process.on("SIGINT", shutdown);
  process.on("SIGTERM", shutdown);
}

function printHelp() {
  console.log(`baijimu-connector-codex ${VERSION}

Usage:
  baijimu-connector-codex start [--host 127.0.0.1] [--port 18110] [--listen stdio://] [--daemon]
  baijimu-connector-codex status
  baijimu-connector-codex stop
  baijimu-connector-codex --version

Environment:
  CODEX_CONNECTOR_PORT=18110
  CODEX_CONNECTOR_CODEX_ARGS='["app-server","--listen","stdio://"]'
`);
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  if (options.version || options.command === "--version") {
    console.log(VERSION);
    return;
  }
  if (options.help || options.command === "help") {
    printHelp();
    return;
  }

  if (options.command === "start") {
    await startServer(options);
    return;
  }

  if (options.command === "status") {
    const path = pidPath();
    console.log(JSON.stringify({
      pidPath: path,
      pid: existsSync(path) ? readFileSync(path, "utf8").trim() : null,
      logPath: logPath(),
    }, null, 2));
    return;
  }

  if (options.command === "stop") {
    const path = pidPath();
    if (!existsSync(path)) {
      console.log(JSON.stringify({ ok: true, stopped: false, reason: "pid file not found" }));
      return;
    }
    const pid = Number(readFileSync(path, "utf8").trim());
    process.kill(pid, "SIGTERM");
    console.log(JSON.stringify({ ok: true, stopped: true, pid }));
    return;
  }

  throw new HttpError(2, `unknown command: ${options.command}`);
}

main().catch((error) => {
  console.error(error.message);
  process.exit(error.statusCode === 2 ? 2 : 1);
});
