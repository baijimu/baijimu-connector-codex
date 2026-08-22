import {
  codexWorkspaceMeta,
  codexCapabilityMeta,
  connectorStartupRetryable,
  defaultWorkspaceChildren,
  normalizeCredentialState,
  normalizeSetupProgress,
  primaryViewMeta,
  profileBadgeMeta,
  setupActionMeta,
  setupStatusMeta,
  shouldShowSetupProgress,
} from "./state.mjs";

const elementIds = [
  "refresh-button", "runtime-status-badge",
  "message", "error", "error-text",
  "error-retry-button", "warning",
  "legacy-home-migration", "legacy-home-message", "restore-external-home-button",
  "auth-profile-list", "add-workspace-button", "codex-workspace-list",
  "workspace-route-notice", "workspace-route-message",
  "workspace-route-action",
  "setup-message", "setup-action-button", "setup-progress", "setup-progress-label",
  "setup-progress-percent", "setup-progress-track", "setup-progress-bar", "setup-step-list",
  "codex-operation-progress", "codex-operation-title", "codex-operation-message",
  "auth-switch-modal",
  "auth-switch-modal-title", "auth-switch-modal-message", "auth-switch-cancel",
  "auth-switch-confirm",
  "workspace-create-modal", "workspace-name-input", "workspace-auth-profile-select",
  "workspace-create-cancel", "workspace-create-confirm",
  "management-workspace-panel",
  "setup-panel", "setup-actions",
];
const elements = Object.fromEntries(elementIds.map((id) => [id, document.getElementById(id)]));

let credentialState = null;
let setupState = null;
let setupMonitorGeneration = 0;
let selectedAuthProfileId = null;
let selectedCodexWorkspaceId = null;
let authModalReturnFocus = null;
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

function renderInstallationState() {
  const capability = codexCapabilityMeta(setupState);
  elements["runtime-status-badge"].textContent = capability.label;
  elements["runtime-status-badge"].className = `status-badge ${capability.tone}`;
  elements["refresh-button"].hidden = false;
}

function errorMessage(error) {
  return error instanceof Error ? error.message : String(error || "操作失败");
}

function setAccountBusy(value) {
  accountBusy = value;
  elements["refresh-button"].disabled = value;
  elements["setup-action-button"].disabled = value;
  elements["add-workspace-button"].disabled = value;
  elements["workspace-create-confirm"].disabled = value || !workspaceCreateCanSubmit();
  elements["workspace-route-action"].disabled = value;
  elements["auth-switch-confirm"].disabled = value || !authSwitchCanSubmit();
  elements["error-retry-button"].disabled = value || !errorRetryAction;
  elements["restore-external-home-button"].disabled = value;
  const action = setupActionMeta(setupState);
  elements["setup-action-button"].textContent = value ? "正在处理…" : action.label;
  document.querySelectorAll(".profile-action, input[name='auth-profile']").forEach((control) => {
    control.disabled = value || control.dataset.profileDisabled === "true";
  });
}

function profileRow({ key, title, badges, active, disabled, selectable, actions }) {
  const row = document.createElement("div");
  row.className = `profile-row${active ? " active" : ""}${disabled ? " unavailable" : ""}`;
  const choice = document.createElement(selectable ? "label" : "div");
  choice.className = "profile-choice";
  if (selectable) {
    const radio = document.createElement("input");
    radio.type = "radio";
    radio.name = "auth-profile";
    radio.value = key;
    radio.checked = selectedAuthProfileId === key;
    radio.dataset.profileDisabled = String(disabled);
    radio.disabled = accountBusy || disabled;
    radio.addEventListener("change", () => {
      selectedAuthProfileId = key;
      elements["auth-switch-confirm"].disabled = accountBusy || !authSwitchCanSubmit();
    });
    choice.append(radio);
  }
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
    button.dataset.profileDisabled = String(action.disabled === true);
    button.disabled = accountBusy || action.disabled === true;
    button.addEventListener("click", action.onClick);
    actionGroup.append(button);
  }
  choice.append(heading);
  row.append(choice, actionGroup);
  return row;
}

function authProfiles() {
  const state = credentialState;
  const targetWorkspace = state?.codexWorkspaces?.find(
    (workspace) => workspace.workspaceId === selectedCodexWorkspaceId,
  );
  const profiles = (state?.profiles || []).map((profile) => {
    const active = targetWorkspace?.authProfileId === profile.profileId;
    const workspace = profile.kind === "baijimu"
      ? state.workspaces.find((item) => item.workspaceId === profile.workspaceId)
      : null;
    const needsReauthorization = profile.kind === "baijimu"
      && !["configured", "verified"].includes(profile.credentialStatus);
    const actions = [];
    if (profile.kind === "baijimu" && workspace?.authorized) {
      actions.push({
        label: "重新授权",
        tone: "secondary",
        onClick: () => {
          closeAuthSwitchModal();
          void reauthorizeWorkspace(profile.workspaceId);
        },
      });
    }
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
      disabled: needsReauthorization,
      selectable: true,
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
      selectable: false,
      actions: [{
        label: "创建认证通道",
        tone: "primary",
        onClick: () => {
          closeAuthSwitchModal();
          void initializeWorkspace(workspace.workspaceId);
        },
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
    empty.textContent = "没有可用的认证通道。可使用 ChatGPT 登录，或先完成百积木工作区授权。";
    list.append(empty);
  }
  elements["auth-switch-confirm"].disabled = accountBusy || !authSwitchCanSubmit();
}

function authSwitchCanSubmit() {
  if (!selectedAuthProfileId) return false;
  const activeProfileId = credentialState?.codexWorkspaces?.find(
    (workspace) => workspace.workspaceId === selectedCodexWorkspaceId,
  )?.authProfileId || null;
  if (selectedAuthProfileId === activeProfileId) return false;
  const profile = credentialState?.profiles?.find((item) => item.profileId === selectedAuthProfileId);
  return Boolean(profile) && !["missing", "invalid"].includes(profile.credentialStatus);
}

function renderCodexWorkspaces() {
  const list = elements["codex-workspace-list"];
  list.replaceChildren();
  for (const workspace of credentialState?.codexWorkspaces || []) {
    const meta = codexWorkspaceMeta(workspace, credentialState?.profiles || []);
    const card = document.createElement("article");
    card.className = `codex-workspace-card${workspace.active ? " active" : ""}`;

    const summary = document.createElement("div");
    summary.className = "codex-workspace-summary";
    const copy = document.createElement("div");
    copy.className = "codex-workspace-copy";
    const heading = document.createElement("div");
    heading.className = "profile-heading";
    const title = document.createElement("strong");
    title.textContent = workspace.name;
    heading.append(title);
    if (workspace.active) {
      const badge = document.createElement("span");
      badge.className = "profile-label current";
      badge.textContent = "当前工作区";
      heading.append(badge);
    }
    if (workspace.isDefault) {
      const badge = document.createElement("span");
      badge.className = "profile-label neutral";
      badge.textContent = "默认";
      heading.append(badge);
    }
    const channel = document.createElement("span");
    channel.className = "workspace-channel-copy";
    channel.textContent = `${meta.channelName} · ${meta.channelDetail}`;
    const home = document.createElement("code");
    home.textContent = workspace.codexHome;
    copy.append(heading, channel, home);

    const actions = document.createElement("div");
    actions.className = "workspace-card-actions";
    const authButton = document.createElement("button");
    authButton.type = "button";
    authButton.className = "button secondary compact profile-action";
    authButton.textContent = "切换认证通道";
    authButton.dataset.profileDisabled = "false";
    authButton.disabled = accountBusy;
    authButton.addEventListener("click", () => openAuthSwitchModal(workspace.workspaceId));
    const launchButton = document.createElement("button");
    launchButton.type = "button";
    launchButton.className = "button primary compact profile-action";
    launchButton.textContent = workspace.active ? "重启 Codex" : "打开工作区";
    launchButton.dataset.profileDisabled = String(!meta.channelAvailable);
    launchButton.disabled = accountBusy || !meta.channelAvailable;
    launchButton.addEventListener("click", () => {
      if (workspace.active) void restartCodex();
      else void activateCodexWorkspace(workspace.workspaceId);
    });
    actions.append(authButton, launchButton);
    summary.append(copy, actions);
    card.append(summary);
    if (workspace.isDefault) {
      const children = defaultWorkspaceChildren(credentialState);
      const nested = document.createElement("section");
      nested.className = "default-workspace-children";
      nested.setAttribute("aria-label", "默认工作区下的原有百积木工作区");
      const nestedHeading = document.createElement("div");
      nestedHeading.className = "default-workspace-children-heading";
      const nestedTitle = document.createElement("strong");
      nestedTitle.textContent = "原有百积木工作区";
      const nestedNote = document.createElement("span");
      nestedNote.textContent = children.length
        ? `共 ${children.length} 个，保留在默认工作区下，并可供全部 Codex 工作区使用`
        : "已创建的百积木工作区会保留在这里";
      nestedHeading.append(nestedTitle, nestedNote);
      nested.append(nestedHeading);
      const childList = document.createElement("div");
      childList.className = "default-workspace-child-list";
      for (const child of children) {
        const badges = profileBadgeMeta({
          active: child.active,
          configured: child.configured,
          credentialStatus: child.credentialStatus,
        });
        if (!child.authorized) badges.push({ label: "历史记录", tone: "neutral" });
        if (child.profileCount > 1) {
          badges.push({ label: `${child.profileCount} 个认证通道`, tone: "neutral" });
        }
        const childActions = [];
        if (child.canSelect && !child.active) {
          childActions.push({
            label: "用于默认工作区",
            tone: "secondary",
            onClick: () => void switchAuthChannel({
              authProfileId: child.profile.profileId,
              codexWorkspaceId: workspace.workspaceId,
            }),
          });
        }
        if (child.canInitialize) {
          childActions.push({
            label: "创建认证通道",
            tone: "primary",
            onClick: () => void initializeWorkspace(child.workspaceId),
          });
        } else if (child.canReauthorize) {
          childActions.push({
            label: "重新授权",
            tone: "secondary",
            onClick: () => void reauthorizeWorkspace(child.workspaceId),
          });
        }
        childList.append(profileRow({
          key: child.key,
          title: `${child.name}（工作区 ${child.workspaceId}）`,
          badges,
          active: child.active,
          disabled: !child.canSelect && !child.canInitialize,
          selectable: false,
          actions: childActions,
        }));
      }
      if (!children.length) {
        const empty = document.createElement("div");
        empty.className = "empty-state compact";
        empty.textContent = "暂无原有百积木工作区；完成工作区授权后会显示在这里。";
        childList.append(empty);
      }
      nested.append(childList);
      card.append(nested);
    }
    list.append(card);
  }
  if (!list.children.length) {
    const empty = document.createElement("div");
    empty.className = "empty-state";
    empty.textContent = "还没有 Codex 工作区。";
    list.append(empty);
  }
}

function availableAuthProfiles() {
  return (credentialState?.profiles || []).filter(
    (profile) => !["missing", "invalid"].includes(profile.credentialStatus),
  );
}

function workspaceCreateCanSubmit() {
  return Boolean(elements?.["workspace-name-input"]?.value.trim())
    && Boolean(elements?.["workspace-auth-profile-select"]?.value);
}

function openWorkspaceCreateModal() {
  if (accountBusy) return;
  const select = elements["workspace-auth-profile-select"];
  select.replaceChildren();
  for (const profile of availableAuthProfiles()) {
    const option = document.createElement("option");
    option.value = profile.profileId;
    option.textContent = profile.kind === "personal"
      ? "ChatGPT 登录"
      : `${profile.name || profile.workspaceName}（工作区 ${profile.workspaceId}）`;
    select.append(option);
  }
  if (!select.children.length) {
    const option = document.createElement("option");
    option.value = "";
    option.textContent = "暂无可用认证通道，请先创建或重新授权";
    option.disabled = true;
    option.selected = true;
    select.append(option);
  }
  elements["workspace-name-input"].value = "";
  elements["workspace-create-confirm"].disabled = !workspaceCreateCanSubmit();
  elements["workspace-create-modal"].hidden = false;
  elements["workspace-name-input"].focus();
}

function closeWorkspaceCreateModal() {
  elements["workspace-create-modal"].hidden = true;
}

async function createCodexWorkspace() {
  if (!workspaceCreateCanSubmit()) return;
  const request = {
    name: elements["workspace-name-input"].value.trim(),
    authProfileId: elements["workspace-auth-profile-select"].value,
  };
  closeWorkspaceCreateModal();
  clearNotices();
  setAccountBusy(true);
  try {
    credentialState = normalizeCredentialState(
      await invokeManagement("createCodexWorkspace", request),
    );
    renderCredentialState();
    setMessage("message", `Codex 工作区“${request.name}”已创建，并已接入所选认证通道。`);
  } catch (error) {
    showError(errorMessage(error), {
      action: openWorkspaceCreateModal,
      label: "重新创建",
    });
  } finally {
    setAccountBusy(false);
  }
}

async function activateCodexWorkspace(codexWorkspaceId) {
  clearNotices();
  setAccountBusy(true);
  elements["codex-operation-title"].textContent = "正在打开 Codex 工作区";
  elements["codex-operation-message"].textContent = "正在关闭现有 Codex，并使用目标工作区的独立状态目录启动。";
  elements["codex-operation-progress"].hidden = false;
  try {
    credentialState = normalizeCredentialState(await invokeManagement(
      "activateCodexWorkspace",
      { codexWorkspaceId },
    ));
    renderCredentialState();
    setMessage("message", "Codex 已使用所选工作区启动。");
  } catch (error) {
    showError(errorMessage(error), {
      action: () => activateCodexWorkspace(codexWorkspaceId),
      label: "重试打开",
    });
  } finally {
    elements["codex-operation-progress"].hidden = true;
    setAccountBusy(false);
  }
}

function renderCredentialState() {
  const state = credentialState;
  renderCodexWorkspaces();
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

function closeAuthSwitchModal() {
  selectedAuthProfileId = null;
  selectedCodexWorkspaceId = null;
  elements["auth-switch-modal"].hidden = true;
  if (authModalReturnFocus instanceof HTMLElement) authModalReturnFocus.focus();
  authModalReturnFocus = null;
}

function openAuthSwitchModal(workspaceId) {
  if (!elements["auth-switch-modal"].hidden || accountBusy) return;
  authModalReturnFocus = document.activeElement;
  selectedCodexWorkspaceId = workspaceId;
  const workspace = credentialState?.codexWorkspaces?.find(
    (item) => item.workspaceId === workspaceId,
  );
  selectedAuthProfileId = workspace?.authProfileId || null;
  elements["auth-switch-modal-title"].textContent = `切换“${workspace?.name || "Codex 工作区"}”的认证通道`;
  elements["auth-switch-modal-message"].textContent = "该工作区可使用当前全部认证通道；确认后不会影响其他工作区的会话、历史、技能或认证选择。";
  renderAuthProfiles();
  elements["auth-switch-modal"].hidden = false;
  const selected = elements["auth-profile-list"].querySelector("input[name='auth-profile']:checked");
  (selected || elements["auth-switch-cancel"]).focus();
}

async function confirmAuthSwitch() {
  if (!authSwitchCanSubmit()) return;
  const authProfileId = selectedAuthProfileId;
  const codexWorkspaceId = selectedCodexWorkspaceId;
  closeAuthSwitchModal();
  await switchAuthChannel({ authProfileId, codexWorkspaceId });
}

async function switchAuthChannel(request) {
  clearNotices();
  setAccountBusy(true);
  elements["codex-operation-title"].textContent = "正在切换认证通道";
  elements["codex-operation-message"].textContent = "正在保存当前认证并原子切换目标工作区的认证通道。";
  elements["codex-operation-progress"].hidden = false;
  try {
    const response = await invokeManagement("switchAuthChannel", request);
    credentialState = normalizeCredentialState(response);
    renderCredentialState();
    setMessage("message", "目标工作区的认证通道已切换；其他工作区保持不变。");
  } catch (error) {
    const message = errorMessage(error);
    await loadState({ ensureReady: false, monitor: false });
    showError(message, {
      action: () => switchAuthChannel(request),
      label: "重试切换",
    });
  } finally {
    elements["codex-operation-progress"].hidden = true;
    setAccountBusy(false);
  }
}

async function restartCodex() {
  clearNotices();
  setAccountBusy(true);
  elements["codex-operation-title"].textContent = "正在重启 Codex";
  elements["codex-operation-message"].textContent = "正在关闭当前工作区的现有进程并重新打开 Codex。";
  elements["codex-operation-progress"].hidden = false;
  try {
    await invokeManagement("restartCodex", {});
    setMessage("message", "已提交 Codex 重启请求；当前认证通道保持不变。");
  } catch (error) {
    showError(errorMessage(error), {
      action: restartCodex,
      label: "重试重启",
    });
  } finally {
    elements["codex-operation-progress"].hidden = true;
    setAccountBusy(false);
  }
}

async function initializeWorkspace(workspaceId) {
  clearNotices();
  setAccountBusy(true);
  try {
    setupState = await invokeManagement("initializeWorkspace", { workspaceId });
    renderSetupState();
    setMessage("message", `已开始为工作区 ${workspaceId} 创建认证通道；当前 Codex 登录、会话和非认证配置保持不变。`);
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
    setMessage("message", `工作区 ${workspaceId} 已重新授权；如果该通道当前生效，Codex 已关闭，请从默认工作区重启。`);
  } catch (error) {
    showError(errorMessage(error), {
      action: () => reauthorizeWorkspace(workspaceId),
      label: "重试授权",
    });
  } finally {
    setAccountBusy(false);
  }
}

function renderPrimaryView() {
  const view = primaryViewMeta(setupState);
  elements["management-workspace-panel"].hidden = !view.workspaceVisible;
  elements["setup-panel"].hidden = !view.setupVisible;

  const capability = codexCapabilityMeta(setupState);
  const routeStatusVisible = view.workspaceVisible
    && (setupState?.completedAtEpochSeconds == null || setupState?.retryable === true);
  elements["workspace-route-notice"].hidden = !routeStatusVisible;
  elements["workspace-route-message"].textContent = routeStatusVisible ? capability.message : "";
  elements["workspace-route-action"].hidden = !(routeStatusVisible && setupState?.retryable === true);
}

function renderSetupState() {
  const meta = setupStatusMeta(setupState);
  const action = setupActionMeta(setupState);
  const status = meta.status;
  elements["setup-message"].textContent = meta.showCurrentError
    ? setupState.error
    : setupState?.message || codexCapabilityMeta(setupState).message;
  renderSetupProgress();
  renderInstallationState();
  renderPrimaryView();
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
elements["add-workspace-button"].addEventListener("click", openWorkspaceCreateModal);
elements["workspace-create-cancel"].addEventListener("click", closeWorkspaceCreateModal);
elements["workspace-create-confirm"].addEventListener("click", () => void createCodexWorkspace());
elements["workspace-name-input"].addEventListener("input", () => {
  elements["workspace-create-confirm"].disabled = accountBusy || !workspaceCreateCanSubmit();
});
elements["workspace-auth-profile-select"].addEventListener("change", () => {
  elements["workspace-create-confirm"].disabled = accountBusy || !workspaceCreateCanSubmit();
});
elements["workspace-create-modal"].addEventListener("click", (event) => {
  if (event.target === elements["workspace-create-modal"]) closeWorkspaceCreateModal();
});
elements["workspace-route-action"].addEventListener("click", () => void retryRouterVerification());
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
  if (!elements["workspace-create-modal"].hidden && event.key === "Escape") {
    closeWorkspaceCreateModal();
    return;
  }
  if (elements["auth-switch-modal"].hidden) return;
  if (event.key === "Escape") {
    closeAuthSwitchModal();
    return;
  }
  if (event.key !== "Tab") return;
  const focusable = [...elements["auth-switch-modal"].querySelectorAll(
    "button:not(:disabled), input:not(:disabled)",
  )];
  if (!focusable.length) return;
  const first = focusable[0];
  const last = focusable.at(-1);
  if (event.shiftKey && document.activeElement === first) {
    event.preventDefault();
    last.focus();
  } else if (!event.shiftKey && document.activeElement === last) {
    event.preventDefault();
    first.focus();
  }
});

void loadState({ ensureReady: true });
