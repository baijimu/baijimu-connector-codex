# Codex 默认状态目录所有权与启动规范

## 结论

安装 `com.baijimu.connector.codex` 即表示当前系统账户的默认 `~/.codex` 由百积木管理。当前版本不提供个人 Codex 环境或第二套 Codex Home；如果以后需要完全独立的 Codex 工作区，应作为单独产品能力设计和授权，不能复用工作区切换入口隐式实现。

## 所有权边界

百积木管理默认 `.codex` 中的：

- `auth.json`：当前生效工作区的 LLM credential；
- `config.toml` 中的百积木模型、路由、认证存储、审批和桌面语言配置项；
- `.baijimu-owner.json`：目录所有权与受管文件声明。

以下内容在所有百积木工作区之间共享，初始化、切换和重新授权均不得移动、删除或重建：

- 会话、任务和历史记录；
- SQLite 与其他 Codex 状态数据库；
- 日志、技能、缓存和用户自定义文件；
- `config.toml` 中不属于百积木的配置项。

每个工作区自己的 credential 保存在 Connector 私有数据目录的哈希键目录中。运行时路径不得暴露工作区 ID，也不得把 credential 写入进程参数、日志或用户环境变量。

## 初始化

1. 验证当前设备确实获得目标工作区授权。
2. 已有有效的私有 credential 时幂等复用；缺失时只在首次初始化中签发。
3. 合并写入默认 `.codex/config.toml` 的受管配置项，保留其他合法 TOML 配置。
4. 写入目录所有权标记。
5. 首个已初始化工作区自动成为当前工作区，并把它的 credential 原子写入默认 `.codex/auth.json`。
6. 安装器只检查 Rust 凭证管理器生成的默认 `.codex`，不设置 `CODEX_HOME`。

安装接管既有 `.codex` 时不迁移或删除已有状态，只替换受管认证文件并更新受管配置项。无法解析的现有 `config.toml` 必须明确失败，不得静默覆盖。

## 工作区切换

切换必须遵循固定顺序：

1. 验证工作区仍被当前设备授权，且私有 credential 可读。
2. 停止当前 ChatGPT/Codex 桌面进程，等待进程退出。
3. 使用同目录临时文件和原子替换更新默认 `.codex/auth.json`；凭证内容相同时不重写。
4. 更新当前工作区元数据。
5. 启动官方桌面应用。

Windows 直接调用安装包声明的稳定 `codex:` 协议，由 Windows Shell 解析当前安装版本；运行期不得为定位
可执行文件枚举 AppX 包或读取应用清单。工作区切换需要停止既有桌面进程时，只查询版本化产品配置声明的
进程名，并在执行停止前校验可执行文件具有有效且受信任的 OpenAI Authenticode 签名。AppX 包身份、最低
系统版本和安装位置只属于安装与升级校验职责。该路径不得写 `HKCU\Environment`，不得调用
`SendMessageTimeout` 或发送 `WM_SETTINGCHANGE`。

macOS 使用系统安装的 ChatGPT/Codex 应用路径启动，并从子进程环境中移除 Connector 私有变量和继承的 `CODEX_HOME`。

## 重新授权

重新授权只签发并更新目标工作区的私有 credential。目标工作区当前生效时，先停止桌面应用，再同步默认 `.codex/auth.json`，最后仅在原来正在运行时重启；非活动工作区不得影响当前桌面进程或默认认证文件。

## 旧版本升级

元数据版本升级时：

- 从每个旧工作区 Home 的 `auth.json` 读取 credential 并写入 Connector 私有凭证库；
- 所有工作区元数据的 `codexHome` 统一归一化为默认 `.codex`；
- 活动工作区 credential 同步到默认 `.codex/auth.json`；
- 旧隔离目录、会话、数据库、日志和配置全部保留原位；
- 不自动合并多个状态库，避免主键、任务和 SQLite 状态冲突。

旧版本遗留的用户级 `CODEX_HOME` 只允许在能证明由 Connector 写入时显式清理。清理后提示用户重启既有终端；不做 Windows 环境广播。普通初始化、切换、启动和健康检查路径都不得读写用户级环境配置。
