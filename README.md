# Baijimu Codex Local App

Baijimu Codex is an independent Rust local application that initializes and configures Codex on the current computer, manages workspace-scoped Codex environments, and exposes local Codex capabilities through one loopback service. Initialization uses the workspace currently authorized in Baijimu Local; it does not ask for a device or unrelated Baijimu project.

The state-directory ownership, first-user initialization, Baijimu-launched process, external Codex behavior, and environment-selection contract is documented in [`docs/codex-home-ownership-and-launch.md`](docs/codex-home-ownership-and-launch.md).

It is installed and supervised by `bridge-agent`. Bridge Agent finishes Connector installation without waiting for Codex initialization. On first use, the workspace-management page automatically starts initialization for the currently authorized workspace and shows the official installer's step and download progress. A new user binds that first workspace to the normal `.codex` directory; if `.codex` already exists without matching Baijimu ownership, it is preserved and the workspace uses `.baijimu/codex/p/<profile-key>`. The default directory can belong to only one workspace, while later workspaces remain isolated. After configuration and capability checks succeed, the Connector attempts to launch and verify the official desktop when the new workspace profile was activated. A desktop auto-launch failure does not invalidate the completed installation: setup succeeds and tells the user to open ChatGPT (or Codex on versions using that name) from the system application list. Later profile switches remain explicit management-page actions, and switching back to the bound workspace restores `.codex`. The selected `CODEX_HOME` is injected into each launch only; Connector startup and internal profile switches never change the user environment. Desktop launch never creates or opens a synthetic project and does not use window visibility as a readiness signal. A current-version failed initialization stops without looping and exposes an explicit repair action. An interrupted attempt or a failure left by an older Connector is marked as requiring fresh verification and may be resumed once by the normal readiness flow instead of being replayed as a current error. Credential issuance, exact workspace validation, the official Codex installer, configuration, smoke tests, and process/state-directory verification run inside this application. Bridge Agent never receives the LLM key.

## Requirements

- Baijimu Local / `bridge-agent` 0.2.21 or newer with the `connector.setup.v1` host capability.
- A Baijimu workspace already authorized in the client.

The official market package ships a Rust/native `baijimu-connector-codex`
binary under `bin/<platform>-<arch>/`. The legacy Node.js implementation is
kept for reference and compatibility, but the platform-managed entrypoint is
the native binary.

The package includes `ui/`, a static interface loaded inside the local-app detail panel. Before initialization succeeds, it presents an installation-only view with the official installer progress or repair action and hides environment and workspace controls. After initialization succeeds, it switches to workspace management: it shows the active Codex identity and state directory, lists authorized Baijimu workspaces and the original Codex environment, and launches Codex with the selected profile. Session and turn operations remain available through the Connector API but are intentionally not duplicated in this interface.

## Install

From a checkout:

```bash
cargo build --release
bridge-agent connector install /path/to/baijimu-connector-codex --replace
bridge-agent connector start com.baijimu.connector.codex
```

Or install the tagged package from a Git remote first:

```bash
git clone https://github.com/momoplan/baijimu-connector-codex.git
bridge-agent connector install /path/to/baijimu-connector-codex --replace
```

The connector listens on `127.0.0.1:18110` by default. Market installation only installs and starts the Connector. The embedded workspace manager invokes an idempotent readiness operation: when the current workspace is authorized and setup is incomplete, it runs the installer compiled into the Connector, creates a workspace-scoped LLM credential, passes it through a private temporary file, and removes the file after setup. The installer receives the selected workspace directory through `CODEX_HOME`. Each setup attempt records its Connector version and attempt identity. Restart only rewrites persisted state when an actual recovery or schema migration is required; unchanged terminal states keep their original timestamps. A failure produced by the current Connector requires an explicit repair retry, so repeated process restarts cannot create an unbounded install loop.

On macOS, Rust exclusively owns the setup workflow, typed progress/result contracts, artifact-manifest parsing, downloads, credential configuration, and verification. The embedded shell is a stateless native-action adapter: it can only verify and install a supplied desktop or CLI artifact and never reads or writes setup state. The artifact manifest location comes from the versioned `installers/upstream-artifact-source.json` catalog embedded in the Connector rather than a platform-specific runtime branch.

Codex CLI discovery belongs entirely to this Connector; Bridge Agent neither selects nor injects a Codex executable. On every app-server start, the Connector searches standard official CLI install locations first, then the process `PATH`, and finally the user's login environment. Windows also supports the Connector-managed content-addressed official CLI installation. Desktop-app-internal Codex binaries are always rejected, even if inherited through `PATH`. The `status` response exposes the resolved path, source, version, `app-server` capability check, checked paths, and actionable errors. There is no persistent CLI-path override or related user setting.

On Windows, discovery only accepts launchers supported by the native process API (`.exe`, `.com`, `.bat`, or `.cmd`) and follows `PATHEXT`; it never executes the extensionless POSIX shim created beside an npm command. If no supported launcher exists, startup fails with `CODEX_BINARY_NOT_FOUND` instead of reaching Windows `ERROR_BAD_EXE_FORMAT` (`os error 193`).

Windows initialization verifies the isolated profile through the documented Codex app-server JSON-RPC sequence. It waits for the initialize response, API-key login response, `account/login/completed`, and `account/updated` before issuing the final `account/read`; it does not infer success from output text or a fixed sleep. It then persists and reads back the desktop locale (default `zh-CN`). Windows sandbox setup, retry, and fallback remain owned by the official ChatGPT/Codex app and are not part of Connector initialization. Protocol errors, early process exit, timeout, a locale read-back mismatch, and a non-`apiKey` final account are reported separately with credentials masked.

Bridge Agent assigns a private application data directory through `BAIJIMU_CONNECTOR_DATA_DIR`. After exclusively binding its loopback port but before reporting startup success or serving health checks, the application synchronously loads or atomically creates `management-token` and verifies it by reading the persisted value back. Credential metadata and the original user `CODEX_HOME` recovery value live in that private directory. Workspace state lives either in the uniquely bound default `.codex` or in a deterministic private `.baijimu/codex/p/<profile-key>` directory. Internal switching atomically updates only Connector metadata and restarts the Connector-managed app-server with an explicit child-process `CODEX_HOME`. Initial setup launches the desktop after committing and validating a newly activated workspace profile; an existing personal profile is preserved until the user explicitly selects another profile. Later desktop profile switches require a management-page action. Connector-private `BAIJIMU_*` and `CODEX_CONNECTOR_*` variables are removed from Codex child environments. Versions that previously wrote a managed path into the user environment expose an explicit, ownership-checked restore action; normal startup never writes or broadcasts user environment changes. No workspace directory is deleted. Existing workspace credentials are validated and reused; a replacement is issued only when the stored credential is no longer valid. Metadata before v6 is migrated on first use. Every `/management/v1/*` request requires the management token. These management routes are local-only and are never registered as relay capabilities.

## CLI

```bash
baijimu-connector-codex start
baijimu-connector-codex start --daemon
baijimu-connector-codex status
baijimu-connector-codex stop
baijimu-connector-codex credential-state
baijimu-connector-codex checkout-project --workspace-id 642 --project-id 7405
```

Configuration can be provided with flags or environment variables:

```bash
CODEX_CONNECTOR_PORT=18110
CODEX_CONNECTOR_PROJECTS_DIR=/absolute/path/to/Baijimu/Projects
CODEX_CONNECTOR_CODEX_ARGS='["app-server","--listen","stdio://"]'
```

`CODEX_CONNECTOR_BAIJIMU_BINARY` 由支持 `connector.managed-tool-dependencies.v1` 的 Bridge Agent 在每次启动时注入，值为已安装并验证的 Baijimu CLI launcher 绝对路径。Connector 不从 `PATH` 查找该命令，也不提供持久化覆盖项。

## Local App Capabilities

The `schemaVersion: "2.0"` manifest declares these methods directly on
`connectorId=com.baijimu.connector.codex`; installation does not create a runtime service or
businessId:

- `status`
- `listThreads`
- `searchThreads`
- `readThread`
- `setThreadReadState`
- `listApps`
- `startThread`
- `resumeThread`
- `startTurn`
- `steerTurn`
- `interruptTurn`
- `recentEvents`
- `request`

`request` is an advanced raw JSON-RPC forwarder and should be treated as high risk in remote authorization policies.

The Connector publishes the raw `codexNotification` stream for diagnostics and
four versioned domain events for stable automation contracts:

- `codexTurnCompleted` identifies the completed, interrupted, or failed turn by
  `threadId` and `turnId` without copying turn items or assistant output.
- `codexThreadClosed`, `codexThreadArchived`, and `codexThreadDeleted` represent
  distinct thread lifecycle transitions and must not be interpreted as turn completion.

Domain-event delivery uses an idempotent event ID and retries temporary local
delivery failures. After Bridge Agent accepts an event, its durable outbox owns
delivery to the platform.

## Local management API

The application exposes authenticated setup and status operations for Bridge Agent:

- `GET /management/v1/setup/state`
- `POST /management/v1/setup/retry`
- `GET /management/v1/credential-state`
- `POST /management/v1/codex/launch` with `{ "mode": "chatgpt" }` or `{ "mode": "baijimu", "workspaceId": 123 }`
- `POST /management/v1/codex/restore-external-home` for an explicit, ownership-checked legacy environment restoration
- `POST /management/v1/projects/checkout`

The local management token is not a Baijimu workspace token or an LLM credential. It only authenticates the loopback call between Bridge Agent and this application.

Thread list responses include the Codex `cwd`, source, git metadata, title, preview, and pagination cursors so callers can choose the right workspace before starting or resuming work.
They also normalize `threadRuntimeStatus`, `activeFlags`, `isInProgress`, `latestTurnStatus`, and `hasUnreadTurn`. Unread state is seeded from the Codex desktop host state and advanced by Connector-observed thread updates; callers clear or restore it explicitly with `setThreadReadState`.

The project checkout operation delegates to the managed `baijimu project checkout`
command. It creates or validates a stable local checkout under
`CODEX_CONNECTOR_PROJECTS_DIR`, uses the platform Git credential helper, and
returns the canonical directory and current `codex/<userId>/...` branch for a
new Codex session. Existing directories are reused only after their Baijimu
workspace/project metadata, origin URL, and Codex branch namespace all match.

## Development

```bash
cargo test
npm run test:rust
```

The integration tests use a fake app-server process and do not require Codex
credentials.

## Release

This repository is the source of truth for both Codex local-app delivery paths,
which intentionally have independent cadences:

- `release.yml` builds a tagged Connector commit, signs the native binaries,
  publishes immutable platform archives, and creates the local-app market
  version. Formal application releases use one tag only: `v<version>`.
- `sync-codex-upstream-artifacts.yml` runs on a schedule or by explicit manual
  dispatch. It downloads the complete customer installer contract (the official
  Codex CLI packages for every installer platform plus desktop App packages), verifies upstream integrity,
  publishes every object under its SHA256, verifies anonymous OSS reads, keeps
  the schema-3 `codex-artifacts/latest.json` projection for installed legacy
  Connectors, and publishes schema-4 `codex-artifacts/v4/latest.json` with the
  minimum OS version extracted from each native desktop package. Connector
  1.2.62 and later fail before download or profile mutation when the host OS is
  unsupported. Content-addressed objects are permanent; synchronization only
  replaces the two `latest.json` pointers after all current objects are public.

Windows installation consumes OpenAI's canonical
`codex-package-<target>.tar.gz` layout and preserves its declared entrypoint,
code-mode host, `rg`, command runner, and sandbox setup resources. The older
flat `.exe.zip` release assets remain in the public snapshot only while older
Connector versions are still in use; new installers never select them.

The synchronizer is a release-side operation. Bridge Agent and customer devices
never execute it. First-use installers only read the already published manifest,
download the platform asset named by that contract, and verify its SHA256.
