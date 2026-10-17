# 百积木 Codex 桌面管理器

权威市场 appId `codex` 对应 Codex 桌面管理器，负责：

- 安装和验证 ChatGPT/Codex 官方桌面应用；
- 初始化已授权工作区的桌面专用 LLM credential，并保存为所有 Codex 工作区可选的认证通道；
- 在每个 Codex 工作区中显式选择 ChatGPT 登录或任一已初始化的百积木工作区授权；
- 用户可手动新增多个 Codex 工作区；每个工作区使用独立 `CODEX_HOME` 承载会话、历史、技能和其他状态；
- ChatGPT 与百积木授权作为全局认证通道目录供每个 Codex 工作区独立选择，切换时只原子更新目标工作区的 `auth.json` 与 provider 和认证存储等认证相关 `config.toml` 项；模型始终由 Codex 或用户选择；
- 旧版本已经为百积木工作区建立的独立 Codex Home 会原路径登记为“历史导入”Codex 工作区，与默认及新建工作区并列显示；历史认证档案继续作为认证通道保留；
- Windows 直接启动可信 AppX 包的 FullTrust 可执行入口，macOS 通过 LaunchServices 启动，不写用户环境变量、不广播环境变化；
- 从旧版隔离档案升级时只迁移工作区凭证，旧会话目录原样保留，不自动合并状态数据库。

安装流程只负责桌面应用、工作区凭证和本地配置，完成后进入 Codex 工作区。安装、刷新状态和启动不发送模型请求，不验证上游模型路由，也不指定验证模型。模型由 Codex 或用户选择，调用结果由实际对话呈现；桌面管理器保留本地进程健康检查以及真实的安装和启动错误。升级时会清理旧版本留在成功安装记录中的路由验证警告。

Windows 安装 MSIX 因权限不足失败时，失败页提供“以管理员权限重试安装”和“打开安装包所在目录”。重试复用已下载的包并重新校验 SHA256，只为 MSIX 安装弹出 UAC；授权取消后保留重试入口，配置与凭证始终由原用户进程处理。MSIX 按用户注册，若 UAC 使用了另一个管理员账号，将在安装前停止并提示联系管理员为当前用户安排安装。安装包丢失时回到普通下载重试；手动安装完成后点击重试即可继续配置。

本应用不安装 Codex CLI，不启动 `codex app-server`，也不声明 Relay 远程能力。CLI、session/thread/turn/event 接口由独立的 appId `04166b2e-a08f-11f1-8622-00163e3536cb`（Codex 远程连接器）负责；OpenAI 兼容模型接口继续由 appId `06042dd8-a08f-11f1-8622-00163e3536cb`（Codex 模型接口服务）负责。

## 状态所有权

Bridge Agent 按 appId `codex` 注入迁移后的 `BAIJIMU_LOCAL_APP_DATA_DIR`。认证通道凭证、ChatGPT 授权快照和新增工作区目录保存在该本地应用私有目录；现有 `~/.codex` 原样登记为默认工作区。客户端启动和状态刷新只识别现场，不改写任何工作区的 `auth.json` 或 `config.toml`。只有用户创建工作区或明确切换认证通道时，本应用才管理目标工作区这两个文件中的认证内容；其余文件始终不动。

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
