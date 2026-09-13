# 应用版本发布

唯一入口是 `.github/workflows/release.yml`。构建、签名、GitHub Release 和 OSS 上传沿用原流水线；市场登记由 `publish-market.sh` 启动 Python 3 标准库实现 `publish_market.py`，直接调用来源环境的 Partner API，不下载百积木 CLI。

GitHub 仓库必须配置以下值，脚本不内置环境地址或工作区：

| 配置 | 类型 | 用途 |
| --- | --- | --- |
| `LOCAL_APP_MARKET_PUBLISH_TOKEN` | Secret | 有来源应用管理权限的 PAT |
| `LOCAL_APP_OWNER_WORKSPACE_ID` | Variable | 来源应用所属工作区 |
| `LOCAL_APP_API_BASE_URL` | Variable | 来源环境 HTTPS API 基地址 |
| `LOCAL_APP_SOURCE_ENVIRONMENT_KEY` | Variable | 来源环境登记的 environmentKey |
| `LOCAL_APP_PUBLIC_ARTIFACT_BASE_URL` | Variable | 已登记公共 OSS 分发基地址 |

PAT 通过 `Authorization: Bearer` 发送至配置的来源环境，工作区通过 `X-Workspace-Id` 传递。来源环境负责向中心市场提交；流水线不向中心市场传递 PAT。认证请求不跟随重定向，公共制品下载不携带认证头。

发布器使用 `/partner/v1/local-app-service/api/local-apps`：先校验应用所有权，验证 OSS 制品摘要，上传原始字节，再以原始 `connector.json` 冻结准确版本。冻结记录引用来源拥有的 artifactId；不构造旧版 URL 类型版本体，也不改写 manifest。冻结后回下载验证每个制品，再调用该版本的 `/submit` 并读取 `/publication` 回执。

已公开 Release 的恢复仍使用同一工作流：在 `main` 上运行 `release_ref=v<version>, publish=true`。应用清单和制品来自不可变标签/Release；发布工具来自执行本次工作流的准确 `github.workflow_sha`，且必须已合入主线。恢复不重建二进制、不移动标签、不覆盖 Release 制品。已冻结版本必须逐字节匹配；请求结果不确定时先回查，`RECEIVING` 使用来源拥有的同一提交尝试继续传输。`REJECTED`、`WITHDRAWN` 需要独立明确处理，不自动重新申请。

`PENDING_REVIEW` 表示提交成功并等待独立审核；只有 `PUBLISHED` 才表示市场已发布。流水线不执行管理员审核。

发布工具测试：

```sh
python3 -m unittest discover -s tools/release/tests -v
```
