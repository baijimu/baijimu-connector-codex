# CODEX-2026-10-03-001：旧 CODEX_HOME 长路径导致 Windows 子进程 ENOTCONN

- 状态：单设备数据迁移完成，目录子进程与桌面启动验证通过；原业务执行未代替用户触发。
- 首次现场确认：2026-10-03；责任组件：Codex 桌面管理器历史数据迁移，OpenAI Codex 子进程异常处理。
- 现场版本：桌面管理器 2.1.1、Bridge Agent 0.10.3、OpenAI.Codex 26.928.3736.0。
- 关键词：`A JavaScript error occurred in the main process`、`read ENOTCONN`、`spawn ENOENT`、`createSocket`、`ChildProcess.spawn`、Git 超时、Windows 长路径、历史 CODEX_HOME。
- 范围：已确认一台保留旧托管长目录的 Windows 设备；客户身份和原始日志留在诊断工单及设备本地，不纳入公开仓库。

## 症状与证据

打开、继续已有任务时弹出主进程 JavaScript 错误。错误窗口位于后台，普通桌面截图可能看不到，常规 Codex 日志也未记录这条异常。通过只读 UI Automation 读取所属 Codex 进程的错误窗口，取得完整堆栈：

```text
Error: read ENOTCONN
  at tryReadStart (node:net:921:20)
  ...
  at createSocket (node:internal/child_process:337:14)
  at ChildProcess.spawn (node:internal/child_process:492:23)
  at spawn (node:child_process:827:9)
  at Object.ai (.../application-network-startup-D74LEWDz.js:14:8134)
```

检查现场已安装包的上述准确位置，确认这是通用 `spawn` 包装器，使用 stdout/stderr pipe；包装器只给 ChildProcess 注册 error 处理，没有给这两个流注册 error 处理。文件名含 network 不代表模型网络请求出错。

同一台设备用 Node 24.14.0、同一个已存在的 `git.exe --version` 做只读对照：

| cwd | 长度 | 结果 |
| --- | ---: | --- |
| 用户目录 | 14 | 成功 |
| 历史 CODEX_HOME | 197 | 成功 |
| 可视化目录的父目录 | 223 | 成功 |
| 历史 Home 下该会话的 visualization 目录 | 260 | ChildProcess 报 ENOENT，stdout/stderr 均报 ENOTCONN |
| 260 字符目录规范化分隔符后 | 260 | 同样失败 |
| 同一目录使用 Windows 扩展路径前缀 | 264 | 成功 |

所有测试目录和 Git 可执行文件均存在。单看 ENOENT 不能判断 Git 未安装。主会话 cwd 位于 Documents 下；触发失败的是桌面端同时检查的 Home 下 visualization 路径，不能只检查主 cwd。

## 根因及责任边界

历史托管 CODEX_HOME 深度过大，加上正常的可视化子目录后，普通 Windows 子进程启动路径达到长度限制。启动失败后管道产生 ENOTCONN，Codex 没有完整处理该流错误，最终显示未捕获异常弹窗。

百积木负责产生并继续使用过长历史目录的集成问题；上游 Codex 负责对启动及管道失败进行正确的异常处理。本次没有修改上游安装包，使用缩短实际数据路径的恢复方案。

这不是凭据状态查询锁、模型网关断线或未登录 ChatGPT 的证据。另行发现的云端登录提示、插件 GitHub 网络错误以及此前 MSI 升级耗时约两分钟不能并入此根因。

## 为什么以前修过仍出现

1. 2026-08-11 的 [780974b](https://github.com/baijimu/baijimu-connector-codex/commit/780974bbdac41f4e6cf73475ac6027bd089e913c)，版本 1.2.42，已把隔离档案路径缩短为 `~/.baijimu/codex/p/<短标识>`，包含旧目录迁移。
2. 1.4.0 的 `75dd8e1` 共享 Home 重构移除了原有目录搬迁，改为凭据档案迁移；后续工作区模型保留历史目录，不自动合并会话数据库。
3. 现场 `originalCodexHomeState.captureSource=user-environment`，旧托管长目录被登记成用户原始目录；`default` 工作区继续指向它，`restoreRequired=false`，初始化状态为 ready。
4. 因此程序升级到 2.1.1 不等于该客户的数据目录已修复。新装且无遗留 CODEX_HOME 的默认目录较短，但不能据此保证任意自定义路径、所有新增工作区或其他存量设备都不受 Windows 长路径影响。

## 本次独立恢复范围

用户授权对该设备执行一次数据迁移，不增加启动、查询时的隐式迁移，也不开发新的自动迁移系统。

前置检查发现默认 `~/.codex` 已有另一份数据，故使用新的短目录 `~/.baijimu/codex/h/<随机短标识>`，不覆盖或合并原默认目录。路径和标识在本次操作计划中确定，不能作为其他设备的固定值。

执行顺序：

1. 读取活动工作区及真实 Home，核对没有并发管理操作；保存管理器元数据、原 CODEX_HOME 和独立操作日志。
2. 停止桌面管理器和目标 Codex 进程树，确认进程已退出，数据库不再被写入。不能只凭 PID 仍可枚举就判断进程存活。
3. 将完整数据复制到全新短目录，保留文件权限；逐文件校验内容哈希。源目录保持原样作为恢复副本。
4. 在副本内迁移结构化路径：会话 rollout 位置、桌面状态与可写根目录、配置中引用及 JSON/数据库内实际路径值。保留历史消息叙述及日志原文，不对所有文本做无差别替换。
5. 检查每个数据库完整性、会话数量、rollout 与工作目录存在性；认证文件保持原内容。
6. 更新本地应用工作区登记、原始 Home 记录和用户级 CODEX_HOME，回读一致后启动管理器与 Codex。
7. 验证原会话目录的 Git 子进程、原会话可见性、应用连接状态与错误窗口。业务任务是否继续成功单独记录，不能用进程启动成功替代。

## 验收记录

2026-10-03 单设备验收：

- 172 个文件，共 377,355,012 字节，复制后逐文件 SHA-256 一致；6 个会话保留，全部 cwd 与迁移后的 rollout 文件存在。
- 7 个 SQLite 数据库完整性检查均为 `ok`；更新 10 个主状态字段、2 个历史 JSON 字段及会话/桌面结构化路径。认证文件内容保持一致。
- 复制时 4 个沙箱程序文件报告 NTFS 权限设置失败；停止提交并只读复核，4 个文件源与副本的 SDDL 实际全部相同，文件内容也一致。未提权、未放宽权限、未重试绕过访问控制。
- 原 Home 和用户原有另一份 `~/.codex` 均保留。新 Home 长度 56，故障会话 visualization 目录长度 119。
- 工作区登记、原始 Home 记录及用户 CODEX_HOME 三者回读一致；管理器本地状态 ready，Codex 配置有效。
- 启动后第一次同步 Git/窗口检查出现超时。随后使用与故障复现相同的异步 spawn：用户目录 1,348 ms、迁移目录 1,546 ms，均正常退出且无 ENOENT/ENOTCONN。首次启动延迟未据此认定已根治。
- Codex 窗口正常响应，没有 Error 弹窗；最近会话列表中原 ERP 会话存在。本地 app-server 初始化连接成功。
- 启动日志另有插件 Git 同步超时、GitHub HTTP 429 与 MCP 握手超时；属于未在本次迁移中修复的独立问题。未发起 ERP 业务部署或续跑消息，业务任务继续执行仍由用户验证。

## 回滚

操作日志及元数据备份位于设备用户私有迁移目录，完整源数据保留在原 Home。回滚前停止相关写入，将迁移后新数据另行保留，再恢复备份的管理器元数据和原 CODEX_HOME。不能直接覆盖迁移后产生的新会话或业务文件。

回滚只恢复迁移前状态，旧长路径故障也会随之恢复，不是该问题的长期解决办法。
