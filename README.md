# Reach

项目仓库：[Teens-in-Times/reach-mcp](https://github.com/Teens-in-Times/reach-mcp)。

让通用 agent 操作本机的独立 Rust MCP 服务。文件、补丁、命令、进程输入与图片直接执行，不经过第二个 agent，也不依赖 Python、Node.js 或其他执行器。

一个二进制，两个可选工具呈现方式，一段文本钥匙，一个本机管理页。管理页和 MCP 的权限分开；工具调用是否经过人工批准，由使用它的 MCP 客户端决定。Reach 收到通过鉴权、权限与参数检查的调用后直接执行。

## 启动

Windows x86_64 / MSVC、Rust 1.99.0 验证。源码使用官方 Rust MCP SDK `rmcp`，依赖锁定在 Cargo.lock。

```powershell
cargo build --release --locked
# --directory 必须是一个尚不存在的私有目录
.\target\release\reach.exe init --directory D:\Reach\private --cwd G:\Project
.\target\release\reach.exe --config D:\Reach\private\config.json serve --background
.\target\release\reach.exe --config D:\Reach\private\config.json ui
```

`init` 创建受当前用户和 SYSTEM ACL 保护的配置、密钥哈希库及状态目录。默认 MCP 地址为 `http://127.0.0.1:42373/mcp`；本机管理页在独立的 `127.0.0.1:42374` 上。`init --no-admin` 禁用管理页，CLI 发钥匙仍可使用。服务只绑定回环地址。

`ui` 由本机用户生成 3 分钟、单次使用的登录票据并打开浏览器。票据只经 URL fragment 交给页面，页面立即移除 fragment；登录后使用 HttpOnly、SameSite=Strict 的 Cookie 和独立 CSRF 校验。管理会话最多 8 小时、闲置 30 分钟失效。管理端口不会随 MCP 的 SSH 转发开放。它不依赖网站账号、云服务或网页登录密码。

## 文本钥匙：复制一次即可

可在管理页添加客户端，或用 CLI：

```powershell
reach --config D:\Reach\private\config.json token issue `
  --principal coding-agent --label "代码助手" --profile compact `
  --scopes read,write,exec,tasks --root G:\Project --ttl-hours 2160 `
  --token-file D:\Reach\private\coding-agent.token
reach --config D:\Reach\private\config.json token list
reach --config D:\Reach\private\config.json token revoke <credential-id>
```

密钥形如 `reach_<id>.<secret>`，secret 来自操作系统的 32 字节密码学随机数。服务端只保存 SHA-256。支持标签、过期、轮换、撤销、权限和工作目录；每个请求都重新读取密钥库，无需重启生效。相同 principal 的钥匙共享任务归属，换 principal 即分开。密钥 ID 在 Authorization 文本内部，模型调用不需要再传客户端或会话 ID。

标准请求头足以连接，不要求 agent 实现 HMAC 请求签名：

```json
{
  "mcpServers": {
    "reach": {
      "type": "http",
      "url": "http://127.0.0.1:42373/mcp",
      "headers": {"Authorization": "Bearer reach_REPLACE_WITH_YOUR_KEY"}
    }
  }
}
```

这是常见客户端的配置示例；具体配置字段依客户端而异。协议层始终使用标准 Authorization header。密钥只给 MCP host，不进入模型的工具参数。不要将真实钥匙提交到 Git、放进提示词或截图。

本方案是私有连接上的共享秘密鉴权，不是 AES 加密，也不伪装成 OAuth。现有远程场景通过 SSH 加密、回环转发；若另行部署给公网、多用户使用，应配合合适的 TLS/OAuth 网关，而不是直接暴露明文 HTTP 端口。

## 精简模式与原生模式

默认 **compact**：只注册 `Run` 和 `Discover` 两个小定义。初始化 `instructions` 提供当前主机、默认目录、授权操作签名、重试与任务规则。详细 schema 留在服务端，需要时只查询一个：

```json
{"name":"Run","arguments":{"tool":"Read","arguments":{"paths":["src/main.rs"]}}}
{"name":"Discover","arguments":{"tool":"ApplyPatch"}}
```

**native**：公开被授权的独立工具，便于客户端做逐工具展示与审批。两种模式共用操作实现、权限和服务端参数校验；精简模式不会让任意参数绕过检查。

模型上下文如何组织、客户端是否使用初始化说明、每轮模型请求是否附带工具定义，以及模型厂商是否缓存前缀，都是 host 的行为，Reach 无法保证只注入一次。它能保证精简模式不再通过 tools/list 注册全套操作 schema；忽略初始化说明的 host 可通过 Discover 补查。相关约束见 [设计说明](docs/architecture.md)。

## 工具

| 操作 | 能力 |
|---|---|
| Read / Glob / Grep | 批量 UTF-8 与目录读取、文件和内容搜索、忽略规则、行号与分页 |
| Write / Edit | 原子单文件写入与精确替换，可用 SHA-256 拒绝过时编辑 |
| ApplyPatch | 熟悉的 Begin Patch 格式；多文件新增、更新、删除、移动；先验证全部文件、保留 CRLF |
| Shell | 独立命令任务、cwd、期限、异步返回及进程树生命周期 |
| Poll / Input / Cancel | 分页读取运行中的 stdout/stderr、写入管道 stdin、停止所属命令树 |
| Tasks | 查找自己的近期任务与丢失的结果，断连后恢复，不重跑命令 |
| Image | 原生 MCP 图片内容，PNG/JPEG/GIF/WebP，最多 8 MiB |
| Info | 主机、默认工作目录、身份、工具模式与有效权限 |

每次调用可覆盖绝对 cwd，默认为钥匙的首个授权目录；没有跨请求全局 chdir。新写入/命令自动产生 task_id，可选 operation_id 只用于明确的相同操作重试。多个 agent 可共用执行器；不同钥匙的任务列表、查询、重试、输入与取消都按 principal 检查。

`Shell.wait_seconds` 只控制本次等结果多久，`timeout_seconds` 控制进程期限。`keep_stdin=true` 开放 Input 管道；这不是 PTY，也不提供持续的交互 shell。Poll 的 offset 是字节位置，按返回的 next_offset 续读。任务结果与日志在 state_dir 持久保留；每个流最多存 16 MiB、内联页最多 12 KB。执行器重启后，未完成任务标为 interrupted，不自动重放。

文件工具限制在钥匙的工作目录内，并排除执行器的私有配置、状态、凭据等保护路径。**exec 是当前系统用户的完整命令执行权限**，不能靠 cwd 或工作目录将它变成系统沙箱。对不可信代码/用户需要 OS 用户、容器或虚拟机隔离。

Write/Edit 是单文件原子替换；ApplyPatch 会先验证所有文件并在普通提交错误时尝试回滚，但多文件修改不是崩溃原子事务，也不能阻止外部进程同时写文件。它不承诺终端、图片理解或原生 agent 私有 API 的完全等价实现。

## 私有远程连接

按 config.example.json 配置可选 ssh 项。原生 SSH 子进程监督回环反向转发，连接失败后重试，不更改 VPN、路由、防火墙或 DNS。只转发 MCP 端口。远端客户端使用目标机的 `127.0.0.1:<remote_port>/mcp`；本机客户端使用本机 MCP 地址。SSH 密钥和已知主机校验沿用现有 OpenSSH 配置，不另存 SSH 密码。

## 开发与验证

```powershell
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

测试包含协议级身份传递、只读绕过、工作目录与私有文件保护、任务归属、撤销/过期、管理 Cookie/CSRF/票据重放、补丁失败无提前写入、CRLF、运行中输出、stdin、期限、取消及重启恢复。管理页为嵌入式 HTML/CSS/JavaScript，无前端构建或运行时依赖。

目前验证目标为 Windows。非 Windows 构建路径尚未验证；不要据此假定拥有与 Windows Job Objects 等价的子进程树隔离。审计记录仅含身份、操作与结果元数据；任务输出可能含用户数据，私有状态目录应持续受权限保护。

本仓库是用户的独立 Reach 项目，未发布到公开包注册表，与其他同名工具无关联。MIT；依赖保留各自许可证。
