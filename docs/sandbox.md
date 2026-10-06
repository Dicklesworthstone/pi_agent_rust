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

## 默认值（重要）

**未写 `sandbox` 段 = 默认开启**：`mode=auto` + `network=off`，即文件系统防护兜底生效（凭据目录拒读、工作区外拒写），网络不限。域名白名单是 opt-in（写 `network: "allowlist"`）。这是 moho-mate fork 的默认；上游 picrab CLI 用户如需旧行为，显式写 `"sandbox": { "mode": "off" }`。

**写段即显式控制**：只要存在 `sandbox` 段，未写的字段回到字段级默认（`mode` 缺省 = `off`）。

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
| `mode` | 段缺失=auto；段存在时缺省=off | `off` 不沙箱；`auto` 平台不支持时告警降级；`on` 不支持即报错（fail-closed） |
| `network` | 段缺失=off；段存在时跟随 `mode` | `auto`/`on` 默认 `allowlist`（全部出网走本地过滤代理）；`off` 为不设网络层（直连放行） |
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

## Windows 支持(2026-10 起)

Windows 走 fork 的 `srt-win` 后端,**机制与 macOS/Linux 本质不同**:专用本地账户
(`srt-sandbox`)+ WFP 内核围栏 + 会话级 NTFS ACL,两跳启动(broker →
CreateProcessWithLogonW 拉起 runner → 受限 token + Job Object 拉起命令)。

### 安装(一次性)

```
srt windows-install
```

弹一次 UAC(幂等;可 `--proxy-port-range LOW-HIGH` / `--sandbox-user`,须与
`sandbox` 配置一致)。运行时**无需管理员**。srt 更新后若内嵌 helper 变化,首次使用会
提示再装一次。

### Windows 平台差距(相对 macOS/Linux)

- **默认权限反转**:沙箱账户对用户目录**零读取权**。`~/.gitconfig`、`~/.cargo`、
  `~/.npm` 等默认不可见——用 `sandbox.allowRead` 显式授予(Windows 专属字段);
  项目目录本身已随 `allowWrite` 授予。
- **bash/工具链必须机器级安装**(如 `choco install git` 到 `C:\Program Files\Git`);
  per-user 安装对沙箱账户不可读,启动即报 `ShellNotReadable` 并给出指引。
- **WSL shell 不支持**(会落到沙箱账户的空 WSL 环境)。
- **PTY 永久禁用**:沙箱化命令一律无 TTY(isatty 型命令行为变化,自动回退管道)。
- **每命令沙箱化启动较慢**(两跳 + 新登录会话)。
- 系统解析器 DNS 不围栏(与 macOS 一致);schannel 证书吊销检查会被围栏拦截
  (`CRYPT_E_REVOCATION_OFFLINE`,需按工具禁用吊销检查)。
- 违规归因(violation store)在 Windows 不可用(无 Seatbelt 日志流等价物)。
- `sandbox.allowRead/allowWrite/denyWrite` 现已全平台可配置(unix 上同样生效)。
