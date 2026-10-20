# Reach

[Teens-in-Times/reach-mcp](https://github.com/Teens-in-Times/reach-mcp)

通用 Agent 的远程手：独立 Rust MCP 服务，直接提供文件、补丁、一次性命令、持久终端与图片能力。不经过第二个 Agent，也没有 Python/Node 运行时胶水层。调用批准由 MCP 客户端负责；Reach 检查鉴权、权限、参数和资源归属后执行。

v0.3 是破坏性重构，不兼容 v0.2 的调用签名和任务记录。使用新的私有状态目录，保留旧实例供已有会话使用。

## Windows 启动

Windows x86_64 / MSVC、Rust 1.99 验证。下载便携包时应保留 reach.exe、conpty.dll 和 x64/OpenConsole.exe 的相对位置。完整终端使用固定版本、经过微软签名验证的 ConPTY。源码构建：

```powershell
cargo build --release --locked
./scripts/prepare-windows-runtime.ps1 -Destination ./target/release
# directory 必须是一个新的私有目录
./target/release/reach.exe init --directory D:\Reach\private --cwd G:\Project
./target/release/reach.exe --config D:\Reach\private\config.json serve --background
./target/release/reach.exe --config D:\Reach\private\config.json ui
```

实际 cargo target_directory 如被覆盖，以 cargo metadata 为准。打包脚本 scripts/build-windows-release.ps1 接受一个新的 Destination，构建便携 ZIP 和校验文件。ConPTY 来源、版本和许可证见 THIRD_PARTY_NOTICES.md。

默认 MCP 为 http://127.0.0.1:42373/mcp，管理页为独立的 127.0.0.1:42374。只监听回环地址；init --no-admin 禁用管理页。

## 文本密钥与管理页

```powershell
reach --config D:\Reach\private\config.json token issue --principal coding-agent --label "代码助手" --profile compact --scopes read,write,exec,tasks --root G:\Project --ttl-hours 2160 --token-file D:\Reach\private\coding-agent.token
reach --config D:\Reach\private\config.json token list
reach --config D:\Reach\private\config.json token revoke <credential-id>
```

密钥形如 reach_<id>.<secret>，秘密部分来自 OS 的 32 字节随机数；服务端仅保存 SHA-256。支持权限、工作目录、过期、撤销和轮换，每次请求重新检查。相同 principal 共享任务与终端归属；不同 principal 相互不可见。真实密钥放在客户端的私有配置中，不传入模型参数。

```json
{"mcpServers":{"reach":{"type":"http","url":"http://127.0.0.1:42373/mcp","headers":{"Authorization":"Bearer reach_REPLACE_WITH_YOUR_KEY"}}}}
```

具体配置字段依客户端而异，协议采用 Authorization: Bearer。私有远程连接通过可选 SSH 回环反向转发加密，不修改 VPN、路由或 DNS。此鉴权不是 OAuth；公网部署需要另行配置 TLS/OAuth 网关。

init 保护私有目录的当前用户/SYSTEM ACL。ui 生成三分钟单次登录票据，页面移除 URL fragment 后兑换 HttpOnly/SameSite=Strict Cookie；管理变更校验 CSRF 与来源。会话最多八小时、闲置三十分钟失效。MCP 密钥不能登录管理页，管理端口不会被自动转发。

## 工具与上下文

| 操作 | 能力 |
|---|---|
| Read / Glob / Grep | 批量 UTF-8、目录、搜索、忽略规则、行号和有界分页 |
| Write / Edit | 明确的创建、覆盖、追加、精确替换；版本保护及预览 |
| ApplyPatch / Recover | 多文件补丁、逐文件提交/回滚报告、持久原始内容与显式恢复 |
| Shell / Poll / Input / Cancel | 一次性命令、分开的 stdout/stderr、期限、输入、取消进程树 |
| Tasks | 找回任务结果、等待最终状态，断连不重跑 |
| TerminalOpen / Read / Input / Resize / Close | 真正的 PTY、持久 Shell、交互、全屏 TUI、屏幕快照和增量原始流 |
| Terminals | 找回自己的终端会话及生命周期状态 |
| Image / Info | 原生 MCP 图片；主机、身份、权限和实际终端后端能力 |

默认 compact 仅注册 Run 和 Discover。初始化 instructions 从 Rust 参数类型及 Schema 自动生成操作签名与使用规则；详细 Schema 通过 Discover 查询单项。native 暴露被授权的独立工具，便于逐工具审批。两者共用同一类型化执行核心。

```json
{"name":"Run","arguments":{"tool":"Read","arguments":{"paths":["src/main.rs"]}}}
{"name":"Discover","arguments":{"tool":"Write"}}
```

Reach 无法控制 host 是否采用初始化说明、每轮如何注入工具或模型供应商的缓存。compact 减少注册 Schema，也失去模型 API 层逐项参数约束；服务端仍校验类型和权限，Run 的注解保持保守。

## 文件、版本与恢复

Write 的 update 是带标签的结构，不存在隐式覆盖：

```json
{"path":"new.txt","update":{"mode":"create","content":"hello\n"}}
{"path":"existing.txt","update":{"mode":"replace","content":"new\n","expected_sha256":"HASH_FROM_READ"}}
{"path":"existing.txt","update":{"mode":"append","content":"more\n","expected_sha256":"HASH_FROM_READ"}}
```

Edit 与补丁修改已有文件也必须带当前 SHA-256。Write、Edit、ApplyPatch 支持 preview:true，返回摘要和有界 diff，不写文件。支持 UTF-8/BOM、CRLF、混合换行及没有最终换行符的文本；UTF-16/二进制修改明确拒绝。

Windows 提交持有排他句柄，在同一句柄上核验版本、写入和回滚，普通外部写入及重命名会被拒绝。**不承诺单文件或多文件崩溃原子性**。提交前原始内容会刷盘到私有状态目录。失败逐文件报告 applied、rolled_back、rollback_failed 或未尝试状态；父目录创建不回滚。

Recover(source_task_id) 检查已结束任务的原始、计划与当前版本。恢复须显式提供每个路径的当前版本，并作为新任务执行；再次变化会冲突，不自动覆盖外部编辑。执行器重启不会自动回滚或重放。恢复数据遵循私有状态的保留期限，不能替代长期备份。

## 命令与终端

Shell 每次启动新进程，cwd 不跨调用保留；wait_seconds 控制响应等待，timeout_seconds 控制进程期限。keep_stdin:true 提供管道输入，和终端模型分开。Windows 用户命令在进程树归属建立后才获准开始。

Poll 在有新字节或任务结束时返回；Tasks 等待最终任务状态。stdout_next_offset/stderr_next_offset 可接在初始预览之后，随后按每页 next_offset 续读；encoding:base64 无损读取原始字节，文本模式报告无效 UTF-8。

TerminalOpen 自动分配 terminal_id，Shell 的 cwd、环境及交互状态持续存在。TerminalInput 支持文本、控制键与 VT 序列，TerminalResize 更改实际 PTY 尺寸。TerminalRead 默认返回解析后的屏幕，since_screen 抑制重复画面；expected_screen 可保证分页版本一致。view:stream/both 返回 base64 字节流和字节游标，溢出有明确 gap。终端退出只依据 OS 进程状态，不猜提示符。PTY 输出为混合流，不能再拆成 stdout/stderr。

HTTP 断开不取消已接受的任务或终端。新调用可省略 operation_id；若要在丢失提交响应后安全重试，应在首次调用就指定 ID。没有重试 ID 的丢失提交必须先查 Tasks/Terminals，不能声称恰好执行一次。终端 input_id 对语义输入去重；written 表示写入 OS 通道，不表示应用已处理。重启后的终端标记 interrupted，输出明确过期，不会重建会话。

## 容量与权限边界

默认四个任务并发执行，活跃/排队共三十二项、每个 principal 八项。结束任务结果最多保留一百二十八项，结束任务日志合计 256 MiB、恢复数据合计 256 MiB、最长七天；活跃任务的输出/恢复空间另受四个 worker 和各项大小上限约束。每流最多存 16 MiB，每页最多 12 KB。到期保留重试身份，达到一万个身份时拒绝新增，而不悄悄允许旧 ID 重跑。

终端最多八个活跃会话、每个 principal 四个，闲置三十分钟关闭；每会话原始输出环 1 MiB，结束后缓存最多六十四个、最长一小时。输入队列三十二项，每次最多 8 KiB；队列满明确拒绝，部分投递不确定时不会建议盲重发。

文件工具执行工作目录和私有路径检查。exec 是当前 OS 用户的完整执行能力，cwd、任务归属和凭据身份不是系统沙箱。对不可信代码/用户使用 OS 用户、容器或 VM 隔离。审计只记录身份、操作和结果元数据；输出及恢复内容仍可能含用户数据。

## 开发和验收

```powershell
./scripts/prepare-windows-runtime.ps1 -Destination ./target/debug/deps
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

scripts/test-service-recovery.cjs 是开发用的真实 HTTP/进程故障测试客户端，不是服务端运行依赖。它需要新建的隔离目录，验证强制终止服务、PTY 后代清理、重启不重放和持久文件恢复。文件提交中途崩溃由 Rust 父测试启动独立子测试并强杀验证。

[架构说明](docs/architecture.md) · [验收记录](docs/refactor-v0.3.md)

Windows 是已验证目标。非 Windows 路径不保证具有相同的强制锁定或进程树清理语义。MIT；依赖与微软运行库保留各自许可证。
