# fs-agent — HTTP 文件服务

服务端目前支持 Linux（需要 `openat2`）；Web、Tauri 和 Node CLI 使用同一 HTTP 协议。服务启动时验证目录句柄能力，不支持时拒绝启动。

```bash
# 在本项目根目录执行（itookit monorepo 内即 tools/fs-agent，作为 submodule）
cargo build --release
# 复制 config.example.toml，设置实际目录、监听地址和允许的 Web/Tauri Origin。
export FS_SERVER_USER='workbench'
export FS_SERVER_PASSWORD='替换为至少8字节的密码'
target/release/fs-agent                     # 自动读取 config.toml
target/release/fs-agent /path/to/config.toml # 也可显式指定
```

配置只有一个用户和一串导出目录：

```toml
listen = "127.0.0.1:8787"
allowed_origins = ["http://localhost:3000", "http://localhost:1420"]

username = "li"                      # 省略时读 FS_SERVER_USER
password_env = "FS_SERVER_PASSWORD"  # 也可内联 password = "至少8字节"

[[exports]]
path = "/n/prj/x1"                   # 别名默认取目录名：x1

[[exports]]
path = "/srv/projects/demo"
access = "rw"                        # 默认 ro；rw 自动加独占锁
```

`allowed_origins` 是 CORS 白名单，只决定「哪个网页的 JS 能读取响应」，不代替认证：`http://localhost:3000` 对应 Web 开发（`apps/web-app` 的 Vite 端口），`http://localhost:1420` 对应 Tauri 开发；打包后的 Tauri 用 `tauri://localhost`（Linux/macOS）或 `http://tauri.localhost`（Windows），生产 Web 填实际 https 域名。写 `["*"]` 放通所有源（仅限本机/受控环境），写 `[]` 则拒绝所有浏览器源；两种写法都不影响 CLI 与 curl。

配置文件统一使用 TOML。不传参数时，服务依次在**进程当前工作目录**和**可执行文件所在目录**查找 `config.toml`，取第一个存在的文件；显式参数优先。找不到配置文件时启动失败并列出已搜索路径。

默认只读。需要写入时，在对应 `[[exports]]` 增加 `access = "rw"`（自动请求独占锁）。工作台添加挂载时再勾选允许写入；三层权限都允许才能修改。

`exclusive` 要求所有修改经本服务完成；目录上的 advisory lock 阻止合作的同根实例，不能阻止编辑器、Git 或其他外部程序。配置内拒绝重叠根与重复别名；不同实例的父子导出根仍需部署侧禁止。共享修改目录使用 `ro`。服务支持 HTTP；公网部署在 TLS 反向代理之后。Origin 白名单只解决浏览器访问，不代替身份认证。用户名/密码使用 UTF-8 HTTP Basic，密码至少 8 字节：用户名取 `username`，未写时读环境变量 `FS_SERVER_USER`；密码取内联 `password` 或 `password_env` 指向的环境变量。Basic 凭据不加密，非可信本机网络应使用 HTTPS。旧 Bearer 配置继续支持内联 `token` 或 `token_env`（至少 24 字节，且不设置 `username`），与密码字段四选一；同时设置 `username` 与 `token`/`token_env` 会启动失败，避免看起来是 Basic 实际只收 Bearer。内联 secret 写在服务端配置文件中，需按主机密钥文件管理权限，且不会输出到日志。

在 Settings → Storage → “远程文件系统”点击“添加远程文件系统”，填写名称、IP:端口（或完整 HTTP(S) 地址）、用户名和密码。工作台“+ 项目”选择本地或远程；远程项目选择连接名称和路径，例如 `/docs/project-a`。第一段为服务端导出别名，后续路径位于该导出之内；项目文件根直接对应此目录，Session 使用 `/workspace` 访问。同一服务的相同规范化路径复用同一个项目，不同路径可创建多个项目。全局不再提供“+ 会话”，在项目内部创建会话。

连接配置和项目引用会持久保存，密码仅保存在宿主运行期；重启后在设置编辑连接并重新输入密码。CLI 持久配置引用由 `MINDOS_REMOTE_<credentialRef中横线替换为下划线>` 环境变量解析。直接 `mindos fs` 使用 `FS_SERVER_USER` + `FS_SERVER_PASSWORD`；未设置用户名时兼容 `FS_SERVER_TOKEN`。

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
pnpm --filter @itookit/vfsdriver-agent test  # 在 itookit monorepo 内
```

驱动测试在 Linux 启动真实 Rust 服务，验证协议、条件保存和 VFS 适配；其他平台跳过该服务端集成测试。设计和验收边界见 [设计文档](../../doc/design/vfs-http-driver.md)。

包含远程来源的项目显示独立远程图标。服务断线时，仅关联项目的文件操作和新会话置灰禁用，已有会话仍可查看，其他项目继续可用；Settings 的重连入口保持可用。恢复连接后解除禁用。

## fs-agent 增量接口

`GET /v1/capabilities` 需要认证，返回安装身份及文件/同步/进程/终端支持情况。可在配置顶层指定稳定的 `server_id = "my-agent-node"`；未配置时为启用执行的服务生成本次启动的随机节点标识（不保证跨重启不变）。命令执行默认开启，配置顶层 `execution = false` 可切换为纯文件服务。启用执行时，启动会验证 Linux bubblewrap、fd 挂载及 user namespace 支持，失败即退出。`sync.push` 在同步服务开启且健康时为 true，详细能力通过 `/v1/sync/capabilities` 查询；`terminal.pty` 仍为 false；普通命令不依赖工作区租约模块。

`.gitignore` 由 MindOS 客户端文件树处理，服务端列表与文件访问不自动过滤。


### Remote commands

MindOS 项目右键菜单选择“启用远程命令”后，File Tools 和 Bash 共用远端 `/workspace`；本机 Shell 不作为回退。命令只挂载认证身份允许的 export 子目录，环境不继承服务端凭据。需要 `/usr/bin/bwrap` 支持 `--bind-fd`、`--ro-bind-fd`、`--disable-userns`。以非特权用户运行；尚不提供 cgroup 资源配额或多租户加固。

`POST /v1/processes` 启动（serverId、epoch、requestId、command、args、cwd、mounts、timeoutMs），`GET /v1/processes/:epoch/:id` 查询，`POST .../cancel` 取消。epoch 来自 capabilities.processEpoch。mount 使用 `{ alias, path, at, access }`，path 为 alias 内相对路径，绝不接受宿主路径。重复 requestId 不再次执行；重启后旧 epoch 被拒绝。取消返回 running 时仍需查询，直到进程确已回收。

每条命令最多一个 rw export（其他 export 为 ro）；该 writer 的独占锁保留给 monitor。单命令、最长 300 秒、stdout/stderr 各 64 KiB，超出输出上限会取消。返回最终有界输出，尚无实时流或 PTY。首版每次启动最多 1024 条进程记录，满后拒绝启动，需要管理员重启；重启不提供结果续接。

命令持有文件/进程互斥门，运行期间所有文件 API 返回 EBUSY；下载流、上传与后台提交保留其门直到结束。命令退出/取消并回收后使所有旧 revision 失效，避免 Bash 改动绕过条件写入。exclusive export 不允许其他宿主进程并发写入。执行锁在 monitor 中保留，daemon 意外退出后锁随进程清理释放。

验收：`FS_AGENT_PROCESS_TEST=1 cargo test`（要求可创建 Linux user/PID/network namespace）。

## 项目多端同步

同步存储、配置、管理员备份恢复与 HTTP 协议见 [单节点同步存储](doc/sync.md)。纯同步实例可使用 [config.sync.example.toml](config.sync.example.toml)，无需配置 export；sync.root 不存在或为空时首次启动自动初始化，也可先用 `fs-agent sync init CONFIG` 显式初始化。同步库与 export 使用独立目录，不自动发布工作目录的变化。

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

顶层 `log_level = "debug"` 为默认值，可设 `trace/debug/info/warn/error/off`。事件输出到 stderr，使用 JSON 行，包含时间、级别、事件名及结构化字段。

- debug：文件变更/命令准入；info：启动就绪、变更提交、命令启动与成功结束、取消和关闭。
- warn/error：认证拒绝、HTTP 错误、命令非零退出、超时、启动或清理失败；保留操作/请求 ID、状态和退出码。
- 普通读取、stat、目录列表、能力查询及状态轮询成功时始终静默，包括 trace/debug。读取错误输出 `http.failed`。
- 不输出 Authorization、口令、请求正文、命令正文或文件内容；命令输出仅记录字节数。进程结束记录是回收后的结果，HTTP 断线不冒充操作已取消。


## SQLite SeqFile

在 export 内通过 `POST /v1/fs/:alias/seq/snapshot` 读取 `{path}` 指定的 `.seq`，返回 `{revision,entries:[{key,value}]}`。`POST /v1/fs/:alias/seq/transaction` 使用 `X-Operation-Id`，请求为 `{path,expectedRevision,changes}`：变更项为 `{action:"set",key,value}` 或 `{action:"delete",key}`。创建时 revision 为 null；更新时必须携带读取的 revision。

每个 SeqFile 是可复制的 SQLite 数据库。服务端执行结构化单文件事务，再复用条件文件替换与持久回执；不接受任意 SQL。读取缺失文件不创建文件，父目录需提前创建。只读 export 禁止写入。每文件最多 16 MiB、每批最多 256 项、key 最多 1024 字节，HTTP 请求体仍有独立上限。并发冲突返回 ECONFLICT；unknown 结果使用现有操作查询确认，不自动重放。此接口与 sync 对象库无关，不提供跨 SeqFile 事务。
