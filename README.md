# Baijimu Codex Desktop

`com.baijimu.connector.codex-desktop` 是 Codex 桌面环境管理应用，负责：

- 安装和验证 ChatGPT/Codex 官方桌面应用；
- 为已授权的百积木工作区创建桌面专用 LLM credential 和 `CODEX_HOME`；
- 展示可用工作区，并由用户显式选择后启动桌面应用；
- 保留原有 ChatGPT 登录和所有工作区目录，切换失败时恢复原选择；
- 从旧版一体化 Connector 一次性导入桌面档案元数据。

本应用不安装 Codex CLI，不启动 `codex app-server`，也不声明 Relay 远程能力。CLI、session/thread/turn/event 接口由 `com.baijimu.connector.codex` 负责；OpenAI 兼容补全接口继续由 `com.baijimu.connector.codex-completion` 负责。

## 状态所有权

Bridge Agent 为本应用注入独立的 `BAIJIMU_CONNECTOR_DATA_DIR`。桌面档案、当前桌面工作区、安装状态和管理令牌只保存在该目录；应用不会读取 Connector 的工作区运行档案。

首次启动且自身尚无元数据时，应用可以从同一 Bridge Agent 数据根目录下的 `com.baijimu.connector.codex/codex-credentials.json` 导入旧桌面档案。导入后保存独立副本，后续不再共享活动状态。

## 本地运行

```bash
cargo run -- start
cargo run -- status
cargo run -- stop
```

默认监听 `127.0.0.1:18111`。Bridge Agent 0.3.0 及以上会注入平台管理的 Baijimu CLI 路径到 `CODEX_DESKTOP_BAIJIMU_BINARY`。

## 验证

```bash
cargo test
npm test
```

本仓库是独立客户端本地应用发布单元，使用自己的版本、标签、签名制品和 `local-app-market` 记录。发布由其他任务执行。
