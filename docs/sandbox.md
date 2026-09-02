# Bash 沙箱（`sandbox.*`，实验性）

picrab 的 bash 工具可选接入 OS 级沙箱：macOS 经 `sandbox-exec`（Seatbelt），Linux 经
bubblewrap + seccomp。实现来自 [`sandbox-runtime`](https://github.com/defims/sandbox-runtime-rs)
crate——`anthropic-experimental/sandbox-runtime`（srt）的行级 Rust 移植，行为基准与偏差
清单见该仓库的 `UPSTREAM_BASE.md`。

## 定位与边界

沙箱是**风险缓解层，不是安全边界**（与上游 srt 口径一致）：它防模型误删工作区外的文件、
防凭据目录被读取、把出网限制在白名单代理上；它不防内核级提权、不防已授权写入的破坏、
广域白名单下数据仍可外泄（domain fronting 可绕过域名过滤）。它与 approval / mediation
（spawn 前分类）正交互补。

## 配置

`~/.pi/agent/settings.json`（项目级 `.pi/settings.json` 同样支持）：

```json
{
  "sandbox": {
    "mode": "auto",
    "network": "allowlist",
    "allowedDomains": ["github.com", "*.github.com"],
    "deniedDomains": [],
    "denyRead": ["~/.ssh", "~/.gnupg", "~/.aws"],
    "allowWrite": [".", "/tmp", "/private/tmp"]
  }
}
```

CLI 等价开关（优先级高于配置）：`pi --sandbox <off|auto|on>`。

| 字段 | 默认 | 说明 |
|---|---|---|
| `mode` | `off` | `off` 不沙箱；`auto` 平台不支持时告警降级；`on` 不支持即报错（fail-closed） |
| `network` | 跟随 `mode` | `auto`/`on` 默认 `allowlist`（全部出网走本地过滤代理）；`off` 为不设网络层（直连放行） |
| `allowedDomains` | `["*"]` | 代理白名单，`*` 全放行；被拒连接返回 403/EPERM |
| `deniedDomains` | 空 | 优先于白名单 |
| `denyRead` | `~/.ssh` `~/.gnupg` `~/.aws` | 额外继承上游 mandatory deny 写保护（`.zshrc`/`.gitconfig`/`.git/hooks` 等） |
| `allowWrite` | cwd + `/tmp` + `/private/tmp` | 相对路径按执行 cwd 解析 |
| `allowUnixSockets` / `allowLocalBinding` / `denyWrite` | 空/false | 同上游语义 |

## 行为要点

- `mode=off` 与无沙箱路径字节级等价（不创建 manager、不绑端口）。
- 沙箱内本地 DNS 解析受限（fail-closed）：代理感知工具（curl/git/pip）自动经代理远端
  解析不受影响；ssh、数据库驱动等非代理感知工具的直连会被兜底拦截——属预期行为，
  放宽方式是调整白名单或 `network: off`。
- 命令输出中出现 `Operation not permitted` / `CONNECT tunnel failed, response 403`
  等特征时，工具结果会附带 `[SANDBOX]` 指引（面向模型：改用白名单内路径/域名重试或
  建议用户调整配置）。
- 每个网络策略共享一对本地代理（127.0.0.1 动态端口），随进程生命周期存在；多会话并发
  安全（profile 按 `(cwd, 配置, 端口)` 派生）。

## 嵌入方（moho-mate 等 SDK 宿主）

`Config.sandbox` 由 settings 文件驱动，经 `default_tool_registry` 自动生效，无需额外
接线。端到端示例见 `examples/sandbox_e2e.rs`（off 零差异 / 凭据拒读 / 白名单拦截 /
PTY 路径 / 双会话共享 manager）。
