# pi-agent — MindOS 的个人智能节点

pi-agent 将设备上的文件、项目和执行环境接入 MindOS，统一提供同步、受控访问及 AI harness 会话管理。MindOS 组织知识、对话和工作流，pi-agent 连接设备上的数据与执行能力。PI 表示 Personal Information（个人信息）。

服务源码：[pi-agent](https://github.com/mushuanli/pi-agent)；统一客户端：[piagent-driver](https://github.com/mushuanli/piagent-driver)，npm 包为 `@itookit/piagent-driver`。

服务端目前支持 Linux（需要 `openat2`）；Web、Tauri 和 Node CLI 使用同一 HTTP 协议。服务启动时验证目录句柄能力，不支持时拒绝启动。

```bash
# 在本项目根目录执行（itookit monorepo 内即 tools/pi-agent，作为 submodule）
cargo build --release
# 复制 config.example.toml，设置实际目录、监听地址和允许的 Web/Tauri Origin。
export PI_AGENT_API_KEY='替换为至少24字节的API-Key'
target/release/pi-agent                     # 自动读取 config.toml
target/release/pi-agent /path/to/config.toml # 也可显式指定
```

文件模式配置一组认证凭据和一串导出目录；可额外启用外部 harness 控制：

```toml
listen = "127.0.0.1:8787"
allowed_origins = ["http://localhost:3000", "http://localhost:1420"]

api_key_env = "PI_AGENT_API_KEY"      # 也可内联 api_key = "至少24字节"

[[exports]]
path = "/n/prj/x1"                   # 别名默认取目录名：x1

[[exports]]
path = "/srv/projects/demo"
access = "rw"                        # 默认 ro；rw 自动加独占锁
```

`allowed_origins` 是 CORS 白名单，只决定「哪个网页的 JS 能读取响应」，不代替认证：`http://localhost:3000` 对应 Web 开发（`apps/web-app` 的 Vite 端口），`http://localhost:1420` 对应 Tauri 开发；打包后的 Tauri 用 `tauri://localhost`（Linux/macOS）或 `http://tauri.localhost`（Windows），生产 Web 填实际 https 域名。写 `["*"]` 放通所有源（仅限本机/受控环境），写 `[]` 则拒绝所有浏览器源；两种写法都不影响 CLI 与 curl。

配置文件统一使用 TOML。不传参数时，服务依次在**进程当前工作目录**和**可执行文件所在目录**查找 `config.toml`，取第一个存在的文件；显式参数优先。找不到配置文件时启动失败并列出已搜索路径。

默认只读。需要写入时，在对应 `[[exports]]` 增加 `access = "rw"`（自动请求独占锁）。工作台添加挂载时再勾选允许写入；三层权限都允许才能修改。

`exclusive` 要求所有修改经本服务完成；目录上的 advisory lock 阻止合作的同根实例，不能阻止编辑器、Git 或其他外部程序。配置内拒绝重叠根与重复别名；不同实例的父子导出根仍需部署侧禁止。共享修改目录使用 `ro`。服务支持 HTTP；公网部署在 TLS 反向代理之后。Origin 白名单只解决浏览器访问，不代替身份认证。用户名/密码使用 UTF-8 HTTP Basic，密码至少 8 字节：用户名取 `username`，未写时读环境变量 `FS_SERVER_USER`；密码取内联 `password` 或 `password_env` 指向的环境变量。Basic 凭据不加密，非可信本机网络应使用 HTTPS。推荐 `api_key` 或 `api_key_env`（至少 24 字节、不设置 `username`），使用 Bearer 认证；旧名称 `token`/`token_env` 仍兼容，但不能和对应新名称同时配置。API Key 与密码认证互斥；同时设置 `username` 与 `token`/`token_env` 会启动失败，避免看起来是 Basic 实际只收 Bearer。内联 secret 写在服务端配置文件中。API Key 按启动连接提示输出到 console，不放入结构化事件或 HTTP 响应；旧 Basic 密码不输出。

在“工具箱 → MCP”新增 HTTP 配置，填写完整 MCP 地址（例如 `http://127.0.0.1:8787/mcp`），将服务启动时输出的 API Key 填入同名字段并测试连接。通用发现响应已携带 pi-agent 能力描述；应用扩展验证后启用项目绑定，不需要额外用户名、密码或勾选框。旧服务器使用其已声明的能力工具。测试后的保存和改名复用验证结果，普通 MCP 不发 pi-agent 专用探测。只有验证成功的 pi-agent 配置可以用于目录绑定，普通 MCP 即使同名也不能绑定。可配置多条连接；工作台新建远程项目选择 MCP 配置和 `/alias/path`。项目展示“服务器名:项目名”，同一服务器、账号与规范化路径复用已有项目，改名不移动目录。

连接配置、API Key 和项目引用通过现有 MCP 配置持久保存；应用重启时从已验证的 MCP 配置恢复驱动凭据，无需重新输入。旧 Basic 配置的密码仍只保存在宿主运行期。CLI 持久配置引用由 `MINDOS_REMOTE_<credentialRef中横线替换为下划线>` 环境变量解析。直接 `mindos fs` 使用 `FS_SERVER_USER` + `FS_SERVER_PASSWORD`；未设置用户名时兼容 `FS_SERVER_TOKEN`。

```bash
# 使用已构建的 CLI；也可以通过现有 CLI 开发入口运行。
node apps/cli/dist/cli.js fs list http://127.0.0.1:8787 docs
node apps/cli/dist/cli.js fs read http://127.0.0.1:8787 docs guide.md --out /tmp/guide.md
node apps/cli/dist/cli.js fs stat http://127.0.0.1:8787 docs guide.md
node apps/cli/dist/cli.js fs status http://127.0.0.1:8787 docs <operationId>
```

API：`GET /v1/exports`、`POST /v1/fs/:alias/stat`（`{paths}`）、`GET entries?path=`、`GET/PUT content?path=`、`POST mutate`（`{action: mkdir|rename|remove, path, to?}`）、`GET operations/:id`、`POST operations/:id/cancel`。路径均为别名内相对路径。PUT 必须有 `If-None-Match: *` 或强 `If-Match`，修改必须有 `X-Operation-Id`。重复 ID 返回 409；查询原 ID 获取结果，不重发内容。状态保留一小时，重启后未知。

每个请求独立完成，无 begin/end/commit 命令；分页以 `nextCursor: null` 结束。取消读取通过断开 fetch/响应流；修改通过取消端点和 deadline。提交后的取消不回滚。`committed` 表示原子命名空间替换可见，不承诺断电持久性或跨文件事务。

当前边界：上传最多 256 MiB；驱动完整读取默认 32 MiB；stat 每批 256 项；目录每页 512 项、单目录扫描上限 10 万项；全局最多 16 个文件工作槽。目录分页每页重新扫描，不是快照。非 UTF-8/特殊文件返回诊断；VFS 旧列表适配器遇到诊断会报不支持。禁止链接遍历和 `.itookit-upload-` 保留名称；独占启动时扫描并清理遗留上传文件，最多遍历 100 万项，无法读取的子目录按尽力而为跳过。路径必须是导出内相对路径：允许名称中的冒号（如 `2024:Q1.md`），拒绝盘符前缀（如 `C:/host`）。新建文件权限 0644、新建目录 0755；条件替换保留被替换文件的权限位。`remove` 当前仅支持文件和空目录；递归删除、append/patch、订阅和搜索端点未开放。外挂项目的 Agent 不装配本地 Shell/TTY，防止进程工具读到同名宿主目录。

```bash
cargo test                                  # 本项目根目录
pnpm --filter @itookit/piagent-driver test  # 在 itookit monorepo 内
```

驱动测试在 Linux 启动真实 Rust 服务，验证协议、条件保存和 VFS 适配；其他平台跳过该服务端集成测试。设计和验收边界见 [设计文档](../../doc/design/vfs-http-driver.md)。

包含远程来源的项目显示独立远程图标。服务断线时，仅关联项目的文件操作和新会话置灰禁用，已有会话仍可查看，其他项目继续可用；Settings 的重连入口保持可用。恢复连接后解除禁用。

## pi-agent 增量接口

`GET /v1/capabilities` 需要认证，返回安装身份及文件/同步/进程/终端支持情况。可在配置顶层指定稳定的 `server_id = "my-agent-node"`；未配置时为启用执行的服务生成本次启动的随机节点标识（不保证跨重启不变）。命令执行默认开启，配置顶层 `execution = false` 可切换为纯文件服务。启用执行时，启动会验证 Linux bubblewrap、fd 挂载及 user namespace 支持，失败即退出。`sync.push` 在同步服务开启且健康时为 true，详细能力通过 `/v1/sync/capabilities` 查询；`terminal.pty` 仍为 false；普通命令不依赖工作区租约模块。

`.gitignore` 由 MindOS 客户端文件树处理，服务端列表与文件访问不自动过滤。


### Remote commands

MindOS 项目右键菜单选择“启用远程命令”后，File Tools 和 Bash 共用远端 `/workspace`；本机 Shell 不作为回退。命令只挂载认证身份允许的 export 子目录，环境不继承服务端凭据。需要 `/usr/bin/bwrap` 支持 `--bind-fd`、`--ro-bind-fd`、`--disable-userns`。以非特权用户运行；尚不提供 cgroup 资源配额或多租户加固。

`POST /v1/processes` 启动（serverId、epoch、requestId、command、args、cwd、mounts、timeoutMs），`GET /v1/processes/:epoch/:id` 查询，`POST .../cancel` 取消。epoch 来自 capabilities.processEpoch。mount 使用 `{ alias, path, at, access }`，path 为 alias 内相对路径，绝不接受宿主路径。重复 requestId 不再次执行；重启后旧 epoch 被拒绝。取消返回 running 时仍需查询，直到进程确已回收。

每条命令最多一个 rw export（其他 export 为 ro）；该 writer 的独占锁保留给 monitor。单命令、最长 300 秒、stdout/stderr 各 64 KiB，超出输出上限会取消。返回最终有界输出，尚无实时流或 PTY。首版每次启动最多 1024 条进程记录，满后拒绝启动，需要管理员重启；重启不提供结果续接。

命令持有文件/进程互斥门，运行期间所有文件 API 返回 EBUSY；下载流、上传与后台提交保留其门直到结束。命令退出/取消并回收后使所有旧 revision 失效，避免 Bash 改动绕过条件写入。exclusive export 不允许其他宿主进程并发写入。执行锁在 monitor 中保留，daemon 意外退出后锁随进程清理释放。

验收：`PI_AGENT_PROCESS_TEST=1 cargo test`（要求可创建 Linux user/PID/network namespace）。

## 项目多端同步

同步存储、配置、管理员备份恢复与 HTTP 协议见 [单节点同步存储](doc/sync.md)。纯同步实例可使用 [config.sync.example.toml](config.sync.example.toml)，无需配置 export；sync.root 不存在或为空时首次启动自动初始化，也可先用 `pi-agent sync init CONFIG` 显式初始化。同步库与 export 使用独立目录，不自动发布工作目录的变化。

## 代码结构

依赖只向下：`http` 依赖领域模块，领域模块依赖 `core`；策略与机制分开放在不同文件里。

| 目录 | 职责 |
| --- | --- |
| `core/` | 与传输无关的基础件：错误词表与 errno 映射、标识符校验、文件/进程互斥门、阻塞工作池、分页游标 |
| `config/` | 配置模型、凭据策略、导出策略、配置文件查找（纯输入与校验，不持有运行时状态） |
| `app/` | 组合根：把配置装配成运行时状态（认证、导出、门、工作池、操作账本、执行准入） |
| `auth.rs` | 身份解析与别名读写授权 |
| `fs/` | 路径策略、目录能力（openat2）、revision、结构化修改、原子上传、启动恢复 |
| `operations/` | 幂等写入收据：ID 复用、保留期与容量、取消语义、等待与超时 |
| `sync/` | SQLite 持久元数据、不可变对象、条件发布、历史恢复、读取保护、GC 与停机灾备 |
| `process/` | 准入 `execution`、请求策略 `policy`、bubblewrap 机制 `sandbox`、监管 `runner`、API `service` |
| `workspace/` | 工作区租约：策略 `lease` 与原子日志机制 `journal`（独立可选，未接入路由） |
| `http/` | 路由与 CORS、请求边界策略 `access`、`range` 解析、各端点 handler |

策略与机制的分界举例：`fs::path` 决定「什么路径合法」，`fs::export` 决定「如何用 `openat2` 打开」；`process::policy` 决定「哪些挂载被授权」，`process::sandbox` 决定「如何拼装 bwrap 参数」；`workspace::lease` 管租约状态机，`workspace::journal` 管 `write → fsync → rename → fsync` 的持久化与故障分级；`operations` 管收据语义，handler 只负责解析与转发。

进程退出分两级：先停止接收命令、请在工作中的操作停止并等待文件门排空（5 秒），再给已打开的连接 5 秒自行结束，避免慢速下载无限拖住 Ctrl-C。

测试与模块对应：`tests/config.rs`、`tests/fs.rs`、`tests/http/`（按端点分组）、`tests/process.rs`、`tests/workspace.rs`、`tests/sync/`，共享脚手架在 `tests/common/`。

## 日志

顶层 `log_level = "info"` 为默认值，可设 `trace/debug/info/warn/error/off`。事件输出到 stderr，使用 JSON 行；每行首先输出带毫秒和本机时区偏移的可读 `time`，再输出 level、event、fields，末尾保留 Unix 毫秒 `timeMs` 供程序处理。例如 `{"time":"2026-10-08T13:44:42.034+08:00","level":"info","event":"server.ready","fields":{"address":"127.0.0.1:8787"},"timeMs":1791438282034}`。时区使用服务进程的本地设置，可通过 `TZ` 调整。sync.diagnostics 是每分钟的调试性能汇总，仅在 debug/trace 下输出；普通运行保持 info 即可。

- debug：文件变更/命令准入；info：启动就绪、变更提交、命令启动与成功结束、取消和关闭。
- warn/error：认证拒绝、HTTP 错误、命令非零退出、超时、启动或清理失败；保留操作/请求 ID、状态和退出码。
- 普通读取、stat、目录列表、能力查询及状态轮询成功时始终静默，包括 trace/debug。读取错误输出 `http.failed`。
- 不输出 Authorization、口令、请求正文、命令正文或文件内容；命令输出仅记录字节数。进程结束记录是回收后的结果，HTTP 断线不冒充操作已取消。


## SQLite SeqFile

在 export 内通过 `POST /v1/fs/:alias/seq/snapshot` 读取 `{path}` 指定的 `.seq`，返回 `{revision,entries:[{key,value}]}`。`POST /v1/fs/:alias/seq/transaction` 使用 `X-Operation-Id`，请求为 `{path,expectedRevision,changes}`：变更项为 `{action:"set",key,value}` 或 `{action:"delete",key}`。创建时 revision 为 null；更新时必须携带读取的 revision。

每个 SeqFile 是可复制的 SQLite 数据库。服务端执行结构化单文件事务，再复用条件文件替换与持久回执；不接受任意 SQL。读取缺失文件不创建文件，父目录需提前创建。只读 export 禁止写入。每文件最多 16 MiB、每批最多 256 项、key 最多 1024 字节，HTTP 请求体仍有独立上限。并发冲突返回 ECONFLICT；unknown 结果使用现有操作查询确认，不自动重放。此接口与 sync 对象库无关，不提供跨 SeqFile 事务。

## 原生 harness 控制中心（MCP 2.0）

### Harness 插件扩展

服务使用编译时注册的 `HarnessPlugin` / `HarnessPlugins` 插件接口。默认注册 Codex 和 Claude Code；`kind` 由注册表识别，未注册插件在启动验证时拒绝。插件负责原生协议、会话存储读取、进程初始化及统一会话/事件/交互格式，公共服务负责 epoch/requestId、回执、串行修改、项目授权、实例缓存和关闭。项目 launcher 接收插件指定的启动参数，不再固定 Codex 参数。

新增实现可通过 `HarnessPlugins::register` 注册，使用 `State::from_config_with_plugins` 装配；标准二进制的内置插件在注册表默认实现中声明。普通实例与项目实例都通过同一个插件工厂创建，项目实例接收 `ProjectRuntime` 并使用共同目录授权及 launcher。插件是可信的进程内代码，当前不支持动态库加载或外部插件自动发现。Claude Code 已有独立 SDK 适配；DeepSeek CLI 尚未实现，配置一个 kind 名字不会创建对应驱动。

启动时（日志级别 info 或更详细）输出可复制的 MCP endpoint、serverId、认证方式、凭据来源及实际 API Key；旧 Basic 配置只显示用户名和密码来源。监听通配地址时另外尝试显示默认路由的本机 IP 候选；多网卡、容器、NAT 或代理场景仍需填写客户端实际可达地址。该探测不发送 UDP 数据包，也不发现公网地址。

认证使用每请求的 HTTP Basic 或 Bearer，没有 cookie 登录会话和自动到期时间；配置凭据不变时，服务重启后仍有效。itookit 的 pi-agent 项目绑定支持标准 MCP API Key（Bearer）和旧 Basic 配置；API Key 使用 MCP 既有存储，驱动运行时只保留内存副本。API Key 不写入项目挂载记录，MCP 导出仍移除 API Key。

使用 [config.harness.example.toml](config.harness.example.toml) 配置。`harnesses` 缺省为空，不启动或访问 Codex；每个 profile 显式设置已存在的绝对 CODEX_HOME 和授权 workspace。运行 pi-agent 的系统用户需已有 Codex 登录状态及已安装的 CLI。要浏览当前用户 ~/.codex 的会话，填写该目录的实际绝对路径；TOML 不展开 `~`。不读取未配置的其他 home，不把 auth.json 等密钥文件作为会话返回。

`execution = false` 关闭原有 shell 服务，但仍可启用 harness；无需 exports 即可仅运行控制中心。workspace 必须与 exclusive rw exports 分离（启动时拒绝重叠），可用 ro export 浏览相同目录。`server_id` 可设置稳定服务身份。

Web/Tauri 在工具箱 MCP 配置并测试连接后，在支持 harness 的 pi-agent 配置中点击“控制中心”。支持 profile/workspace、新建、分页/归档列表、历史、继续、发送、增量输出、命令/文件审批、用户问题、显式中断和未知请求收据确认。历史浏览不会接管会话；继续仅允许授权 cwd、拒绝接管其他宿主的活跃线程。关闭面板只释放客户端连接，服务端 turn 继续运行。CLI 宿主已统一 adapter 接入，独立 harness 命令行界面尚未提供。

服务提供认证后的 `POST /mcp`：MCP SDK 2.0.0 / 协议 2026-07-28，支持 server/discover、tools/list/call、ping；不宣告 Tasks 扩展。工具为 piagent_capabilities、harness_profiles、harness_sessions、harness_session_read、harness_events、harness_operation、harness_create、harness_resume、harness_turn、harness_interrupt、harness_respond。请求验证 protocol/version、method/name 和客户端 metadata，带 Origin 的请求还验证允许列表。Codex 控制采用官方 app-server stdio JSON-RPC，命令和目录只来自服务端配置。

已验证 Codex CLI 0.159.2。项目沙箱在配置的真实绝对路径挂载 harness home，并保留 `/harness` 兼容入口，避免原生 SQLite 索引中的绝对日志路径失效。`harness_session_info` 只读取元信息；普通会话历史优先原生 API，paginated 会话及 `toolDetail: "summary"` 请求读取 home 下 sessions/archived_sessions JSONL。历史使用 8 MiB 的日志窗口，必要时通过有界缓冲向前查找真实 turn_context/task_started，返回最多 100 项/2 MiB，通过 `nextCursor` 加载更早内容。保留真实 turnId、时间和消息内容数组，兼容 custom_tool_call；摘要模式在分页预算前去除工具参数和输出，只返回工具名称、操作、首个非空命令行（最多 240 个字符）和目标文件。事件也支持摘要模式并保留原始游标及审批请求。原生 createdAt/updatedAt 分别统一转换为毫秒，缺失时间返回 null；空名称回退原生请求预览，branchName 独立返回；session.native 不重复附带整份 turns。路径禁止符号链接，未完成尾行等待刷新；不修改原生索引或日志。继续会话使用 `thread/resume` 的 `excludeTurns`，由原生 CLI 确认可用性。

`harness_fork` 调用原生 `thread/fork`，可选名称经 `thread/name/set` 保存，返回 `parentSessionId`。仅授权项目的可写会话允许创建分支；空会话、活动会话和未知结果期间 UI 禁用创建。分支创建同样携带 epoch/requestId，并以回执避免重放。分支是独立的原生会话，历史保留在 harness home。

限制：每 profile 1024 条进程内修改收据、128 个 pending 调用/交互、事件环最多 1024 项/8 MiB、待审批总量最多 8 MiB、每轮提示最多 128 KiB、原生单行最多 8 MiB；客户端 MCP 响应最多 32 MiB。输出 gap 显示需要刷新历史。发送结果未知不重放；epoch 改变表示服务已重启，旧收据不再可确认。app-server 退出后本 profile 拒绝继续写入；需服务重启重新发现，避免悄悄失去接管关系。服务关闭回收监管进程组；主动脱离该组的后代不属于此回收保证。

验证（在 itookit 根目录）：

```bash
cargo test --offline --manifest-path tools/pi-agent/Cargo.toml
PI_AGENT_CODEX_TEST=1 cargo test --offline --manifest-path tools/pi-agent/Cargo.toml --test harness
PI_AGENT_HARNESS_TEST=1 pnpm --filter @itookit/piagent-driver test tests/network.test.ts
```

第一项包含原生协议模拟测试；第二项用独立临时 CODEX_HOME 启动真实 Codex，只验证创建/历史，不调用模型，也不读取个人会话；第三项用真实 MCP SDK、Rust HTTP 服务和模拟 Codex 验证完整操作。

server/discover 的 `_meta['itookit/pi-agent']` 返回文件协议版本、同源相对 HTTP 端点、安装 serverId、项目和 harness 支持；piagent_capabilities 返回同一描述并保留旧客户端兼容。客户端优先复用标准发现响应，不根据名字判断服务；该描述是本服务扩展，不是 MCP 标准保证。建议显式配置全局唯一且稳定的 server_id，以识别同一安装的不同访问地址。多个原生 harness 由服务端插件注册表适配，共用这一 MCP 连接；当前内置 Codex 与 Claude Code。


## 目录项目模式

[config.projects.example.toml](config.projects.example.toml) 开启服务端项目 catalog。导出根是项目选择范围，`project_register` 可以绑定其任意层级子目录，或在已有父目录下创建一个新目录。文件、bash 和项目 Codex 使用同一份目录及挂载授权，sync 数据项目独立。稳定 server_id 必须配置，catalog 和 native home 不可放入导出根。

MCP 提供 project_roots/list/read/register/configure/exec；harness 工具增加 projectId/revision/readOnly 上下文。新 harness profile 设置 projects=true，在外层 bubblewrap 内运行；命令应指向可独立运行的 native binary。projects.network 默认为 false；联网需显式开启。目标挂载目录先创建，再提交策略。无法启用沙箱时拒绝执行。

当前 FileGate 是安装级，turn 执行期间文件请求可能返回 EBUSY。VMM 和项目级并发尚未实现，已留共同 ProjectLauncher 端口。详细授权、兼容行为与验证范围见 [项目模型](../../doc/design/pi-agent-project-model.md)。

同时启用 `[projects]` 与 `[sync]` 后，目录绑定支持数据集落地、目录回传和双向同步。
使用 `project_sync_directories` 浏览项目内的已有子目录；绑定和预览不写入文件。
`project_sync_configure` 根据 policyRevision 改方向并使旧预览失效。
`project_sync_preview` 返回新增／覆盖操作、方向和包含两侧摘要的 conflictDetails。
`project_sync_compare` 对当前计划提供最多 256 KiB 的 UTF-8 文本及基线；二进制和大文件只返回说明。
`project_sync_resolve` 选择 dataset／directory 来源并生成新的 planId，复核后使用 execute 确认。
回传复用 sync 的 CAS 发布与持久回执，关闭面板或未知结果时通过 status 继续同一计划。
默认更新模式保留目标独有文件，不传播删除、不穿透附加挂载或 .mindos；镜像删除未开放。

服务曾名为 fs-agent。升级时继续使用原 config.toml、server_id、API Key、projects.root、sync.root 和 harness home；数据目录无需改名。新程序为 pi-agent，MCP 主发现工具为 piagent_capabilities，旧 fsagent_capabilities 保留为兼容别名。文件／项目协议标识、HTTP 授权头和 fs-agent.files／fs-agent.bundle 的规范编码沿用原格式，已有摘要、历史和绑定可以继续使用。


### 项目与原生会话搜索

`project_search` 接收 projectId/revision/query/mode（path/content）。发现描述的 fileSearch 仅在隔离执行和 `/usr/bin/rg` 可用时声明；执行使用固定 argv、字面/忽略大小写（支持换行）匹配、只读 pinned 项目视图，无网络和 symlink 跟随，保留嵌套 mount 遮蔽并排除 gitignore、隐藏、私有及原生 home。上限为 100 条、2 MiB 输出/单文件、10 秒、4 并发。无匹配与进程/隔离错误分别返回；取消回收子进程。

Codex 声明 capabilities.search，`harness_session_search` 接收 profileId、可选 projectId/revision、query、mode（title/content）、archived。经授权列表和原生历史解析读取用户/assistant 文本与命令摘要，不递归扫描 home。最多 20 页列表、每会话 64 页历史、16 MiB、100 条、15 秒、2 并发。结果携带原生 session/turn/item 身份；超限 truncated=true，当前无续页。状态保留 statusDetails.activeFlags 和最近 turn 结果；notLoaded/未知枚举不推断已经完成，独立 CLI 的可读历史不代表本服务有控制权。


### 原生管理与内联附件

Codex 提供 `harness_rename`、`harness_archive`、`harness_unarchive`、`harness_delete`，均要求可写授权并使用 epoch/requestId 回执去重。rename 调用 thread/name/set，不 resume；archive 允许本实例持有且原生状态明确 idle，或原生 notLoaded 历史；后者不表示外部进程已空闲，归档不会停止独立 CLI。归档前核验派生子会话的授权与状态，归档后释放所有权；unarchive 核验恢复的原生 cwd，不 resume、不启动 turn。delete 调用原生 thread/delete，要求受控 idle 或已归档 notLoaded，前置有界核验全部派生子会话的授权和状态；回执返回 deletedSessionIds，原生负责日志及元数据删除，项目文件保留。

`harness_turn.attachments` 接受最多 5 个内联 text/image：UTF-8 文本每个 64 KiB，PNG/JPEG/WebP 图片解码每个 256 KiB，编码内容总计 512 KiB。拒绝非法名称、二进制文本、宿主路径、远程 URL、其他图片格式和超限内容，在启动 turn 前完成验证。MCP JSON body 上限为 2 MiB，以容纳合法附件的 JSON 转义；其他 JSON 路由仍为 512 KiB。


### Claude Code

显式配置已安装的 Claude CLI 和已存在的私有目录；`home` 对应 `CLAUDE_CONFIG_DIR`，不会自动读取未配置的 `~/.claude`。项目实例复用 ProjectLauncher 和 Bubblewrap 的目录、mounts、网络授权：

```toml
[[harnesses]]
id = "claude"
kind = "claude"
command = "/usr/local/bin/claude"
home = "/srv/pi-agent/claude-home"
projects = true
```

真实验收版本为 Claude Code 2.1.209。控制使用 [Agent SDK 流式输入](https://code.claude.com/docs/en/agent-sdk/streaming-vs-single-mode) 的 stream-json、initialize/control_request/control_response；每个受控会话独立子进程，最多 16 个。新建、恢复、文本/图片附件、中断、工具审批及 AskUserQuestion 转换为统一 harness 接口。使用 manual 权限模式和 stdio 审批通道；不跳过审批。turn 只有收到相同原生 user UUID 的 replay 确认才提交回执；超时或断线结果未知，不自动重发。客户端关闭只释放观察，不中断远端 turn；服务端关闭回收进程组和审批请求。

历史只读解析私有 home 的 `projects/*/*.jsonl`，按实际 cwd 核验项目归属，拒绝链接和越界，不修改原生索引。目录列表检查最多 2048 个目录名称、8192 个文件、16 MiB 元信息及 5 秒；历史每次读取 8 MiB 窗口，返回最多 100 项/2 MiB。真实用户 UUID 定义轮次，多条 assistant 消息保留该身份；工具只返回摘要。恢复使用[原生日志绝对路径](https://code.claude.com/docs/en/sessions)，兼容宿主 cwd 与项目沙箱虚拟 cwd 的差异；恢复前核验整份日志的身份及 cwd，日志上限 16 MiB，超限明确失败；历史读取与恢复验证共用 4 个 blocking 工作槽。搜索共用 100 条/16 MiB/15 秒预算及每会话 64 页上限。

独立 CLI 历史可发现，状态为 notLoaded、owned=false；读取不获得控制权，显式 resume 才建立本服务的独立受控进程，不能接管外部正在运行的进程。Claude 当前未声明 fork/rename/archive/unarchive。新建空会话由原生子进程持有，首条消息写入前不保证重启后可发现；原生 JSONL 落盘可能稍晚于 result 事件，历史刷新会获取随后写入的记录。

```bash
cargo test --test claude --test project_watch
cargo build
python3 tests/real_claude.py
```

最后一项为安装真实 CLI 后可选的验收：使用临时 home/项目、本地 Anthropic 协议 peer，验证原生审批写文件、图片、独立 CLI 发现及恢复；不访问个人数据或云模型，不代替桌面视觉验收。

### 项目目录变化

发现元数据的 `fileWatch: true` 声明 `project_watch` / `project_unwatch`。请求携带 projectId/revision，可重用返回的 watchId；响应仅为 watchId/version/gap/truncated，不含宿主路径、文件名或正文。Linux inotify 监听 pinned 项目根与 mounts，不跟随链接，不扫描隐藏目录、原生 home 或被挂载遮蔽的目录。外部编辑器、独立 CLI、新目录中的变化均推进版本；目录变动或队列溢出重建监听，溢出报告 gap。

最多 16 个观察，空闲 60 秒后在后续请求清理；客户端关闭发送 unwatch，授权变更/项目移除释放相应观察。建立监听最多 2048 个目录、50000 项及 2 秒，覆盖不足返回 truncated；driver 每 30 秒推进一次部分覆盖版本以补充刷新。旧服务器没有 fileWatch 时不调用新工具。观察按 owner/project/revision 及根目录身份核验，授权失效立即停止读取；没有原生会话的项目仍观察目录。目录变更不表示外部会话正在运行，也不授予执行控制。
