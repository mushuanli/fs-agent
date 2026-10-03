# 单节点同步存储

同步库使用独立的 `sync.root`，与 export、工作目录和 Bash 执行数据分开。它保存不可变 SHA-256 对象、数据集版本和持久操作回执，支持同一账号的多端设备交替发布。客户端负责文件扫描、共同基线、合并、覆盖和会话接续；本服务负责存储、条件发布、发现和恢复。

## 部署和管理员命令

使用 [纯同步配置](../config.sync.example.toml)，设置凭据和新的空存储目录。`execution=false` 时无需 bubblewrap，也可以不配置 exports。所有同步 HTTP 请求复用现有认证；副本 ID 不能替代认证凭据。

```bash
cargo build --release
# 首次初始化；普通服务启动从不自动初始化。
target/release/fs-agent sync init /path/to/config.toml
target/release/fs-agent /path/to/config.toml
```

同步与执行可以同时启用，但同步库不得与 export、只读运行库及其实际目录别名重叠。启动会拒绝重叠目录。数据库和对象库必须位于支持 SQLite WAL、文件锁、硬链接和 fsync 的本地文件系统。

管理员操作需要停机，并使用同一个 `sync.lock`；另一实例持锁时命令失败。备份和恢复目的目录必须为空，且不能与源目录重叠。

```bash
target/release/fs-agent sync verify /path/to/config.toml
target/release/fs-agent sync gc /path/to/config.toml
target/release/fs-agent sync backup /path/to/config.toml /independent-disk/backup-001
# 配置中的 sync.root 应改为新的空目标目录。
target/release/fs-agent sync restore /path/to/restore.toml /independent-disk/backup-001
# 使用经过完整摘要校验的源文件修复指定对象。
target/release/fs-agent sync repair /path/to/config.toml PROJECT SHA256 /path/to/trusted-object
```

备份包含静止的数据库、仍存在的 WAL、对象、每个文件的完整性清单和最后写入的 `COMPLETE` 标记。校验使用独立副本，不修改最终备份。备份不自动打包认证配置、TLS 或部署凭据，应另行保存这些配置。用于防磁盘损坏的备份应存放在独立故障域。

恢复保持 authorityId，生成新的 historyEpoch 和 cursor 密钥。旧操作记录留作诊断，新代次不继承旧副本序号。客户端保留本地修改，重新注册副本、读取成员和 head、对账并激活，从 opSeq=1 开始发布。当前管理员命令提供同服务灾备恢复；另一个 authority 的克隆需后续增加明确入口。

初始化中断产生 `init.pending`，可重复执行同一 init 命令。恢复中断产生 `restore.pending`，普通启动拒绝打开它；可用原备份重复执行 restore。修复与 GC 的持久意图由正常启动恢复。已有对象、标记或 WAL 而数据库缺失时，服务拒绝启动。

init 续作也遵守上述拒绝规则：已有 storage.json、对象或 WAL 时，init.pending 不能授权创建空数据库或新身份。数据库身份已写入但 marker 尚未完成时，可以沿用原身份完成首次初始化。

verify 使用只读数据库连接，不执行启动恢复、不重建引用缓存、不登记 corrupt；它检查数据库、对象摘要和受保护 manifest 的引用闭包。损坏修复由 repair 显式执行，未完成的初始化/恢复应先完成对应流程。直接调用 backup 会停止该实例准入并等待活动任务，然后在数据库锁保护下复制；管理员 CLI 始终另取 root 独占锁。

## HTTP 使用顺序

路由位于 `/v1/sync`。只有 capabilities 豁免 `X-Sync-History-Epoch`；其余请求携带该头，命令体中的 historyEpoch 必须与头一致。旧代次的读取、查询、取消和写入全部拒绝。

1. `GET /capabilities` 获取 authorityId、historyEpoch、namespace、限制和支持的能力。
2. `POST /replicas` 注册 `{ "replicaId": "device-A" }`，得到 reconciling 状态。
3. `GET /projects?state=all`，再按选择范围读取 `GET /projects/P/datasets?state=all`；有 nextCursor 时继续分页。
4. `POST /replicas/device-A/activate`，提交 `scopes: [{projectId, cursor}]`。没有项目时可以提交空 scopes；已有项目时至少选择一个有效清单范围。
5. 创建项目、检查并上传对象、创建数据集；后续更新使用当前 head 条件发布。

注册、激活、pin 和对象上传采用各自身份契约；项目及数据集变更统一携带 operationId、replicaId、opSeq、authorityId、historyEpoch。项目范围内的变更另需 expectedProjectLifecycleRevision。opSeq 和 generation 均为规范十进制字符串，不使用浮点数。

```json
{
  "operationId": "device-A-3",
  "replicaId": "device-A",
  "opSeq": "3",
  "authorityId": "<capabilities authorityId>",
  "historyEpoch": "<capabilities historyEpoch>",
  "expectedProjectLifecycleRevision": "1",
  "expectedHead": { "generation": "1", "manifestHash": "<current hash>" },
  "nextManifestHash": "<uploaded canonical manifest hash>"
}
```

创建项目使用 `POST /projects` 和 projectId。创建数据集使用 `POST /projects/P/datasets`，增加 datasetId、kind、logicalId、manifestHash，首个 head 与成员事件一同提交。kind 支持 files、session、organization、definitions、bundle；files 使用文件 manifest，其余使用通用 bundle。项目和逻辑身份删除后不复用。

### 对象与 manifest

`POST /projects/P/objects/check` 接受最多 1000 个 hashes，仅返回当前项目 ready 对象。`PUT /projects/P/objects/HASH` 传原始字节，必须提供 Content-Length；采用整对象传输，无块级续传。对已验证存在的对象可以直接复用并返回 reused，避免满额时重复占用配额。新的上传流计算并验证完整摘要，超过长度、对象预算、期限或并发预算时拒绝。

manifest 通过同一 PUT 接口上传，使用 JCS 的受限 schema：固定 ASCII 字段名、整数 version=1、规范字符串及十进制 size，拒绝扩展字段。文件 entries 按路径 UTF-8 字节序排序；父目录显式存在，根目录隐含。服务端在发布时验证规范编码、对象长度及闭包。

```json
{"entries":[],"format":"fs-agent.files","version":1}
```

`GET /projects/P/objects/HASH` 支持单 Range、If-Range 和 ETag，下载前完整校验服务端对象，并保留打开的文件描述符直到响应结束。客户端仍需在全量接收或 Range 拼接后校验完整 SHA-256。`GET /projects/P/manifests/HASH` 只返回已经发布校验过的 manifest。

### 发布、发现和回执

`POST /projects/P/datasets/D/publish` 使用上面的条件请求。成功返回 committed 和新 head；同 manifest 返回 noChange，仍需满足旧 head 条件。竞争失败返回 HEAD_CONFLICT 和 not-committed，消费该 opSeq，重规划使用下一序号。

同副本、同序号、同规范命令返回原回执；同序号不同内容返回 OPERATION_REUSED。`GET /replicas/R/operations/SEQ` 查询持久结果。回执过期后返回 OPERATION_EXPIRED；保留高水位，不把旧序号作为新命令。`POST /replicas/R/operations/SEQ/cancel` 在原请求未准入时需携带 `{target, command}`，其中 command 是完整原请求；已提交结果不能取消或回滚。

进入提交阶段后，取消仅返回当前结果，客户端可以停止等待，但须通过查询确认终态。首版不承诺中断所有 pending 发布。

成员枚举返回固定 catalogRevision、cursor、nextCursor 和 changesCursor。`GET /projects/P/changes?cursor=...` 返回固定上界分页事件及 hasMore；发现到的是已发布版本，无法发现其他设备尚未上传的数据。游标签名绑定 namespace、项目、过滤条件、代次与期限，过期后全量对账。成员列表可以发现新建的 session 数据集，但服务器不合并 Round 或恢复旧运行。

`POST /projects/P/replicas/R/ack` 携带 cursor 和规范十进制 scopeRevision。ACK 检查副本活跃状态、写入门禁和新增行预算；范围 revision 不得回退，同一范围 revision 的消费位置也不得回退。它不代表客户端共同基线或磁盘应用已经完成。

### 历史、删除与保护

`GET /projects/P/datasets/D/versions` 与 changes 独立，支持 cursor/limit；`.../versions/G` 查询指定版本。响应包含 generation、manifestHash、committedAt、supersededAt、retainUntil 和 contentStatus。选择旧 manifest 后正常 publish，H8 恢复 H3 内容会产生 H9。

`POST /projects/P/datasets/D/delete` 携带 expectedHead；restore 携带 expectedDeletedGeneration、sourceGeneration 和项目生命周期条件。`POST /projects/P/delete` 固定当时有效成员；`.../restore` 撤销项目删除，任何必要对象损坏则整体拒绝。删除与恢复均推进项目 lifecycleRevision，旧写入计划不能复活。项目撤销删除不等于任意时间点整体回滚，projectCheckpoint=false。

旧 head 从被替代时起获得历史窗口，删除内容从删除时起获得回收窗口；当前有效 head 永远保护。对象保留根包括当前 head、已登记的历史/回收期限、回执期限和读取 pin。多个引用取保护并集，修改保留配置不缩短既有承诺。

`POST /projects/P/read-pins` 接受 manifestHash、requestKey、可选 ttlSeconds。renew/release 使用返回 pinId；读取保护到期后需要重建。release 可提前释放记录，对象可保守保留至已经承诺的期限。

服务每分钟执行一次有界 GC；管理员 gc 可立即运行。每批最多删除 1000 个对象，通过 deleting 状态和 gc_items 恢复未完成删除。运行期使用单调时间推进期限；重启发现时钟回退时暂停 GC。

配额按同项目去重后的已安装对象与上传预留计算，跨项目分别计费。当前、历史和暂存均占额度，不因容量满提前删历史。保留数据库工作空间；无法确认的存储错误返回 unknown 并关闭写入准入，查询持久结果后修复或重启。

准入在数据库锁内复核。关闭中拒绝新外部写入，已接受命令和已取得预留的上传可受控收尾；提交结果不确定后禁止继续提交。读取 I/O 或普通查询失败不会直接把对象标成 corrupt，也不一律关闭全库；已存内容摘要/长度不符和缺失才支持持久损坏判断。SQL 查询失败向上传播，不伪装成版本 expired；新上传摘要不符属于输入错误。

操作准入预留 operation-id 与 pending 回执行，最终业务新增行超限则回滚业务并更新既有拒绝回执。确定性拒绝只有在外层事务提交后才可返回终态；SQL 自动回滚、SAVEPOINT 清理失败或回执写失败返回 unknown，保留准入记录供恢复。逻辑行预算与真实磁盘故障分别测试，磁盘保留预算不等于永远能写成功。

drain 只在停止准入后调用，跟踪接收中的上传、blocking 工作、后台变更和异步清理；超时明确返回 SYNC_DRAIN_TIMEOUT。进程记录 sync.drain_failed 后按退出期限结束，剩余意图由重启恢复，不能将超时当作成功排空。

历史与 catalog 索引保守保留；首版未压缩所有旧索引或身份记录，达到 max_metadata_records 时拒绝增长。该限额同时覆盖 records、对象、manifest 引用和上传记录。副本过期后不得原地激活，应注册新副本并对账。正文可回收，身份 tombstone 和序号高水位独立保留。

## 实现与验收

SQLite 使用 WAL、synchronous=FULL、foreign_keys=ON。项目、数据集、版本、成员快照、副本和操作使用带 scope/kind/key 主键的类型化 records；对象、manifest_refs、上传预留和 gc_items 使用独立关系表。领域规则在 policy/commands/operations/catalog/retention，文件机制在 store，HTTP 只解析和传输。

```bash
cargo fmt --check
cargo test --all-features -- --test-threads=1
# 新版 clippy 的 manual_inspect 告警来自现有两个非同步模块。
cargo clippy --all-targets --all-features -- -D warnings -A clippy::manual_inspect
FS_AGENT_PROCESS_TEST=1 cargo test --all-features -- --test-threads=1
node scripts/check-sync-fixtures.mjs
cargo build
python3 scripts/sync-smoke.py target/debug/fs-agent
```

sync-fault-injection 是显式测试 feature，默认构建不读取故障环境变量。它覆盖准入、发布提交、安装、GC、修复和恢复中断，以及回滚/回执写失败和提交已成功但结果不确定的分类。故障子进程测试串行运行，避免 fork 期间暂时继承其他测试的 root 锁；CAS 测试仍显式创建竞争线程。库内正确性测试另包含实际 SQLite FULL、锁内门禁、只读 verify 和流式关闭/取消。真实 HTTP 脚本包含 A/B/C 条件发布、回执去重、原 root 不可用时的空目录恢复、旧 epoch 查询/取消/重试隔离及受保护版本的摘要校验。进程退出和逻辑 I/O 故障不代表真实断电验收。
