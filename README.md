# itookit-vfs-server — HTTP 文件服务

服务端目前支持 Linux（需要 `openat2`）；Web、Tauri 和 Node CLI 使用同一 HTTP 协议。服务启动时验证目录句柄能力，不支持时拒绝启动。

```bash
# 在本项目根目录执行（itookit monorepo 内即 tools/itookit-fs-server，作为 submodule）
cargo build --release
# 复制 config.example.toml，设置实际目录、监听地址和允许的 Web/Tauri Origin。
export FS_SERVER_USER='workbench'
export FS_SERVER_PASSWORD='替换为至少8字节的密码'
target/release/itookit-vfs-server                     # 自动读取 config.toml
target/release/itookit-vfs-server /path/to/config.toml # 也可显式指定
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

`exclusive` 要求所有修改经本服务完成；目录上的 advisory lock 阻止合作的同根实例，不能阻止编辑器、Git 或其他外部程序。配置内拒绝重叠根与重复别名；不同实例的父子导出根仍需部署侧禁止。共享修改目录使用 `ro`。服务支持 HTTP；公网部署在 TLS 反向代理之后。Origin 白名单只解决浏览器访问，不代替身份认证。用户名/密码使用 UTF-8 HTTP Basic，密码至少 8 字节：用户名取 `username`，未写时读环境变量 `FS_SERVER_USER`；密码取内联 `password` 或 `password_env` 指向的环境变量。Basic 凭据不加密，非可信本机网络应使用 HTTPS。旧 Bearer 配置继续支持内联 `token` 或 `token_env`（至少 24 字节，且不设置 `username`），与密码字段四选一。内联 secret 写在服务端配置文件中，需按主机密钥文件管理权限，且不会输出到日志。

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

当前边界：上传最多 256 MiB；驱动完整读取默认 32 MiB；stat 每批 256 项；目录每页 512 项、单目录扫描上限 10 万项；全局最多 16 个文件工作槽。目录分页每页重新扫描，不是快照。非 UTF-8/特殊文件返回诊断；VFS 旧列表适配器遇到诊断会报不支持。禁止链接遍历和 `.itookit-upload-` 保留名称；独占启动时扫描并清理遗留上传文件，最多遍历 100 万项。`remove` 当前仅支持文件和空目录；递归删除、append/patch、订阅和搜索端点未开放。外挂项目的 Agent 不装配本地 Shell/TTY，防止进程工具读到同名宿主目录。

```bash
cargo test                                  # 本项目根目录
pnpm --filter @itookit/vfsdriver-http test  # 在 itookit monorepo 内
```

驱动测试在 Linux 启动真实 Rust 服务，验证协议、条件保存和 VFS 适配；其他平台跳过该服务端集成测试。设计和验收边界见 [设计文档](../../doc/design/vfs-http-driver.md)。

包含远程来源的项目显示独立远程图标。服务断线时，仅关联项目的整个抽屉及内部项置灰禁用，其他项目继续可用；Settings 的重连入口保持可用。恢复连接后解除禁用。
