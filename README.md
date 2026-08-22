# 百积木 Codex 桌面管理器

`com.baijimu.connector.codex` 是线上 `codex` 应用的后继版本，产品名称为 Codex 桌面管理器，负责：

- 安装和验证 ChatGPT/Codex 官方桌面应用；
- 初始化已授权工作区的桌面专用 LLM credential，并保存为所有 Codex 工作区可选的认证通道；
- 在每个 Codex 工作区中显式选择 ChatGPT 登录或任一已初始化的百积木工作区授权；
- 用户可手动新增多个 Codex 工作区；每个工作区使用独立 `CODEX_HOME` 承载会话、历史、技能和其他状态；
- ChatGPT 与百积木授权作为全局认证通道目录供每个 Codex 工作区独立选择，切换时只原子更新目标工作区的 `auth.json` 与认证相关 `config.toml` 项；
- Windows 直接启动可信 AppX 包的 FullTrust 可执行入口，macOS 通过 LaunchServices 启动，不写用户环境变量、不广播环境变化；
- 从旧版隔离档案升级时只迁移工作区凭证，旧会话目录原样保留，不自动合并状态数据库。

安装结果和百积木路由验证结果相互独立：安装未完成时界面只显示安装状态；应用、工作区凭证和配置完成后进入 Codex 默认工作区，不再显示安装状态。路由验证会在后台对暂态错误最多尝试三次；验证仍未通过时，默认工作区内显示可重新验证的警告，不会退回安装视图。刷新或“重新验证路由”只复用现有工作区凭证进行探测，不会重新安装或重新签发凭证。

本应用不安装 Codex CLI，不启动 `codex app-server`，也不声明 Relay 远程能力。CLI、session/thread/turn/event 接口由独立的 `com.baijimu.connector.codex-connector`（Codex 远程连接器）负责；OpenAI 兼容模型接口继续由 `com.baijimu.connector.codex-completion`（Codex 模型接口服务）负责。

## 状态所有权

Bridge Agent 继续按 `com.baijimu.connector.codex` 注入原有 `BAIJIMU_CONNECTOR_DATA_DIR`。认证通道凭证、ChatGPT 授权快照和新增工作区目录保存在该 Connector 私有目录；现有 `~/.codex` 原样登记为默认工作区。客户端启动和状态刷新只识别现场，不改写任何工作区的 `auth.json` 或 `config.toml`。只有用户创建工作区或明确切换认证通道时，本应用才管理目标工作区这两个文件中的认证内容；其余文件始终不动。

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
