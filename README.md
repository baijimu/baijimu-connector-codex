# 百积木 Codex 桌面管理器

`com.baijimu.connector.codex` 是线上 `codex` 应用的后继版本，产品名称为 Codex 桌面管理器，负责：

- 安装和验证 ChatGPT/Codex 官方桌面应用；
- 为已授权的百积木工作区创建桌面专用 LLM credential 和 `CODEX_HOME`；
- 展示可用工作区，并由用户显式选择后启动桌面应用；
- 保留原有 ChatGPT 登录和所有工作区目录，切换失败时恢复原选择；
- 在原 Connector 数据目录中原位升级并继续使用既有桌面档案元数据。

本应用不安装 Codex CLI，不启动 `codex app-server`，也不声明 Relay 远程能力。CLI、session/thread/turn/event 接口由独立的 `com.baijimu.connector.codex-connector`（Codex 外部连接器）负责；OpenAI 兼容补全接口继续由 `com.baijimu.connector.codex-completion`（Codex 补全服务）负责。

## 状态所有权

Bridge Agent 继续按 `com.baijimu.connector.codex` 注入原有 `BAIJIMU_CONNECTOR_DATA_DIR`。桌面档案、当前桌面工作区、安装状态和管理令牌在同一目录内原位升级；应用不会读取新的外部 Connector 工作区运行档案。

## 本地运行

```bash
cargo run -- start
cargo run -- status
cargo run -- stop
```

默认监听 `127.0.0.1:18110`，继承旧版本端口以保证升级连续性。Bridge Agent 0.3.0 及以上会注入平台管理的 Baijimu CLI 路径到 `CODEX_DESKTOP_BAIJIMU_BINARY`。

## 验证

```bash
cargo test
npm test
```

本仓库是 `codex` 客户端本地应用的唯一发布单元，继续使用 `momoplan/baijimu-connector-codex` 的 `main`、`v<version>` 标签、签名制品和既有 `local-app-market` 记录。
