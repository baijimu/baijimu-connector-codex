# Codex 配置写入范围

## 登录方式

管理器不限制 Codex 登录方式。自 2.1.3 起，不再生成或恢复
`forced_login_method`，路由就绪检查也不要求该字段。
显式应用任一认证通道时，会移除目标 `config.toml` 中旧版留下的该字段；
旧授权快照中的 `forcedLoginMethod` 被忽略，不再保存到新快照。
启动和状态读取保持只读，不批量改写用户已有 Codex Home。

## 选择百积木认证通道时

`src/credential.rs` 对目标 Codex Home 执行以下更新：

| 文件 / 字段 | 写入值 | 用途 |
| --- | --- | --- |
| `config.toml` / `model_provider` | `baijimu-router` | 选择百积木模型路由 |
| `config.toml` / `cli_auth_credentials_store` | `file` | 使 Codex 读取管理器写入的文件凭证 |
| `model_providers.baijimu-router.name` | `baijimu-router` | provider 名称 |
| `model_providers.baijimu-router.base_url` | 产品配置中的百积木路由地址 | 模型请求目的地址 |
| `model_providers.baijimu-router.wire_api` | `responses` | 请求协议 |
| `model_providers.baijimu-router.requires_openai_auth` | `true` | 使用 Codex 认证凭证发送请求 |
| `auth.json` / `OPENAI_API_KEY` | 所选工作区授权凭证 | 请求鉴权，禁止记录凭证值 |
| `auth.json` / `auth_mode` | `apikey` | 描述当前凭证类型，不是登录方式限制 |

`auth.json` 是完整切换：原授权保存到对应认证档案，再写入所选授权。
不是把新 key 合并进原 ChatGPT token 对象。

## 选择 ChatGPT 认证通道时

恢复该通道保存的 `auth.json`；尚未登录时移除目标 `auth.json`，让用户登录。
恢复快照中的 `model_provider`、`cli_auth_credentials_store` 和百积木 provider
配置段；快照未包含的这些字段会被删除。新建 ChatGPT 通道不预设这些字段。
无论旧快照内容如何，都不会恢复 `forced_login_method`。

## 工作区目录和其他状态

当前主线在 Windows 显式打开或切换工作区时，将用户级 `CODEX_HOME` 指向所选
目录并广播环境变化；该行为会影响以后启动的其他 Codex 进程。
macOS 启动时向目标进程传递所选 `CODEX_HOME`。
管理器还维护认证档案、工作区目录、安装状态和 `.baijimu-owner.json` 所有权标记。

管理器不写入模型选择、推理强度、审批策略、沙箱模式、会话历史或 Codex 界面语言。
管理器不配置验证模型，也不执行模型路由探测；`defaultUiLocale` 仅用于管理器安装界面。

## 升级验证

旧版配置已手工移除登录限制的设备，升级后应正常识别路由就绪状态。
其他设备可显式重新应用当前认证通道来移除旧限制。回归测试覆盖新配置、旧 API / ChatGPT
限制、旧授权快照恢复、认证通道往返切换，以及模型/审批/沙箱配置保留。
