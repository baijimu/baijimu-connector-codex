# 百积木 Codex 桌面管理器

`com.baijimu.connector.codex` 是线上 `codex` 应用的后继版本，产品名称为 Codex 桌面管理器，负责：

- 安装和验证 ChatGPT/Codex 官方桌面应用；
- 初始化已授权工作区的桌面专用 LLM credential，并接管默认 `~/.codex` 的认证与百积木路由配置；
- 展示可用工作区；已初始化工作区可直接启动或显式重新授权；
- 所有工作区共享默认 `.codex` 中的会话、历史和状态；启动时只原子替换 `auth.json` 中的工作区凭证；
- Windows 直接启动可信 AppX 包的 FullTrust 可执行入口，macOS 通过 LaunchServices 启动，不写用户环境变量、不广播环境变化；
- 从旧版隔离档案升级时只迁移工作区凭证，旧会话目录原样保留，不自动合并状态数据库。

安装结果和百积木路由验证结果相互独立：应用、工作区凭证和配置完成后即可打开 Codex，路由验证会在后台对暂态错误最多尝试三次。验证仍未通过时，安装状态保持成功，界面显示可重新验证的警告；刷新或“重新验证路由”只复用现有工作区凭证进行探测，不会重新安装或重新签发凭证。

本应用不安装 Codex CLI，不启动 `codex app-server`，也不声明 Relay 远程能力。CLI、session/thread/turn/event 接口由独立的 `com.baijimu.connector.codex-connector`（Codex 远程连接器）负责；OpenAI 兼容模型接口继续由 `com.baijimu.connector.codex-completion`（Codex 模型接口服务）负责。

## 状态所有权

Bridge Agent 继续按 `com.baijimu.connector.codex` 注入原有 `BAIJIMU_CONNECTOR_DATA_DIR`。各工作区凭证保存在该 Connector 私有目录；默认 `~/.codex` 由本应用管理 `auth.json` 和百积木路由配置，其余会话、历史、技能和状态文件不随工作区切换。

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

本仓库是 `codex` 客户端本地应用的唯一发布单元，继续使用 `baijimu/baijimu-connector-codex` 的 `main`、`v<version>` 标签、签名制品和既有 `local-app-market` 记录。
