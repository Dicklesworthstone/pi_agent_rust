# defims/picrab ↔ earendil-works/pi 对齐

> **中文版** · English (default): [sdk-mapping.md](./sdk-mapping.md)
>
> `defims/picrab` 是 pi 引擎([earendil-works/pi](https://github.com/earendil-works/pi),
> TypeScript SDK `@earendil-works/pi-coding-agent`)的 Rust 版本,持续跟随上游。
>
> 本文档记录两者的 `AgentSession` 接口差异,作为补齐 in_process 方法、追上游 release
> 时检测变更的基线。
>
> - **对齐基准**:earendil-works/pi 的 `AgentSessionLike` 接口(agegr/pi-web `lib/pi-types.ts`,跟踪 **v0.84.4**)
> - **被对齐对象**:defims/picrab 的 `AgentSessionHandle`(in_process 路径,`src/sdk.rs`)
> - **Rust crate lib 名**:`pi`(Cargo 包名 `pi_agent_rust`)

## 核心规则

- **文档默认英文**:正文默认英文(`X.md`);中文版放 `X.zh-CN.md`,文件头互相链接;内容变更时两份同步更新。
- **提交用英文**:本仓库 git commit message 一律英文。

## fork 核心要求

消费方(moho-mate)进程内嵌引擎(in_process 路径),对 fork 的三个核心要求,补齐方法/追上游时不得破坏:

1. **能并行** — 多个 `AgentSessionHandle` 实例可同时运行(多会话并发),handle 之间不得引入全局锁或共享可变状态把并发串行化。
2. **异步的并行** — 并行经 async 任务实现(引擎跑在 asupersync runtime 上,调用方自建 runtime),不阻塞 OS 线程;事件流经 `on_event`/`subscribe` 回调送出。
3. **异步** — 全链路 `async fn`,形态对齐 TS SDK 返回 Promise 的方法;长操作(prompt/bash/compact)可经 `AbortHandle`/`Signal` 随时取消,不阻塞调用方。

## 状态图例

- ✅ **已对齐** — 两边都有,语义一致(可能有参数形态差异)
- 🟡 **形态不同** — 语义对齐,但调用方式/参数/返回类型不同(语言范式差异)
- ❌ **缺口** — TS SDK 有,Rust 无(待补)
- ⏭️ **架构差异** — 扩展/custom-ui 系统,Rust 不实现

---

## 1. 会话核心(🟡 部分对齐)

| TS SDK (`AgentSessionLike`) | Rust (`AgentSessionHandle`) | 说明 |
|---|---|---|
| `prompt(text, options?)` | `prompt(input, on_event)` / `prompt_with_abort(input, signal, on_event)` / `prompt_images_with_abort(input, images, signal, on_event)` | 🟡 Rust 拆成 3 个变体;TS 图片走 options.images,Rust 有专用变体(fork commit e285868e) |
| `abort()` | `new_abort_handle()` + `AbortHandle.abort()` | 🟡 Rust 需预建 AbortHandle+Signal |
| `subscribe(listener)` | `subscribe(listener)` / `unsubscribe(id)` | ✅ Rust 多了显式 unsubscribe |
| `dispose()` | `into_inner()` | 🟡 Rust 消费 handle 取内部 session |
| `reload(options?)` | `continue_turn(on_event)` / `continue_turn_with_abort(...)` | 🟡 语义不完全等价 |
| `setModel(model)` | `set_model(provider, model_id)` | 🟡 TS 传对象,Rust 拆 provider+model_id |
| `model` (readonly) | `model() -> (String, String)` | ✅ |
| `setThinkingLevel(level)` | `set_thinking_level(level)` | ✅ |
| `setSessionName(name)` | `set_session_name(name)` | ✅ |
| `compact(customInstructions?)` | `compact(on_event)` / `compact_with_instructions(instructions, on_event)` | ✅ fork 补了 instructions 版 |
| `steer(text, images?)` | handle 上无(仅 RPC 路径:`RpcTransportClient::steer`) | ❌ |
| `followUp(text, images?)` | handle 上无(仅 RPC 路径) | ❌ |
| `sessionId` (readonly) | `session().session.lock().header.id` | 🟡 需 async 锁 |
| `sessionFile` (readonly) | `session().session.lock()` 读文件路径 | 🟡 |
| `isStreaming` (readonly) | 无(`AgentSessionState` 无此字段,需经事件流追踪) | ❌ |
| `isCompacting` (readonly) | 无(`AgentSessionState` 无此字段) | ❌ |

## 2. 统计 / bash / 压缩(✅ fork 已补)

| TS SDK | Rust | 说明 |
|---|---|---|
| `getSessionStats()` | `get_session_stats()` | ✅ fork commit e178fb48 |
| `getLastAssistantText()` | `get_last_assistant_text()` | ✅ fork commit e178fb48 |
| `setAutoCompactionEnabled(b)` | `set_auto_compaction(b)` | ✅ fork commit e178fb48 |
| `compact(customInstructions?)` | `compact_with_instructions(...)` | ✅ fork commit e178fb48 |
| `executeBash(cmd, onChunk?, opts?)` | `bash(cmd, abort_rx)` | 🟡 fork commit 52d39dc3,参数形态不同(abort 用 oneshot,无 onChunk) |
| `autoCompactionEnabled` (readonly) | 无(`AgentSessionState` 无此字段,只能 set) | ❌ 缺 getter |

## 3. 队列管理(❌ 缺口)

TS SDK 有队列(steering/followUp)的读写 + 清空,Rust 的 pi 内部 queue 是私有的。

| TS SDK | Rust | 说明 |
|---|---|---|
| `clearQueue()` | 无(pi 内部 queue 私有) | ❌ moho-mate 用 LoopState 镜像清 |
| `getSteeringMessages()` | 无 | ❌ |
| `getFollowUpMessages()` | 无 | ❌ |
| `pendingMessageCount` (readonly) | 无 | ❌ |

## 4. 工具管理(🟡 部分对齐)

Rust 有 `ToolRegistry` 和 `Agent::tools()`,但部分查询/设置方法未在 handle 上暴露。

| TS SDK | Rust | 说明 |
|---|---|---|
| `getAllTools()` | `Agent::tools()` + `extension_tool_defs` (引擎层有,handle 无直接包装) | 🟡 引擎已有,handle 包装待加 |
| `getActiveToolNames()` | 同上(工具列表可枚举,活跃态查询 handle 无) | 🟡 |
| `setActiveToolsByName(names)` | 无(moho-mate 用 set_system_prompt 近似) | ❌ PARTIAL |

## 5. 导航(❌ 缺口)

| TS SDK | Rust | 说明 |
|---|---|---|
| `navigateTree(targetId, opts?)` | 底层 `Session.navigate_to` 有,handle 无包装 | 🟡 走 session_mut().session.lock().navigate_to |

## 6. bash 辅助状态(🟡 部分对齐)

| TS SDK | Rust | 说明 |
|---|---|---|
| `abortBash()` | 由调用方管 oneshot 通道,handle 无直接方法 | 🟡 实现方式不同,功能等价 |
| `isBashRunning` (readonly) | 无 | ❌ 缺状态查询 |

## 7. 压缩 / retry / 上下文(✅ fork 已补)

| TS SDK | Rust | 说明 |
|---|---|---|
| `abortCompaction()` | `compact()` 本身可取消(AbortHandle) | ✅ compact 通过 AbortSignal 可取消 |
| `setAutoRetryEnabled(b)` | `set_auto_retry(b)` (引擎层已有) | ✅ |
| `autoRetryEnabled` (readonly) | 通过 `state()` 可查 | 🟡 间接获取 |
| `getContextUsage()` | 通过 `get_session_stats()` 中的 `contextUsage` 字段 | 🟡 已有等价信息 |

## 7.5. SessionManager(✅ 已对齐)

pi-web-rust 通过 `pi::sdk::SessionIndex` + `pi::sdk::SessionMeta` + `pi::sdk::build_session_context` 接线。

| TS SDK | Rust | 状态 |
|---|---|---|
| `SessionManager.listAll()` → `SessionInfo[]` | `SessionIndex::new().list_sessions(None)` → `Vec<SessionMeta>` | ✅ SessionMeta 含 first_message/parent_session_path/modified_ms |
| `SessionManager.open(path).getEntries()` | `Session::open(path)` + `session.entries` 公开字段 | ✅ |
| `buildSessionContext(entries, leafId, byId)` | `pi::sdk::build_session_context(entries, leaf_id, by_id)` → `SessionContextSnapshot` | ✅ 自由函数(leaf→root 走 parentId + compaction 截断) |
| `resolveModelScopeWithDiagnostics(patterns, modelRuntime)` | `pi::sdk::resolve_model_scope_with_diagnostics(patterns, registry, allow_missing)` → `(Vec<ScopedModel>, Vec<String>)` | ✅ 返回结构化 diagnostics |
| `getAgentDir()` | `Config::global_dir()` | ✅ |

## 8. 设置 / 模型运行时(🟡 部分对齐)

| TS SDK | Rust | 状态 |
|---|---|---|
| `settingsManager` (readonly) | 无 | ❌ 缺设置管理器 |
| `modelRuntime` (readonly) | `agent.model_registry()` / `agent.auth_storage()` | ✅ 已加 pub getter |
| `agent.prepareNextTurnWithContext(context, signal?)` | 无 | ❌ 上游 0.84.4 新增 |

> **上游 0.84.4 类型扩展**(不新增行):`ToolInfo`新增 `parameters?`、`promptGuidelines?`、`sourceInfo?`;`ResourceLoaderLike`新增 `getAgentsFiles()`;`executeBash` options 新增 `operations?: BashOperations`。这些都是返回类型/参数类型扩展,没有新 handle 方法。

## 9. 扩展系统(✅ 2026-08-20/22 已完成对接)

TS SDK 内嵌 DefaultResourceLoader(skills/extensions 自动加载);Rust 原版留在 CLI 层——
我们的 fork 从两端(web wire + SDK 自动加载)补齐了这个缺口,custom-UI poll 协议让
`extension_ui_input` 成为 `respond_ui` 的薄封装。

| TS SDK | Rust | 状态 |
|---|---|---|
| `extensionRunner` (readonly) | `extension_manager()` / `has_extensions()` | ✅ 已对接(UI 通道 + tools/commands RPC 已通) |
| `promptTemplates` (readonly) | `get_commands()` 三源之一(load_prompt_templates) | ✅ |
| `resourceLoader` (readonly) | auto-load:skills 四源(f74dd3a8)+ 扩展自动发现(88abc5f1, `SessionOptions::no_extensions`) | ✅ |
| `bindExtensions?` | `enable_extensions_with_policy`(显式路径注入面) | ✅ |

## 10. 自定义消息(❌ 上游 0.84.4 新缺口)

上游 `@earendil-works/pi-coding-agent` v0.84.4(2026-08-28)在 `AgentSessionLike` 上新增了
发送任意自定义消息的方法。

| TS SDK | Rust | 状态 |
|---|---|---|
| `sendCustomMessage<T>(message, options?)` | 无 | ❌ 上游 0.84.4 新增,Rust 尚无等价物 |

---

## handle 独有方法(不在 AgentSessionLike 中)

in_process handle 还暴露一批 TS SDK 没有的 Rust 侧方法:
`messages()`、`state()`、`thinking()` / `thinking_level()`、`max_tokens()` / `set_max_tokens()`、
`listeners()` / `listeners_mut()`、`session()` / `session_mut()`、`compaction_settings()`、
`ask_tool()`、`with_session()`、`extension_manager()` /
`has_extensions()` / `extension_region()`、`from_session_with_listeners()`。
多为 RPC 名称镜像或 Rust 原生接线所需;属增量,不计入对齐。

## 对齐进度(含上游 0.84.4)

| 类别 | 总数 | ✅ 已对齐 | ❌ 缺口 |
|---|---|---|---|
| 会话核心 | 16 | 12 | 4 |
| 统计/bash/压缩 | 6 | 5 | 1 |
| 队列管理 | 4 | 0 | 4 |
| 工具管理 | 3 | 2 | 1 |
| 导航 | 1 | 1 | 0 |
| bash 辅助 | 2 | 1 | 1 |
| 压缩/retry/上下文 | 4 | 4 | 0 |
| 设置/模型(含 agent 钩子) | 3 | 1 | 2 |
| 扩展 | 4 | 4 | 0 |
| 自定义消息(§10) | 1 | 0 | 1 |
| **合计** | **44** | **30** | **14** |

**已对齐率:68%**(30/44;上游 `@earendil-works/pi-coding-agent` 跟踪至 **v0.84.4**)。

## fork 补齐记录(defims/picrab)

| commit | 方法 | 对齐的 TS SDK |
|---|---|---|
| `f74dd3a8` | SessionOptions::skills + auto-load | resourceLoader (skills 半部) |
| `0f6d283a` | compact→CompactionResultInfo统计+ AgentSession::shutdown(flush+扩展停) | compact stats / dispose 语义 |
| `88abc5f1` | SessionOptions::no_extensions + discover_extensions_blocking 自动装配 | resourceLoader (扩展半部) |
| `213b7c80` | Agent::tools() | getTools 面 |
| `7a2a023d` | (session_index) 索引快照 first_message/modified 真值 | SessionManager.listAll 元数据 |
| `e178fb48` | get_session_stats / get_last_assistant_text / set_auto_compaction / compact_with_instructions | getSessionStats / getLastAssistantText / setAutoCompactionEnabled / compact |
| `52d39dc3` | bash | executeBash |
| `2595adae` | (macOS 类型修复,非方法补齐) | — |
| `baee8763` | (删除不在 AgentSessionLike 中的 RPC-only 方法:get_available_models / set_steering_mode / set_follow_up_mode / get_messages / get_state;steer/follow_up 仍只在 RPC 路径) | — |
| `86e8cac6` | SessionMeta 扩展(first_message/parent_session_path/modified_ms) + build_session_context 自由函数 + sdk 导出 SessionIndex/SessionMeta | SessionManager.listAll / buildSessionContext |
| `adfdc96c` | resolve_model_scope_with_diagnostics + AgentSession model_registry()/auth_storage() getters | resolveModelScopeWithDiagnostics / modelRuntime readonly |
| `36fdac58` | createAgentSessionServices / FromServices 拆分 | createAgentSessionServices / createAgentSessionFromServices |
| `e285868e` | prompt_images_with_abort — 文本+图片 content blocks | prompt with images(上游对齐) |
| `87af990e` | get_messages / get_state RPC 名别名加到 in_process handle | RPC 兼容别名 |
| `4f292bba` | SessionOptions::secrets — 嵌入式凭据卫生 seam | secrets 管理 |
| `40e48d4c` | SessionOptions::models_path — registry 覆写路径(测试隔离) | models path override |

## 追上游流程

本文档跟踪 **`@earendil-works/pi-coding-agent` v0.84.4**(2026-08-28)。
本地 `picrab-web` submodule 将这些依赖锁定在 **v0.84.2**
(`lib/pi-types.ts` 位于 commit `4d26aeb`)。

```bash
# 1. earendil-works/pi 发新版时,更新 AgentSessionLike 接口
#    对照本文档 §1-10 检查新增/变更的方法

# 2. 在本仓库(defims/picrab)补齐缺口
# 补方法到 src/sdk.rs 的 AgentSessionHandle impl 块
cargo check --lib  # 验证

# 3. push fork
git push origin master:main

# 4. 消费方 moho-mate(宿主仓库)更新 submodule + 验证
cd <moho-mate 根目录>
git add pi-agent-rust
cargo check
```

## 相关文件

- TS SDK 接口:上游 agegr/pi-web `lib/pi-types.ts`(`AgentSessionLike`;消费方 moho-mate 的 pi-web-rust submodule 内有此文件)
- Rust handle:`src/sdk.rs`(`AgentSessionHandle` impl)
- Rust 引擎核心:`src/agent.rs`(`AgentSession`)
- RPC 路径(参考):`src/rpc.rs`(`RpcTransportClient`)
- 详细签名:消费方 moho-mate 的 `docs/pi-sdk-probe-notes.md`
- English version: [sdk-mapping.md](./sdk-mapping.md)
