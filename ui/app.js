import {
  codexCapabilityMeta,
  connectorStartupRetryable,
  normalizeCredentialState,
  normalizeSetupProgress,
  profileBadgeMeta,
  setupActionMeta,
  setupStatusMeta,
  shouldShowSetupProgress,
} from "./state.mjs";

const elementIds = [
  "refresh-button", "integration-unavailable-panel", "capability-message",
  "go-to-runtime-button", "runtime-status-badge",
  "message", "error", "error-text",
  "error-retry-button", "warning",
  "legacy-home-migration", "legacy-home-message", "restore-external-home-button",
  "auth-profile-list",
  "setup-message", "setup-action-button", "setup-progress", "setup-progress-label",
  "setup-progress-percent", "setup-progress-track", "setup-progress-bar", "setup-step-list",
  "switch-progress", "switch-progress-message", "auth-switch-modal",
  "auth-switch-modal-title", "auth-switch-modal-message", "auth-switch-cancel",
  "auth-switch-confirm",
  "management-workspace-panel",
  "setup-panel", "setup-actions",
];
const elements = Object.fromEntries(elementIds.map((id) => [id, document.getElementById(id)]));

let credentialState = null;
let setupState = null;
let setupMonitorGeneration = 0;
let pendingCodexLaunch = null;
let accountBusy = false;
let errorRetryAction = null;
const STARTUP_RETRY_ATTEMPTS = 20;

function bridge() {
  const api = window.baijimuLocalApp;
  if (!api || api.version !== 1 || typeof api.invoke !== "function") {
    throw new Error("当前 Bridge Agent 不支持应用内嵌界面，请先升级 Bridge Agent。");
  }
  return api;
}

async function invokeManagement(operation, argumentsValue = undefined) {
  let lastError;
  for (let attempt = 0; attempt < STARTUP_RETRY_ATTEMPTS; attempt += 1) {
    try {
      return await bridge().invoke(operation, argumentsValue);
    } catch (error) {
      lastError = error;
      if (!connectorStartupRetryable(error) || attempt + 1 >= STARTUP_RETRY_ATTEMPTS) throw error;
      await new Promise((resolve) => window.setTimeout(resolve, Math.min(500, 100 + attempt * 25)));
    }
  }
  throw lastError || new Error("Codex 桌面管理器初始化超时。");
}

function setMessage(target, value) {
  elements[target].textContent = value;
  elements[target].hidden = !value;
}

function showError(value, { action = null, label = "重试" } = {}) {
  const message = value == null ? "" : String(value);
  errorRetryAction = typeof action === "function" ? action : null;
  elements["error-text"].textContent = message;
  elements.error.hidden = !message;
  elements["error-retry-button"].textContent = label;
  elements["error-retry-button"].hidden = !errorRetryAction;
  elements["error-retry-button"].disabled = accountBusy;
}

function clearNotices() {
  setMessage("message", "");
  showError("");
}

function renderContentVisibility() {
  const capability = codexCapabilityMeta(setupState);
  const hasAuthorizedWorkspace = credentialState?.workspaces?.some(
    (workspace) => workspace.authorized,
  );
  const hasProfile = (credentialState?.profiles?.length || 0) > 0;
  elements["management-workspace-panel"].hidden = !capability.available && !hasAuthorizedWorkspace && !hasProfile;
  elements["integration-unavailable-panel"].hidden = capability.available;
}

function renderIntegrationState() {
  const capability = codexCapabilityMeta(setupState);
  elements["capability-message"].textContent = capability.message;
  elements["runtime-status-badge"].textContent = capability.label;
  elements["runtime-status-badge"].className = `status-badge ${capability.tone}`;
  elements["refresh-button"].hidden = false;
  renderContentVisibility();
}

function errorMessage(error) {
  return error instanceof Error ? error.message : String(error || "操作失败");
}

function setAccountBusy(value) {
  accountBusy = value;
  elements["refresh-button"].disabled = value;
  elements["setup-action-button"].disabled = value;
  elements["error-retry-button"].disabled = value || !errorRetryAction;
  elements["restore-external-home-button"].disabled = value;
  const action = setupActionMeta(setupState);
  elements["setup-action-button"].textContent = value ? "正在处理…" : action.label;
  document.querySelectorAll(".profile-action").forEach((button) => {
    button.disabled = value || button.dataset.profileDisabled === "true";
  });
}

function profileRow({ title, badges, active, disabled, actions }) {
  const row = document.createElement("div");
  row.className = `profile-row${active ? " active" : ""}${disabled ? " unavailable" : ""}`;
  const heading = document.createElement("div");
  heading.className = "profile-heading";
  const strong = document.createElement("strong");
  strong.textContent = title;
  heading.append(strong);
  for (const badge of badges) {
    const label = document.createElement("span");
    label.className = `profile-label ${badge.tone}`;
    label.textContent = badge.label;
    heading.append(label);
  }
  const actionGroup = document.createElement("div");
  actionGroup.className = "profile-actions";
  for (const action of actions) {
    const button = document.createElement("button");
    button.type = "button";
    button.className = `button ${action.tone || "secondary"} compact profile-action`;
    button.textContent = action.label;
    button.setAttribute("aria-label", `${action.label}${title}`);
    button.dataset.profileDisabled = String(disabled || action.disabled === true);
    button.disabled = accountBusy || disabled || action.disabled === true;
    button.addEventListener("click", action.onClick);
    actionGroup.append(button);
  }
  row.append(heading, actionGroup);
  return row;
}

function authProfiles() {
  const state = credentialState;
  const profiles = (state?.profiles || []).map((profile) => {
    const active = state.activeProfile?.profileId === profile.profileId;
    const workspace = profile.kind === "baijimu"
      ? state.workspaces.find((item) => item.workspaceId === profile.workspaceId)
      : null;
    const needsReauthorization = !["configured", "verified", "external"].includes(profile.credentialStatus);
    const actions = [];
    if (profile.kind === "baijimu" && workspace?.authorized) {
      actions.push({
        label: "重新授权",
        tone: "secondary",
        onClick: () => reauthorizeWorkspace(profile.workspaceId),
      });
    }
    actions.push({
      label: active ? "重启" : "切换并启动",
      tone: active ? "secondary" : "primary",
      disabled: needsReauthorization,
      onClick: () => openAuthSwitchModal({ authProfileId: profile.profileId }),
    });
    return {
      key: profile.profileId,
      title: profile.kind === "baijimu"
        ? `${profile.name || profile.workspaceName}（工作区 ${profile.workspaceId}）`
        : profile.name,
      badges: profileBadgeMeta({
        active,
        kind: profile.kind,
        disabled: false,
        configured: !needsReauthorization,
        credentialStatus: profile.credentialStatus,
      }),
      active,
      disabled: false,
      actions,
    };
  });
  for (const workspace of state?.workspaces || []) {
    if (!workspace.authorized || workspace.configured) continue;
    profiles.push({
      key: `workspace-source-${workspace.workspaceId}`,
      title: `${workspace.name || `工作区 ${workspace.workspaceId}`}（工作区 ${workspace.workspaceId}）`,
      badges: profileBadgeMeta({ configured: false }),
      active: false,
      disabled: false,
      actions: [{
        label: "创建授权档案",
        tone: "primary",
        onClick: () => initializeWorkspace(workspace.workspaceId),
      }],
    });
  }
  return profiles.sort((left, right) => {
    if (left.active !== right.active) return left.active ? -1 : 1;
    if (left.disabled !== right.disabled) return left.disabled ? 1 : -1;
    return 0;
  });
}

function renderAuthProfiles() {
  const list = elements["auth-profile-list"];
  list.replaceChildren();
  for (const profile of authProfiles()) {
    list.append(profileRow(profile));
  }
  if (!list.children.length) {
    const empty = document.createElement("div");
    empty.className = "empty-state";
    empty.textContent = "没有可用的授权档案。完成个人登录或百积木工作区授权后即可创建。";
    list.append(empty);
  }
}

function renderCredentialState() {
  const state = credentialState;
  setMessage("warning", state?.discoveryWarning || "");
  const migration = state?.legacyGlobalCodexHome;
  elements["legacy-home-migration"].hidden = !migration?.restoreRequired;
  if (migration?.restoreRequired) {
    const restoreTarget = migration.restoreValue || "系统默认 .codex（取消用户级 CODEX_HOME）";
    elements["legacy-home-message"].textContent = migration.canRestore
      ? `检测到旧版 Connector 留下的用户级 CODEX_HOME。可恢复为：${restoreTarget}；当前已打开的外部 Codex/终端需要重启。`
      : "检测到用户级 CODEX_HOME 指向 Connector 私有目录，但无法证明原始值，已保持不变。";
    elements["restore-external-home-button"].hidden = !migration.canRestore;
  }
  renderAuthProfiles();
}

function codexLaunchCopy(request) {
  const profile = credentialState?.profiles?.find(
    (item) => item.profileId === request.authProfileId,
  );
  const name = profile?.kind === "baijimu"
    ? `${profile.name || profile.workspaceName}（工作区 ${profile.workspaceId}）`
    : profile?.name || "所选授权档案";
  return {
    title: `使用${name}启动 Codex`,
    message: `将关闭当前 Codex，保存当前档案可能已刷新的授权，再把“${name}”的授权与认证配置原子写入固定 .codex 后重新启动。会话、历史记录和其他状态不会切换或移动。`,
    progress: `正在切换到${name}的凭证并启动 Codex…`,
  };
}

function closeAuthSwitchModal() {
  pendingCodexLaunch = null;
  elements["auth-switch-modal"].hidden = true;
}

function openAuthSwitchModal(request) {
  if (pendingCodexLaunch || accountBusy) return;
  const copy = codexLaunchCopy(request);
  pendingCodexLaunch = request;
  elements["auth-switch-modal-title"].textContent = copy.title;
  elements["auth-switch-modal-message"].textContent = copy.message;
  elements["auth-switch-modal"].hidden = false;
  elements["auth-switch-confirm"].focus();
}

async function confirmAuthSwitch() {
  const request = pendingCodexLaunch;
  if (!request) return;
  const copy = codexLaunchCopy(request);
  closeAuthSwitchModal();
  await launchCodex(request, copy.progress);
}

async function launchCodex(request, progressMessage) {
  clearNotices();
  setAccountBusy(true);
  elements["switch-progress-message"].textContent = progressMessage;
  elements["switch-progress"].hidden = false;
  try {
    const response = await invokeManagement("launchCodex", request);
    credentialState = normalizeCredentialState(response);
    renderCredentialState();
    setMessage(
      "message",
      "已切换到所选授权档案并提交 Codex 启动请求。",
    );
  } catch (error) {
    const message = errorMessage(error);
    await loadState({ ensureReady: false, monitor: false });
    showError(message, {
      action: () => launchCodex(request, progressMessage),
      label: "重试启动",
    });
  } finally {
    elements["switch-progress"].hidden = true;
    setAccountBusy(false);
  }
}

async function initializeWorkspace(workspaceId) {
  clearNotices();
  setAccountBusy(true);
  try {
    setupState = await invokeManagement("initializeWorkspace", { workspaceId });
    renderSetupState();
    setMessage("message", `已开始为工作区 ${workspaceId} 创建授权档案；当前 Codex 登录、会话和非认证配置保持不变。`);
    void monitorSetup();
  } catch (error) {
    setAccountBusy(false);
    showError(errorMessage(error), {
      action: () => initializeWorkspace(workspaceId),
      label: "重试初始化",
    });
  }
}

async function reauthorizeWorkspace(workspaceId) {
  clearNotices();
  setAccountBusy(true);
  try {
    credentialState = normalizeCredentialState(
      await invokeManagement("reauthorizeWorkspace", { workspaceId }),
    );
    renderCredentialState();
    setMessage("message", `工作区 ${workspaceId} 已重新授权；如果该工作区当前生效，Codex 已按需重启。`);
  } catch (error) {
    showError(errorMessage(error), {
      action: () => reauthorizeWorkspace(workspaceId),
      label: "重试授权",
    });
  } finally {
    setAccountBusy(false);
  }
}

function renderSetupState() {
  const meta = setupStatusMeta(setupState);
  const action = setupActionMeta(setupState);
  const status = meta.status;
  elements["setup-message"].textContent = meta.showCurrentError
    ? setupState.error
    : setupState?.message || (status === "pending"
      ? "正在确认当前授权工作区并准备自动初始化 Codex。"
      : "等待初始化");
  renderSetupProgress();
  renderIntegrationState();
  setAccountBusy(status === "running");
  elements["setup-actions"].hidden = false;
  elements["setup-action-button"].hidden = !action.visible;
  elements["setup-action-button"].textContent = action.label;
}

function formatBytes(value) {
  const bytes = Math.max(0, Number(value) || 0);
  if (bytes >= 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  if (bytes >= 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${bytes} B`;
}

function setTextIfChanged(element, value) {
  if (element.textContent !== value) element.textContent = value;
}

function setupDownloadLabel(step) {
  if (!(step.totalBytes > 0) || step.downloadedBytes == null) return "";
  const downloadedBytes = Math.max(0, step.downloadedBytes);
  const totalBytes = Math.max(0, step.totalBytes);
  const percent = Math.min(100, Math.round((downloadedBytes / totalBytes) * 100));
  return ` · ${formatBytes(downloadedBytes)} / ${formatBytes(totalBytes)}（${percent}%）`;
}

function setupStepStateLabel(state) {
  return ({
    pending: "等待",
    running: "进行中",
    completed: "完成",
    skipped: "跳过",
    failed: "失败",
  })[state] || state;
}

function renderSetupProgress() {
  const progress = normalizeSetupProgress(setupState);
  const visible = shouldShowSetupProgress(setupState);
  elements["setup-progress"].hidden = !visible;
  if (!visible) return;

  const current = progress.steps.find((step) => step.state === "running")
    || [...progress.steps].reverse().find((step) => ["completed", "failed"].includes(step.state));
  setTextIfChanged(elements["setup-progress-label"], current
    ? `${current.index}/${progress.steps.length} ${current.name}`
    : "准备初始化");
  setTextIfChanged(elements["setup-progress-percent"], `总进度 ${progress.percent}%`);
  elements["setup-progress-track"].setAttribute("aria-valuenow", String(progress.percent));
  elements["setup-progress-bar"].style.width = `${progress.percent}%`;

  const list = elements["setup-step-list"];
  const existing = new Map(
    [...list.children].map((item) => [Number(item.dataset.stepIndex), item]),
  );
  const activeIndexes = new Set(progress.steps.map((step) => step.index));
  [...list.children].forEach((item) => {
    if (!activeIndexes.has(Number(item.dataset.stepIndex))) item.remove();
  });
  progress.steps.forEach((step, stepIndex) => {
    let item = existing.get(step.index);
    if (!item) {
      item = document.createElement("li");
      item.dataset.stepIndex = String(step.index);
      const marker = document.createElement("span");
      marker.className = "setup-step-marker";
      const copy = document.createElement("span");
      copy.className = "setup-step-copy";
      const title = document.createElement("strong");
      const detail = document.createElement("small");
      const state = document.createElement("em");
      copy.append(title, detail);
      item.append(marker, copy, state);
    }
    item.className = `setup-step ${step.state}`;
    const marker = item.querySelector(".setup-step-marker");
    const title = item.querySelector("strong");
    const detail = item.querySelector("small");
    const state = item.querySelector("em");
    setTextIfChanged(marker, ["completed", "skipped"].includes(step.state) ? "✓" : String(step.index));
    setTextIfChanged(title, step.name);
    setTextIfChanged(
      detail,
      `${step.detail || setupStepStateLabel(step.state)}${setupDownloadLabel(step)}`,
    );
    setTextIfChanged(state, setupStepStateLabel(step.state));
    const expectedAtIndex = list.children[stepIndex];
    if (expectedAtIndex !== item) list.insertBefore(item, expectedAtIndex || null);
  });
}

async function monitorSetup() {
  const generation = ++setupMonitorGeneration;
  while (
    generation === setupMonitorGeneration
    && (setupState?.status === "running"
      || (setupState?.status === "succeeded" && setupState?.completedAtEpochSeconds == null))
  ) {
    await new Promise((resolve) => window.setTimeout(resolve, 1000));
    if (generation !== setupMonitorGeneration) return;
    try {
      setupState = await invokeManagement("setupState");
      renderSetupState();
      if (setupState?.status === "succeeded" && setupState?.completedAtEpochSeconds != null) {
        await loadState({ ensureReady: false, monitor: false });
        return;
      }
      if (setupState?.status === "failed") {
        showError(setupState?.error || "Codex 初始化失败。", {
          action: retrySetup,
          label: "重新安装并修复",
        });
        return;
      }
    } catch (error) {
      setAccountBusy(false);
      showError(errorMessage(error), {
        action: () => loadState({ ensureReady: false }),
        label: "重新检查",
      });
      return;
    }
  }
}

async function ensureCodexReady() {
  const readiness = await invokeManagement("ensureCodexReady", {});
  setupState = readiness?.setup || setupState;
  renderSetupState();
  switch (readiness?.readiness) {
    case "ready":
      if (setupState?.status === "succeeded" && setupState?.completedAtEpochSeconds == null) {
        void monitorSetup();
      }
      return;
    case "initializing":
      elements["setup-panel"].scrollIntoView({ behavior: "smooth", block: "start" });
      setMessage("message", readiness?.message || "正在自动下载安装并配置本机 Codex。");
      void monitorSetup();
      return;
    case "failed":
      showError(readiness?.message || "Codex 初始化失败，请检查失败步骤后重新安装修复。", {
        action: retrySetup,
        label: "重新安装并修复",
      });
      return;
    case "needs_workspace":
      showError(readiness?.message || "请先完成当前百积木工作区授权。", {
        action: () => loadState({ ensureReady: true }),
        label: "重新检查",
      });
      return;
    default:
      throw new Error(readiness?.message || "无法确认本机 Codex 初始化状态。");
  }
}

async function loadState({ ensureReady = false, monitor = true } = {}) {
  clearNotices();
  setAccountBusy(true);
  try {
    const [credential, setup] = await Promise.all([
      invokeManagement("credentialState"),
      invokeManagement("setupState"),
    ]);
    credentialState = normalizeCredentialState(credential);
    setupState = setup;
    renderCredentialState();
    renderSetupState();
    if (ensureReady) await ensureCodexReady();
    else if (monitor && (
      setupState?.status === "running"
      || (setupState?.status === "succeeded" && setupState?.completedAtEpochSeconds == null)
    )) void monitorSetup();
  } catch (error) {
    elements["runtime-status-badge"].textContent = "检查失败";
    elements["runtime-status-badge"].className = "status-badge danger";
    showError(errorMessage(error), {
      action: () => loadState({ ensureReady, monitor }),
      label: "重新加载",
    });
  } finally {
    if (setupState?.status !== "running") setAccountBusy(false);
  }
}

async function retrySetup() {
  clearNotices();
  const workspaceId = credentialState?.currentWorkspaceId;
  if (!workspaceId) {
    showError("客户端当前授权中缺少工作区信息。", {
      action: () => loadState({ ensureReady: true }),
      label: "重新检查",
    });
    return;
  }
  setAccountBusy(true);
  try {
    setupState = await invokeManagement("setupRetry", { workspaceId });
    renderSetupState();
    setMessage("message", "已开始重新安装并修复本机 Codex。");
    void monitorSetup();
  } catch (error) {
    setAccountBusy(false);
    showError(errorMessage(error), {
      action: retrySetup,
      label: "重试修复",
    });
  }
}

async function retryRouterVerification() {
  clearNotices();
  const workspaceId = credentialState?.currentWorkspaceId;
  if (!workspaceId) {
    showError("客户端当前授权中缺少工作区信息。", {
      action: () => loadState({ ensureReady: true }),
      label: "重新检查",
    });
    return;
  }
  setAccountBusy(true);
  try {
    setupState = await invokeManagement("verifyRouter", { workspaceId });
    renderSetupState();
    setMessage("message", "已开始重新验证百积木路由；验证期间仍可打开 Codex。");
    void monitorSetup();
  } catch (error) {
    setAccountBusy(false);
    showError(errorMessage(error), {
      action: retryRouterVerification,
      label: "重新验证",
    });
  }
}

function refreshState() {
  if (setupState?.status === "succeeded" && setupState?.retryable === true) {
    return retryRouterVerification();
  }
  return loadState({ ensureReady: true });
}

async function restoreExternalCodexHome() {
  clearNotices();
  setAccountBusy(true);
  try {
    credentialState = normalizeCredentialState(
      await invokeManagement("restoreExternalCodexHome", {}),
    );
    renderCredentialState();
    setMessage("message", "旧版 Connector 留下的用户级 CODEX_HOME 已恢复；请重启已打开的外部 Codex 和终端。");
  } catch (error) {
    showError(errorMessage(error), {
      action: restoreExternalCodexHome,
      label: "重试恢复",
    });
  } finally {
    setAccountBusy(false);
  }
}

elements["refresh-button"].addEventListener("click", () => void refreshState());
elements["setup-action-button"].addEventListener("click", () => {
  const action = setupActionMeta(setupState);
  if (action.operation === "verify") void retryRouterVerification();
  else void retrySetup();
});
elements["go-to-runtime-button"].addEventListener("click", () => {
  elements["setup-panel"].scrollIntoView({ behavior: "smooth", block: "start" });
  elements["setup-panel"].focus({ preventScroll: true });
});
elements["restore-external-home-button"].addEventListener("click", () => void restoreExternalCodexHome());
elements["error-retry-button"].addEventListener("click", () => {
  const action = errorRetryAction;
  if (action && !accountBusy) void action();
});
elements["auth-switch-cancel"].addEventListener("click", closeAuthSwitchModal);
elements["auth-switch-confirm"].addEventListener("click", () => void confirmAuthSwitch());
elements["auth-switch-modal"].addEventListener("click", (event) => {
  if (event.target === elements["auth-switch-modal"]) closeAuthSwitchModal();
});
document.addEventListener("keydown", (event) => {
  if (event.key === "Escape" && pendingCodexLaunch) closeAuthSwitchModal();
});

void loadState({ ensureReady: true });
