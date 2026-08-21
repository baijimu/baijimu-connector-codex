export const DEFAULT_MODEL = "gpt-5.6-sol";

export function connectorStartupRetryable(error) {
  const code = String(
    error?.code || error?.data?.code || error?.data?.error?.code || "",
  ).toLowerCase();
  const message = error instanceof Error
    ? error.message.toLowerCase()
    : String(error || "").toLowerCase();
  return code === "connector_initializing"
    || message.includes("connector_initializing")
    || message.includes("正在初始化 codex connector");
}

function positiveInteger(value) {
  const number = Number(value);
  return Number.isInteger(number) && number > 0 ? number : null;
}

export function normalizeCredentialState(value) {
  const input = value && typeof value === "object" ? value : {};
  const workspaces = Array.isArray(input.workspaces)
    ? input.workspaces
        .map((workspace) => ({
          workspaceId: positiveInteger(workspace?.workspaceId),
          name: String(workspace?.name || "").trim(),
          authorized: workspace?.authorized === true,
          configured: workspace?.configured === true,
          userIds: Array.isArray(workspace?.userIds) ? workspace.userIds.map(positiveInteger).filter(Boolean) : [],
        }))
        .filter((workspace) => workspace.workspaceId)
    : [];
  const profiles = Array.isArray(input.profiles)
    ? input.profiles.map(normalizeProfile).filter(Boolean)
    : [];
  return {
    activeMode: "baijimu",
    currentWorkspaceId: positiveInteger(input.currentWorkspaceId),
    activeWorkspaceId: positiveInteger(input.activeWorkspaceId),
    codexConfigured: input.codexConfigured === true,
    credentialStatus: String(input.credentialStatus || "not_configured"),
    activeProfile: normalizeProfile(input.activeProfile),
    profiles,
    workspaces,
    chatgpt: {
      available: input.chatgpt?.available !== false,
      configured: input.chatgpt?.configured === true,
      authMode: String(input.chatgpt?.authMode || ""),
      accountId: String(input.chatgpt?.accountId || ""),
      codexHome: String(input.chatgpt?.codexHome || ""),
    },
    originalCodexHome: String(input.originalCodexHome || input.chatgpt?.codexHome || ""),
    originalCodexHomeState: {
      captured: input.originalCodexHomeState?.captured === true,
      wasSet: typeof input.originalCodexHomeState?.value === "string",
      value: typeof input.originalCodexHomeState?.value === "string" ? input.originalCodexHomeState.value : "",
      captureSource: String(input.originalCodexHomeState?.captureSource || ""),
    },
    activeCodexHome: String(input.activeCodexHome || ""),
    externalCodexHome: typeof input.externalCodexHome === "string" ? input.externalCodexHome : "",
    legacyGlobalCodexHome: {
      restoreRequired: input.legacyGlobalCodexHome?.restoreRequired === true,
      canRestore: input.legacyGlobalCodexHome?.canRestore === true,
      currentValue: typeof input.legacyGlobalCodexHome?.currentValue === "string"
        ? input.legacyGlobalCodexHome.currentValue
        : "",
      restoreValue: typeof input.legacyGlobalCodexHome?.restoreValue === "string"
        ? input.legacyGlobalCodexHome.restoreValue
        : "",
      restoredAtEpochSeconds: Math.max(
        0,
        Number(input.legacyGlobalCodexHome?.restoredAtEpochSeconds) || 0,
      ),
    },
    discoveryWarning: typeof input.discoveryWarning === "string" ? input.discoveryWarning.trim() : "",
  };
}

export function normalizeProfile(value) {
  if (!value || typeof value !== "object") return null;
  const workspaceId = positiveInteger(value.workspaceId);
  if (!workspaceId) return null;
  return {
    profileId: String(value.profileId || ""),
    environment: String(value.environment || "prod"),
    userId: positiveInteger(value.userId),
    clientId: String(value.clientId || ""),
    workspaceId,
    workspaceName: String(value.workspaceName || `工作区 ${workspaceId}`).trim(),
    model: String(value.model || DEFAULT_MODEL).trim() || DEFAULT_MODEL,
    activatedAtEpochSeconds: Math.max(0, Number(value.activatedAtEpochSeconds) || 0),
    codexHome: String(value.codexHome || ""),
    credentialStatus: String(value.credentialStatus || ""),
  };
}

export function profileBadgeMeta({
  active = false,
  systemDefault = false,
  disabled = false,
  configured = true,
  credentialStatus = "",
} = {}) {
  const badges = [];
  if (active) badges.push({ label: "当前", tone: "current" });
  if (systemDefault) badges.push({ label: "共享 .codex", tone: "default" });
  if (disabled) badges.push({ label: "未授权", tone: "warning" });
  else if (!configured) badges.push({ label: "未初始化", tone: "warning" });
  else if (["missing", "invalid"].includes(credentialStatus)) {
    badges.push({ label: "需重新授权", tone: "danger" });
  }
  return badges;
}

export function credentialStatusMeta(status) {
  switch (status) {
    case "verified":
      return { label: "已验证", tone: "success" };
    case "invalid":
      return { label: "凭证无效", tone: "danger" };
    case "invalid_context":
      return { label: "归属异常", tone: "danger" };
    case "unverified":
      return { label: "暂未验证", tone: "warning" };
    case "not_configured":
      return { label: "尚未配置", tone: "neutral" };
    default:
      return { label: "状态未知", tone: "neutral" };
  }
}

export function setupStatusMeta(value) {
  const status = String(value?.status || "pending");
  const labels = {
    pending: "等待初始化",
    running: "正在初始化",
    succeeded: "已完成",
    failed: "初始化失败",
    interrupted: "初始化已中断",
    needs_retry: "需要重新验证",
  };
  const routeVerificationRunning = status === "succeeded"
    && value?.completedAtEpochSeconds == null;
  const routeVerificationFailed = status === "succeeded"
    && value?.retryable === true;
  return {
    status,
    label: routeVerificationRunning
      ? "路由验证中"
      : routeVerificationFailed
        ? "路由验证失败"
        : labels[status] || status,
    retryable: value?.retryable === true
      || ["failed", "interrupted", "needs_retry"].includes(status),
    showCurrentError: status === "failed" && Boolean(String(value?.error || "").trim()),
  };
}

export function codexCapabilityMeta(value) {
  const status = String(value?.status || "pending");
  if (status === "succeeded") {
    if (value?.completedAtEpochSeconds == null) {
      return {
        available: true,
        label: "可打开 · 验证中",
        tone: "warning",
        message: "Codex 已完成安装配置，可以打开使用；百积木路由正在后台验证。",
      };
    }
    if (value?.retryable === true) {
      return {
        available: true,
        label: "可打开 · 验证失败",
        tone: "warning",
        message: "Codex 已完成安装配置，可以打开使用；百积木路由验证暂未通过，可稍后重新验证。",
      };
    }
    return {
      available: true,
      label: "可用",
      tone: "success",
      message: "Codex 运行环境已就绪，可以查询会话、读取线程并发起对话。",
    };
  }
  if (status === "running") {
    return {
      available: false,
      label: "安装中",
      tone: "warning",
      message: "Codex 桌面环境正在安装，完成前暂时不能启动工作区环境。",
    };
  }
  if (["failed", "interrupted", "needs_retry"].includes(status)) {
    return {
      available: false,
      label: status === "failed" ? "安装失败" : "需要处理",
      tone: "danger",
      message: "Codex 桌面环境需要修复，完成前暂时不能启动工作区环境。",
    };
  }
  return {
    available: false,
    label: "准备中",
    tone: "neutral",
    message: "正在准备自动初始化 Codex 桌面环境。",
  };
}

export function setupActionMeta(value) {
  const meta = setupStatusMeta(value);
  if (meta.status === "succeeded" && value?.retryable === true) {
    return {
      visible: true,
      operation: "verify",
      label: "重新验证路由",
    };
  }
  if (meta.retryable) {
    const labels = {
      interrupted: "立即重试",
      needs_retry: "重新验证",
    };
    return {
      visible: true,
      operation: "retry",
      label: labels[meta.status] || "重新安装并修复",
    };
  }
  return {
    visible: false,
    operation: null,
    label: meta.status === "running" || meta.status === "pending"
      ? "正在处理…"
      : "重新安装并修复",
  };
}

export function normalizeSetupProgress(value) {
  const setupStatus = String(value?.status || "pending");
  const installer = value?.installerStatus && typeof value.installerStatus === "object"
    ? value.installerStatus
    : {};
  const sourceSteps = setupStatus === "needs_retry" ? [] : installer.steps;
  const steps = (Array.isArray(sourceSteps) ? sourceSteps : []).map((step, index) => ({
    index: Math.max(1, Number(step?.index) || index + 1),
    name: String(step?.name || `步骤 ${index + 1}`),
    state: String(step?.state || "pending"),
    detail: String(step?.detail || ""),
    downloadedBytes: step?.downloadedBytes != null && Number.isFinite(Number(step.downloadedBytes))
      ? Number(step.downloadedBytes)
      : null,
    totalBytes: step?.totalBytes != null && Number.isFinite(Number(step.totalBytes))
      ? Number(step.totalBytes)
      : null,
  }));
  const finishedStates = new Set(["completed", "skipped"]);
  const finished = steps.filter((step) => finishedStates.has(step.state)).length;
  const current = steps.find((step) => step.state === "running");
  const downloadFraction = current?.totalBytes > 0 && current?.downloadedBytes >= 0
    ? Math.min(1, current.downloadedBytes / current.totalBytes)
    : 0;
  const calculated = steps.length > 0
    ? Math.round(((finished + downloadFraction) / steps.length) * 100)
    : 0;
  const percent = setupStatus === "succeeded" ? 100 : Math.max(0, Math.min(99, calculated));
  return {
    status: setupStatus,
    locale: String(installer.locale || ""),
    percent,
    currentStep: Math.max(0, Number(installer.currentStep) || current?.index || 0),
    startedAt: String(installer.startedAt || ""),
    updatedAt: String(installer.updatedAt || ""),
    steps,
  };
}

export function shouldShowSetupProgress(value) {
  const progress = normalizeSetupProgress(value);
  return progress.status !== "succeeded" && progress.steps.length > 0;
}
