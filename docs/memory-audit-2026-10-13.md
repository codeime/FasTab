# FasTab 内存与热路径扩展审计（2026-10-13）

源码基线：`e57178113cd8816b6b52ccef7613c47a807001b4`。当前安装版为 0.0.7。本轮在之前按需加载、10 秒空闲回收、原生窗口释放和双向 outbox 优化的基础上继续调查。

**状态：本轮实现、逐项 review/fix 和整体源码复核完成；远端 CI 与安装版验收另行验证。** 逐项记录及保留的协议边界见第 7–8 节。主代理负责共享 IPC、热路径与交叉复核；GPT-6 Astra ultra 负责队列协议设计和独立审查，GPT-6.1 Sol 分别实施 engine 与 PTY/IME，并交叉复核。没有本地构建、编译或执行项目回归测试，没有重启应用、操作用户终端或发送合成消息到正在使用的应用。下文现场数字来自改动前安装版，不能算作本次优化收益。

下文的源码行号均指上述基线；P1/P2 表示处理优先级，不代表现场已经复现。物理内存、活跃堆分配、估算 payload 和容器容量分别记账。

## 1. 当前运行现场：之前的回收确实发生了

2026-10-13 16:31（Asia/Shanghai），主进程 PID 95230，启动于 10-12 15:33:19；`sample` 确认路径为 `/Applications/Fastab.app/Contents/MacOS/fastab`、版本 0.0.7。

| 指标 | 本次观测 | 解释边界 |
| --- | --- | --- |
| desktop phys_footprint / peak | 28.4 / 51.8 MiB | 已运行约 25 小时；不是与旧版本的受控 A/B |
| malloc 活跃分配 | 9,863,776 bytes，约 9.41 MiB | 包括原生框架与 Rust，不能全部算作可清缓存 |
| malloc dirty+swap fragmentation | 9,951 KiB，约 9.72 MiB | 分配器区域中的未使用空间，不等于能立即归还的页数 |
| registry、三类 hook cache、catalog/captured bytes | 全部为 0 / unloaded | 这些已观测 owner 在本次采样确实已释放 |
| history diagnostics | 177 行、0 个 slot index、7,402 bytes | 当前统计没有覆盖全部 NextWords 结构，见 M4 |
| engine worker | 0 queued / active / abandoned | 无当前积压或放弃中的执行线程 |
| GPUI / 图标 | 0 窗口、overlay hidden、0 图标与 pending paths、worker inactive | heap 类型表也未列出 GPUIWindow/GPUIView/GPUIPanel/CAMetalLayer |
| IPC | 3 sessions，outbox messages/bytes/responses 全部为 0 | 不覆盖所有 incoming 队列、reader backing 和 session 字符串容量 |
| 请求 | 89 submitted、73 completed、16 cancelled，0 failed/watchdog/panic | 不能据低负载现场推断大输入场景无问题 |
| IME | 9.2 MiB，peak 9.4 MiB | 保持现有 IMK 连接，未重启 |
| 三个 fterm | 5.6 / 5.6 / 5.8 MiB | 多标签累加仍是重要维度 |
| 五个产品进程合计 | 54.6 MiB | 不应与仅主进程的 28.4 MiB 混用 |

两秒线程样本以主循环、Tokio、数据库维护、日志与 engine 等待为主，没有发现该样本内的忙转。AXUIElement 为 183 个、NSLock 为 16 个；缺少同负载前后计数，不由一次绝对数量判定 AX 泄漏。

现场证据：`/tmp/fastab-memory-audit-20261013/{desktop-vmmap.txt,desktop-heap.txt,desktop-sample.txt,engine.json}`。临时文件可能被系统清理，重要数字已记录于上表。此前沙箱限制导致 `ps` 和 IPC 访问失败，本轮取得只读进程访问后重新采样；失败结果没有记成零占用。

结论是“已观测到旧回收链路生效，同时仍有未被现有诊断覆盖的所有者”，不是“进程所有可释放资源都已清空”。本轮不继续压缩 10 秒宽限，也不以 20 MiB 为无条件验收线。

## 2. 新确认的优先处理项

### M1 / P1：PTY 同步输出的时间和字节限制没有形成真正边界

**源码确认，尚未做新运行回放。** 每个 fterm 的 `Processor` 拥有 `SyncState.buffer`，`crates/alacritty_terminal/src/ansi.rs:132` 默认即 `Vec::with_capacity(2 MiB)`。这只是容量请求，不能直接视作每个标签已经消耗了 2 MiB 物理内存。

进入 DCS synchronized update 后，`:237` 每字节 push；`:256-257` 达到所谓 2 MiB 上限时只记录 `pending_dcs = SyncEnd`，`:268` 对普通 ASCII 继续等待，直到后续 ESC 才在 `:276` 执行 `stop_sync`。150 ms deadline 虽在 parser 内设置并暴露 `sync_timeout()`，全仓没有生产调用者处理该 deadline。`crates/fastab_term/src/main.rs:292-340` 的生产路径只有 `advance` 与输出边界检查，主循环没有对应 timer 分支。

**触发：**同步开始序列（十六进制 `1b 50 3d 31 73 1b 5c`）后持续输出不含 ESC 的文本。buffer 可超过 2 MiB 持续增长；如果随即停止输出，超时也不会自动结算。fterm 内部 Term 更新暂停，可能影响 prompt/编辑缓冲与补全；原始 stdout 仍透传，不能描述为整个终端停止显示。

**释放：**后续满足结束条件时 replay；`ansi.rs:215` 只 clear，保留历史最大容量；最终随 Processor/标签退出释放。

**实施方向：**

1. 在达到字节预算时强制结束当前同步积累，不再依赖未来终止字符；定义与 parser replay 相容的边界，结算时清理 pending_dcs 等同步状态，避免强制结束留下旧 SyncEnd。
2. 将 deadline 接入 main loop，即使没有下一块 PTY 输出也能结算；避免在已就绪的输出分支下饿死 timer。
3. 默认零/小容量，实际进入同步输出时分配；结算后过大的空缓冲退回基准容量。
4. timeout/forced replay 产生的本地控制事件仍走同一 `MainLoopControl`，保留先转发原始输出、后发送 CSI-u 控制的顺序，不能回到 parser 自发自收的阻塞 channel。

**远端验收：**真实 `process_pty_bytes` 路径覆盖多 read、超过 2 MiB 无 ESC、停止输出、合法结束、UTF-8/OSC 边界和 CSI-u；核对全部 stdout 字节、Term 后续可用、最大积累及空缓冲回收。新包再做隔离终端回放，不能只测 `SyncState` 辅助函数。

### M2 / P2：IPC 大包的 backing 与 session 文本有连接级高水位保留

**已独立源码复核。** `crates/fastab_ipc/src/recv_message.rs:34` 消费完整消息后仅 `BytesMut::advance(len)`。锁定 bytes 1.12.1 的 `advance_unchecked` 只移动指针并减少可见 len/cap，advance 本身不释放；后续 reserve 可以复用原 allocation。连接空闲时旧 allocation 可长期保留，直到后续分配替换或 reader Drop。因此不能用“空 buffer / capacity 接近零”证明 backing 已回收。

`crates/fastab_remote_ipc/src/remote.rs:93` 的 reader 随连接长期存在。`crates/fastab_proto/src/lib.rs:193-203` 读取长度、复制完整 body，reader 本身没有 frame byte limit。发端 outbox 的预算没有覆盖收端未完成帧与解析暂存。

另一条独立 owner 是 `crates/fastab_desktop/src/remote_ipc/mod.rs:49` 的 `session.edit_buffer.text.clone_from(&hook.text)`：空/短新输入复用旧 String capacity；prompt/preexec 更新 context、通知 EndInput，却没有回收这个容量。连接 `remove_id` 最终释放 session。

**影响范围必须收窄：**正常 fterm 单个 encoded frame 已限 4 MiB，编辑文本还受 prompt 到 viewport 底部的范围约束；大粘贴让 prompt 滚出可直接返回 None。不能声称每次 2 MiB paste 都会让 desktop 保留 2 MiB 文本。较大的 env/alias frame 可使 reader backing 保持高水位；session 文本高水位需此前实际接收到较大的 EditBufferHook，例如大 viewport、单 cell 组合字符或其他有效协议客户端。

**实施方向：**在帧完整消费且无余留时替换过大 reader backing；有下一个半帧时必须保留全部未消费字节。session 文本在空输入/执行结束或明显缩小时按阈值换回小缓冲，保留需要的当前输入语义。另为接收帧加入与发送协议、历史客户端兼容的字节约束；接收限制与 allocation 回收分开验收。

**远端验收：**真实握手 socket 上大帧→小帧→prompt/preexec→idle→断连，记录 backing ownership、session 文本 capacity、解析峰值和物理内存。覆盖多帧同 read、半帧、最大合法上下文、错误 EOF；不要仅断言可见 `capacity()`。

### M3 / P2：脚本输出展开后没有对应预算，去重前数据被 session 保留

**源码确认。** `crates/fastab_engine/src/process.rs:6` 的 256 KiB stdout 上限约束原始字节；`generate.rs:1034-1045` 的 `all_split` 却为每段创建完整 Suggestion。重复 `x\n` 可以展开成约 13 万行。

关键并非最终界面显示多少：`generate.rs:404` 先 `rows.clone()` 写入 GeneratorSession，`:430` 才去重输出。结果即使只有一条，session 仍持有全部重复行；下一键 `:658-663` 的 refilter 又复制匹配行。三类 hook cache 的 4 MiB 预算不覆盖 session。`rank.rs:809-823` 会把小最终结果重建为小 Vec，因此不把最终输出 capacity 也列成永久保留。

**释放边界：**参数 identity 改变、EndInput、clear/generation refresh 或 Engine Drop；这是活动输入期间的放大和重复工作，不是已确认的空闲后永久泄漏。脚本/splitOn 由当前 compiler 保留到 IR，属于实际可达生产路径。

**实施方向：**先消除 session 中无价值的重复行和不必要的整批 clone，再补展开后 row/byte 统计。去重必须与既有名称、metadata、query term、排序和 refilter 语义等价。若限制唯一候选总量，必须先定义跨源排序/过滤规则，不能简单把每源截成前 49 条；单目录 top-49 不是全列表预算。

**远端验收：**真实 EngineClient + 脚本 fixture，覆盖重复短行、unique 行、空分段、多 generator、trigger/debounce、改 query、取消和 EndInput。断言最终候选及其顺序不变，session 保存的行数/bytes 与下一键复制量减少；记录大输入耗时和 peak，而非只验证缓存 helper。

### M4 / P2：历史行数上限没有约束 NextWords 放大与索引字节数

**源码确认，当前现场历史量很小。** 实际来源是 shell/fish/custom history command 与 SQLite recent rows（`rank.rs:1068`、`:1188`），每源最多 10,000 行；shell/custom-command 输出还受 256 KiB stdout 上限约束，但没有独立的单条历史长度或 NextWords 展开预算；SQLite recent rows 没有 command 字节上限。allShells 可合并多个源。正常排序也使用 Frecency/NextWords，不只有显式历史 generator 才触发。

`rank.rs:426` 在每个空格处再次把完整 prefix 交给 `bump`；canonical 路径 `:376` 从 trie root 重走 token，长多 token 行有平方级工作。非规范空白 prefix 在 `:389` 保存完整 String：约 20 KiB、10,000 个短 token 且有双空格的行，按前缀长度求和就可能产生约 100 MB 的字符串正文分配。这个数字是源码复杂度模型，**不是机器上的测量结果**。构造内没有取消检查；`runtime.rs:695` 前后门控不能及时中断正在展开的历史。

Engine 还持有 `HistoryStore`，`history.rs:125` 的 slot index 只限 32 项、不限字节，`:392-396` 保存独立 String。EndInput 与 10 秒维护释放 specs/hooks/catalog，却不释放 Frecency/NextWords 和这些 indexes。后者仍有 alias/shell 失效、LRU、clear/generation refresh 等释放路径；不称其无条件泄漏。

**诊断缺口：**`history.rs:220` 统计 lines/index，不包含 Frecency 的 NextWords；当前 7,402 bytes 不代表完整历史资源总量。

**实施方向：**先将 NextWords 改为单次行遍历，精确 prefix 可共享底层行与 byte range，加入协作取消；保留空白、权重、重复命令、最近优先语义。随后补齐诊断，再用大历史回放决定 index 的 byte budget/idle retirement。不要为当前约 7 KiB 的原始历史强制频繁卸载重读。

**远端验收：**实际历史 source fixture，覆盖规范/双空格长行、Unicode、取消响应、多个 root/alias/shell/slot、重复权重与排序、EndInput 后 owner。对 canonical 路径检查复杂度随 token 数的增长，不能只测短行正确性。

### M5 / P2：有界 outbox 后面仍有无界 incoming 与 host 队列

**源码确认，尚未制造慢消费者现场。** `crates/fastab_term/src/ipc.rs:133` local incoming、`:219` remote incoming、`:170` 每连接 response queue 均无界；`:157` accept 后直接 spawn，缺少并发 admission 限制。reader 持续解码后入队，主循环正在等待 stdout/master 写入时仍可继续积累小合法帧。`main.rs:1222/1261/1286` 的 biased select 也把连续 PTY 输出置于 remote/local 消费之前。当前 generation 检查防止旧消息执行，但不提前释放排队载荷。

desktop 的 `event_loop.rs:106-108` 仍为无界 host channel；当前 D8 只合并连续完全相同的非空输入。不同输入与控制/位置/结果事件是屏障，主线程慢时仍会积压。这是原优化有意保留的语义边界，本轮补全后续设计，不称 D8 修复失效。

**实施方向：**按方向建立包含 in-flight 的 count+estimated-bytes 预算、连接所有权及取消收尾；先为 fterm incoming 加真实背压/退连接协议，再设计 host admission。严禁机械地将同步 parser 或主线程 sender 改为阻塞 bounded send，否则会重引自锁；也不能无声丢弃 Insert/EndInput，或合并不同输入破坏 paste/history/self-insertion 判断。退连接后不得自动重放可能已执行的 Insert，部分帧写出后不得继续复用连接。诊断要区分 decoder、incoming、outbox、host，不能只报当前 outbox 的零。

**远端验收：**合法小帧突发+暂停消费者、持续 PTY 输出、slow stdout、断连/重连旧 generation、并发连接与关闭期间控制消息。断言总预算有界、任务能退出、控制有序且完整；单帧最大长度测试不能代替总队列测试。

## 3. 更低风险的热路径优化与条件性机会

| 编号 | 已确认的工作/所有者 | 改进方向与约束 |
| --- | --- | --- |
| O1：Unix paste 的无用文本副本 | `fastab_term/src/input/mod.rs:1527-1530` 构造完整 Paste String，同时交出 raw；`main.rs:1175-1178` 在 Unix 正常 `Some(raw)` 路径只消费 raw | raw 路径避免整段 UTF-8 解码与 String；保留 Windows `None + Paste`、无效 UTF-8、跨块 marker、完整字节序列。降低峰值和 CPU，不称长期泄漏。 |
| O2：候选高亮按字符分配 | `fastab_gpui/src/list.rs:730-777` Vec<char>、每字符 String/TextRun 后再 merge；`:271-292` fuzzy 另建字符/小写/索引数组 | 用原字符串 byte ranges 构造合并 run；保持 Unicode、ASCII-only 大小写、匹配位置语义。先测 visible rows 的 allocation 次数与帧耗时。 |
| O3：每帧候选对象深复制 | `list.rs:1220` clone 整个 SuggestionItem，`:1382-1391` click closure 保留，点击再复制插入字段 | 用共享不可变候选快照/最小点击载荷；不能直接捕获可变列表下标，使旧帧点击插入新行。保留 generation/identity 校验。 |
| O4：每 hook 重建环境变量 | `fastab_engine/src/hook_types.rs:63` 深复制为 BTreeMap；`hook_backend.rs:617` typed context 再复制为 Vec | 请求内共享只读 env，实际访问全环境时物化；保留重复 key 的 last-write-wins、顺序及 executeCommand 环境语义。Arc 请求传递已优化，但不能据此认为 dispatch 零复制。 |
| O5：IME 合并发生在入队之后 | `fastab_input_method/src/imk.rs:100` 无界 mpsc，`:113` worker dequeue 后 drain_latest；`:158` connect 无 deadline，200 ms 仅限 write | latest 单槽 mailbox 可以明确限制积压；连接阻塞先用远端 macOS fixture 验证。保留 worker 30 秒空闲退出、最新 caret 恢复和已有 IMK 连接。未发现本次现场 connect 卡住。 |
| O6：普通 debounce 的过时 timer | `fastab_desktop/src/overlay.rs:1275-1286` detached timer，换代/隐藏只在 `:1294-1299` 到期拒收 | 像 ShowIntent 一样直接拥有并取消旧 Task，可提前释放外层载荷；GPUI timer 经 `vendor/gpui/src/executor.rs:357` 调用 `platform/mac/dispatcher.rs:57` 的 dispatch_after_f，取消 Task 不会撤销已排入 GCD 的 runnable，底层释放须单独验证。当前 bundled 常见默认 200 ms，影响有限；长自定义 delay 才会长持 task/proxy，不描述成大量候选/窗口被钉住。 |
| O7：文件 watcher admission | `fastab_desktop/src/file_watcher.rs:17` 无界事件 channel；相同 settings 内容去重在读取之后 | 如实测高频配置写入有负担，再把目录事件变为有界 dirty/reload 信号，保留最后版本与失败重试；目前没有配置事件风暴证据。 |

这些项不需要以牺牲候选完整性或改变输入语义换取收益，适合与故障边界修补分开提交、分别测量。

## 4. 降级或排除的方向

- **static package/git cache：只列自定义 IR 条件项。** `generate.rs:228-230` 的 `PACKAGE_JSON/GIT_REFS` 仅按 32-entry LRU 管理，不参加 Engine idle/clear；package helper `:1397-1400` 整份读取/保留 JSON，无 byte limit。可是当前 compiler `scripts/compile-spec-ir.mjs:858` 不生成 `npm-scripts`、`npm-deps`、`git-refs`，bundled npm/pnpm 走 typed/native hook，未找到普通打包路径的调用证据。因此不能归因日常 npm/git 内存，也不优先为它搭复杂回收系统。保留自定义 IR 时可做紧凑字段投影、不缓存 oversized、取消发布屏障；删除则先确认兼容范围。
- **没有新确认的正常隐藏/Settings 关闭后 renderer 泄漏。** renderer/layer 主动 Drop、Metal pool 代际释放、最后窗口关闭后的 text/asset cache 清理仍在。持续不关闭窗口的 glyph atlas/raster bounds 可单列长期活跃场景实验，但不清空稳定 FontId，也不在 GPU 使用中抢占资源。
- **已修项不重复实施。** grid 可达行/列容量、stdin 聚合/raw backing、catalog 惰性与 captured bytes、hook 三类 byte/idle、engine mailbox、单 native icon/AX worker 均已有对应处理。
- **不是所有 parser 缓冲都无界。** 锁定的 VTE 非 std 构建 OSC 使用 1024 字节 ArrayVec；title stack 有深度界限。M1 是同步输出路径，与这些缓冲分开。
- **SQLite 与 AI 不作为本轮无依据改动。** SQLite 生产 recent history 按主键 id 倒序，不是猜测中的全表 start_time 排序；pool 已有 idle 收缩。AI 响应有 content-length/chunk 双检查，推荐 cache 64 项、ID 限 32 字符。AI 设置隐藏不等于已保存运行配置停止，审计仍核过这些边界。
- **分配器强制回收仍不加入。** 约 9.72 MiB fragmentation 不是保证可回收收益；此前 pressure relief 没建立稳定收益。本轮没有新的 allocator 对照实验，不缩栈、不换全局 allocator，也不把 mmap 虚拟大小算成真实占用。

## 5. 建议实施与 review 顺序

| 步骤 | 内容 | 关闭条件 |
| --- | --- | --- |
| S1 | M1 同步输出真上界、deadline、惰性缓冲 | 真实 parser/main-loop 远端回归通过；Astra 复核 replay/CSI-u/控制顺序；新包隔离回放 |
| S2 | M2 reader backing 与 session text 回收，补接收帧预算 | 分帧/拼帧/大 context 兼容，空 backing 真释放；不以可见 capacity 判断底层所有权 |
| S3 | M3 generator session 去重与分配减量 | 输出/排名/refilter 等价，计数包含保留的 session；总量策略单独决定 |
| S4 | M4 NextWords 单次构建、共享 prefix、协作取消 | 长行复杂度与取消延迟改善；空白/权重/排序不变；完整历史 bytes 可观察 |
| S5 | M5 接收侧总预算与公平性 | 先出协议设计再改队列；压力/断连回放通过，无丢控制与自锁 |
| S6 | O1/O2/O3/O4 逐项减少热路径临时分配 | 每项单独对照输入、结果、分配次数/峰值与耗时；不要一次重写多个 hot path |
| S7 | O5/O6，以及有现场依据时的 O7 | 明确任务取消/单槽所有权；真实 caret、timer、最后 settings 快照仍正确 |
| S8 | 整体回放与安装版验收 | exact SHA 远端编译/Clippy/测试通过；同负载安装版 A/B；总体 review/fix 后才宣称收益 |

每步执行“实现 → 独立 review → 修复 → 针对性远端回归”，出现新修改才重复受影响验证。所有构建和可执行项目回归继续放远端 CI；不能以本轮源码审计冒充测试通过。提交按内容分组，推送后的 CI 必须核对本次确切提交。

## 6. 统一测量与诊断要求

1. **固定身份与负载：**记录运行 PID/启动时间/构建身份、标签数、终端种类、history/env/输出规模、设置窗口操作次数；同一负载比较冷启动、首次触发、连续使用、空闲 10/60 秒与较长放置。
2. **分清三层数字：**物理占用用 `phys_footprint`；live heap/fragmentation 用原生工具；业务诊断提供 payload/count/owner 和 backing 高水位。三者不能相加或相互替代。
3. **补缺失 owner：**同步输出 len/cap/deadline、reader backing 高水位、session text、各 incoming/host 队列及 in-flight、GeneratorSession 行数/bytes、NextWords 与 history indexes、存活 debounce/IME pending 槽。诊断不得读取 shell 文本、env 值或重新加载 catalog。
4. **兼顾恢复成本：**记录首次候选/冷恢复 p50/p95、主线程停顿、扫描/分配次数、复制字节和空闲唤醒；释放更多却每次重建更慢不能自动算改进。
5. **覆盖真实边界：**普通输入、大粘贴、同步输出、重复/独特 generator 行、长历史、多标签切换、Settings 开关、IME 与 AX 终端、慢 socket/文件系统、取消和连接半关闭。现有 Metal workflow 仍需真正可用的 GPU/GUI runner；普通 CI 通过不证明硬件路径通过。

本轮结果支持先补齐资源预算和遗漏的所有者，再减少临时分配；当前证据不支持继续一刀切缩短空闲期限。

## 7. 实施记录与验证边界

下表的“源码复核”只表示实际 diff 的独立审查与修复，不表示远端测试已经运行。新增回归均放在现有 Rust 测试模块，禁止在本机执行。

| 项目 | 已实施内容 | 当前复核与回归 |
| --- | --- | --- |
| M1 | 同步缓冲默认空；2 MiB 达限立即 replay；main loop 接入 150 ms deadline；过大空缓冲退休；沿用控制事件/CSI-u 顺序；按批次记录 buffer len/capacity/deadline 数值诊断 | Sol 实施、Astra 独立源码复核通过；覆盖合法结束、连续 ASCII、静默超时、半截 UTF-8/OSC、CSI-u、Windows delayed prompt。主循环正在等待慢 stdout/master 时不能抢占，不承诺严格 150 ms 墙钟释放 |
| M2 | shared protocol 编码/解析同为 4 MiB encoded frame；16 KiB 分块读取；高水位 reader 替换 backing 并保留小尾帧；session 大文本缩小及 prompt/preexec 时退容量；诊断补 session 文本 len/capacity | 主代理实施、Sol 独立源码复核通过；Bytes witness 检查旧 backing 真失去 owner；分块、拼帧、超长 header 与真实握手大/小输入、prompt/preexec 回归。reader backing 本次通过测试 owner witness 验证，未增加进程级 reader 聚合字段；frame 限制不是 allocator capacity 上限 |
| M3 | split/lines 先借用去重再创建 Suggestion；session 按 name/kind/insert/hidden 保留首行 metadata 后压缩容量；诊断补 session rows/bytes | Sol 实施、主代理独立源码复核通过；真实 Engine 脚本 fixture、删除源文件后 refilter、空分段、unique 行完整性、EndInput。未对唯一候选硬截断 |
| M4 | NextWords 改为非递归 arena、单遍扫描与 token ID；协作取消不发布半成品；history indexes 32 项及 4 MiB 估算预算；10 秒只退休 derived indexes；补全 NextWords 诊断 | Sol 实施、主代理独立源码复核通过；旧 walk oracle 对照多种空白、10k 深度低栈构造/释放、长单词共享、取消、大索引完整服务但不缓存、真实 Engine idle/cold-rebuild |
| M5 | fterm 共用 256 条 / 8 MiB 估算 incoming 预算、8 个 local connection / 8 个 RunProcess 执行槽；desktop 异步 host 256 条 / 16 MiB、remote/local 连接 32/64；主循环公平选择；预算贯穿处理与回复 | Sol/Astra 分别实施、主代理/Sol 交叉复核并修复关闭、取消、回复和环境快照边界；完整范围与同步残留见下文。压力回放尚待远端执行 |
| O1 | Unix raw paste 产 RawString 并移交完整 raw backing，避免无用 UTF-8 文本副本；默认 parser 与 Windows text Paste 路径保留 | Sol 实施、主代理独立源码复核通过；真实 Unix stream 与 parser 分块 fixture 覆盖 marker、无效 UTF-8、嵌入 ESC、后续按键、旧 backing 不再共享 |
| O2 | fuzzy 不再另建小写字符数组；高亮按原始 UTF-8 连续 byte range 建 run | 主代理实施、Sol 独立源码复核通过；增加 Unicode/组合字符/ASCII case/公共前缀对照 |
| O3 | 帧闭包保留结果与排序 revision、行号；点击先校验 revision/可见性/query，再复制插入数据 | 主代理实施、Sol 独立源码复核通过；覆盖同 identity 的 metadata 更新、AI 重排、隐藏后的旧帧，继续保留 desktop generation/session/identity 校验 |
| O4 | 请求内共享环境原始字符串和惰性有序索引；单键反向借用查找；typed/native context 共用 | 主代理实施、Sol 独立源码复核通过；保留 last-write-wins、字典序、JSON contract、filegen 原始 env 顺序；验证字符串 backing 共享，并通过 typed Exec fixture 核对传给命令执行器的环境 |
| O5 | IME 单 pending mailbox，允许另一个 in-flight frame；退休/enqueue 同锁、网络发送无锁、异常退出可重建 | Sol 实施、主代理独立源码复核通过；阻塞 sink 突发、最新值、退休交界、worker failure。未改 IMK 生命周期或 OS connect 行为 |
| O6 | 普通 debounce Task 有明确 owner；新 generation、退休与隐藏取消旧任务 | 主代理实施、Sol 独立源码复核后修正两处旧测试的可变 controller 绑定，复核闭环；GPUI executor 回归验证只发送最新任务事件。底层已排入 GCD 的 timer 仍可能唤醒，未宣称撤销原生 timer |
| O7 | 保持条件项 | 未观测配置写入事件风暴，按原审计条件暂不改 watcher 合并协议；不能用本次其他队列限额宣称 watcher 已有硬上限 |

本记录的源码复核阶段尚未执行远端 CI 与安装版 A/B，不能用远端旧 SHA 的成功状态替代此次改动验证。后续 CI 必须覆盖当前确切提交的 macOS workspace 编译、Clippy、现有及新增回归；GUI/Metal 与安装版 footprint/cold-latency 另行验收。


### S5 desktop host：本次实现范围及保留的协议边界

- **已接入有限 admission 的来源：**remote socket 的 EditBuffer、Prompt/PreExec/PostExec 通知、EndInput、InterceptedKey Action、session cleanup，以及 local socket 的 caret/focus/event/UI command/diagnostics。每个 connection 顺序 await，未为每条消息 spawn 等待任务；保留全部不同输入和 control barrier，仅继续合并连续且尚未消费的完全相同非空输入。
- **预算的准确含义：**异步 host 事件为 256 条 / 16 MiB 估算 Rust payload，permit 从准入持有到 dispatch 结束，包含 in-flight。`host_async_events` / `host_async_payload_bytes` 报这部分；`host_async_waiters` / `host_async_waiting_bytes` 报已构造、尚未获准进入该预算的等待输入。后者另受 remote 32 / local 64 个 accepted connection 的来源数限制；还存在当前解码帧、reader backing、session、下游 engine/UI 所有权，**不能称为整个进程只有 16 MiB 或实测节省 16 MiB**。计数和字节快照是并发下的近似诊断，不是物理 footprint。
- **连接生命周期：**listener 在 accept 前取得 slot，slot 持有至 handler 和 cleanup hook 都结束；因此慢 host 不会把已关 socket 变成无限 cleanup waiters。remote 未握手与 local 首完整帧均有 5 秒 deadline；local 后续 idle policy 保留。local response 写超时后退休该连接，命令可能已执行，不重放。remote 任意未握手 Request（包括 PostExec）不能进入 await hook。
- **关闭与公平性：**唯一 host receiver 关闭时，用与所有 enqueue 相同的锁发布 closed、关闭 permit 等待并显式清空 flume queue；proxy 继续存活也不能保留旧 payload。GPUI 每批 64 个事件通过真实 1 ms timer 让出一轮，避免始终 ready 的队列在单个 poll 中无限排空；这不抢占单次较慢的 dispatch/native call。
- **同步残留仍然存在：**GPUI 点击、可见行和 completion result/timer 回调；host dispatch 内派生 caret/permission/control；tray/menu 与 fallback 菜单线程；AX/NSWorkspace focus 和原始 `WindowServerEvent` FIFO；配置/permission/credential/AI 等原有同步通知。它们仍使用非阻塞同步 enqueue，由 `host_sync_events` / `host_sync_payload_bytes` 暴露 host ownership，AX 上游另外暴露 `host_ax_events`。native object 和 opaque error/credential 的内部堆不被该估算完整覆盖。**本次没有声明整个 host 或 AX source 已有硬上限。**
- **为何没有机械限制 AX：**原生 callback 的 blocking send 会阻塞产生/消费事件的 UI run loop；activation worker 在 scheduling mutex 下使用 try_send，改为 bounded 会丢 focus。直接去掉 Tokio bridge 并同步映射进 host 又会改变与异步 input 的 barrier 先后和 frontmost 过滤时间。未证明等价前保留原管线，不用 latest-only 覆盖不同 focus，更不丢 Insert/EndInput。
- **后续闭环项：**为同步来源设计可证明的源级 slot/握手或同线程 host dispatch；先规定跨 input/focus/caret 的顺序与 stale invalidation，再压缩仅等价的状态。completion result 的任意唯一候选目前无总量硬截断，不能直接套 16 MiB ingress 后忽略 oversized 错误。O7 配置风暴仍需现场证据和独立合并协议。

新增回归涵盖满 count/bytes、permit 持有到 dispatch、同步 main→self、不同输入/EndInput 保序、关闭时 Weak payload 真释放、等待取消、connection cleanup 占槽、握手拒绝和静默/半首帧退出。当前仅已写入与源码复核；未在本机编译或执行，仍需远端 CI 和安装版行为/资源回放。

### S5 fterm：预算、取消与本轮 review 修复

- local 与 remote incoming 共用 256 条 / 8 MiB 估算载荷预算，包含已取出而尚未处理完的请求；RunProcess 继续持有到任务退出，local 回复继续持有到 writer 完成或退休。已解码但等待预算的载荷单独计数，并由最多 8 个 local connection 加当前 remote connection 限定来源数量；reader backing、子进程输出、Command 内部存储等不是这一个数字能完整描述的总堆。
- local 同一连接一次只保留一个处理/回复中的请求，回复槽在请求准入前预留。主循环用 oneshot 交出结果，不等待慢 writer。完整 admission（预算、串行槽、reply handoff、入队）最多等待 5 秒，写超时或响应预算不足均退休连接；命令可能已执行，不能重放。读帧有 30 秒期限。
- RunProcess 最多 8 个执行槽。环境在 main dispatch 时冻结并追加估算预算，保留此前观察 shell context 的时序；预算不足明确返回 busy。尚未开始的任务等待执行槽时监听 generation 失效，旧连接断开便退休；已经开始的任务保留既有执行超时及 kill-on-drop 行为。
- main loop 先检查同步输出 deadline，其余 readiness 公平选择；实际 handler 在 select 外完成，因此 timer 不会取消一次已开始的 PTY/stdout partial write。local NotifySSH 直接走 MainLoopControl，不同步发回自身 bounded channel。

逐项独立 review 找到并修复了以下问题：

1. **关闭 receiver 不等于释放 flume queue。** 保留 sender 时原队列仍拥有 payload；desktop receiver 现在与 enqueue 同锁发布 closed、关闭准入等待并主动 drain，Weak witness 覆盖真实所有权释放。
2. **环境快照不能提前到 decoder。** 那会越过尚未处理的 UpdateShellContext。最终冻结点仍在 main dispatch，并用非阻塞追加预算避免主循环等待自身消费。
3. **只限制读帧会留下 admission 等待者。** peer 退出后，预算或串行槽等待可能继续占住连接。deadline 已覆盖整段准入。
4. **只限制请求会漏掉大响应。** local 回复追加 owned payload 与编码暂存估算；不足时明确关闭回复/连接，不能悄悄当作无回复继续。
5. **失效 generation 的排队命令仍会持有环境和预算。** 执行槽等待现可被 generation 失效取消，不改变已经开始的命令策略。
6. **未握手的 PostExec 可绕开握手期限。** remote 所有 Request 现在统一要求握手完成；local 静默/半首帧也有退出边界。
7. **Insert 只写一次 PTY 会在短写时截断。** 真实 InsertText 分支改用既有 `AsyncMasterPtyExt::write_all`，继续传递写错误且不重放；socket fixture 将每次写入限制为 257 bytes，核对 128 KiB 请求的完整字节。

这些修复已按实际 diff 交叉复核。远端仍须执行真实 socket、队列满载、旧 generation 取消、持续 PTY 输出与慢 writer 回放；不能仅凭 helper 单测或格式检查宣称关闭了全部运行时边界。

## 8. 整体源码复核结果与剩余验收

GPT-6 Astra ultra 已复核 M1/M2、generator 去重、NextWords/历史索引、O1–O6 与跨模块生命周期；desktop host 自身实现另由主代理和 Sol 独立复核。关闭、取消、环境快照、响应预算及最后 Insert 短写修复均重新复核闭环，当前未发现未解决的高置信源码阻断问题。

本地只运行直接 `rustfmt --edition 2024 --check` 与 `git diff --check`，均通过；检查包括新增 `ipc/incoming.rs`。这不是编译或测试通过。现有 Rust 测试模块中已补真实 parser、Engine、socket、GPUI executor 与 IME sender fixture，尚未执行。

后续验收必须覆盖本次确切提交，而不是已有远端版本：

1. macOS workspace 编译、Clippy 与受影响 crate 的既有及新增回归。
2. 持续 PTY 输出、local/remote 请求、慢 stdout/master 同时存在时的整循环公平性、deadline 和取消；批次/队列 helper 测试不能代替此项。
3. 多终端 paste、Tab/Insert、EndInput、断连重连与 IME 恢复，检查完整字节、控制顺序和不重放。
4. 同负载安装版冷启动、连续使用、空闲 10/60 秒和长时间放置的 `phys_footprint`、heap、owner 与恢复延迟对照。原安装版的 28.4 MiB 不属于此次 diff 的收益。

**明确保留：**native 同步 host/AX 等来源尚无统一硬上限；reader backing 未增加进程级聚合诊断；O7 watcher 仍按原条件等待事件风暴证据。没有用截断唯一候选、缩短 10 秒空闲期限、清空稳定 FontId 或强制 allocator relief 来掩盖这些边界。

## 9. 提交后复核：事件顺序回归修复

上轮 `38918d1b` 的远端 Rust / JavaScript CI 已通过，包含 workspace 测试、资源生命周期回放和 dist 构建。随后对照实施前的 `e5717811` 复核，确认以下三处是本轮新增回归；第 8 节记录的是此前源码审查时的结论，不能替代这次发现。

| 引入提交 | 回归 | 本次修复 |
| --- | --- | --- |
| `7fd63f2b` | key settlement 和 Remote Insert 同时就绪时，公平选择可能先写 Insert，越过暂存的普通输入 | 生产 selector 将 sync deadline、连接状态和按键结算放在普通 I/O 之前；普通 I/O 之间继续公平选择，handler 仍在 selector 外执行 |
| `7fd63f2b` | child exit 和 PTY 数据同时就绪时，可能先退出而丢失尾部输出 | child exit 放在普通 I/O 之后，恢复旧实现对已就绪 PTY 输出的优先级；这不是新增完整的 EOF 排空协议 |
| `4b389d76` | 输入等待 admission 时，同步焦点事件先进入 FIFO；恢复后的旧输入借用新窗口 caret 重新显示 | 输入在 hook 入口取得顺序号，跨预算与 legacy 通知等待保留；host 仅在原有身份校验接受真实焦点变化后推进失效边界。旧非空输入不再恢复，旧空输入只结束对应会话；新输入与普通 caret 保留 |

回归用例直接覆盖生产 selector、真实 outbox / socket / InsertText / PTY 输出，以及满 count / bytes 队列、已入队但未处理的焦点事件、同文新输入去重和旧空输入的会话隔离。焦点失效也包含经身份校验的 `TerminalCaret { invalidate_input: true }`；过期 focus 与普通 caret 不推进失效边界。

主代理与 GPT-6 Astra 独立复核最终 diff 后，未发现未解决的高置信阻断问题；直接 `rustfmt --check` 与 `git diff --check` 通过。移除了派发路径提取后不再使用的内部 `GpuiOverlayBuffer`，避免为测试保留生产死代码。

本次工作区修复需重新通过确切提交的远端 CI；上轮绿色结果不覆盖这些新增修改。本地不构建、不执行项目测试。安装版的切窗、输入顺序及实际物理内存收益仍需分别验收。

## 10. 0.0.8 运行版：原生引用与空闲 backing 复核（2026-10-15）

### 现场证据与解释边界

运行主进程 PID 12511 已存活约 24.5 小时，安装文件与映射中的主程序 UUID 均为 `337DFAB6-5F94-32C7-BF09-C96FC048CE6F`。连续采样的物理 footprint 为 39.0 → 39.6 → 39.7 → 39.7 MiB，峰值 75.2 MiB；期间请求数和终端会话数有变化，不是完全静止的负载。采样时 spec、hook、catalog、generator、worker pending、窗口与图标载荷均已退休；history 保留约 21.4 KiB，属于设计保留的原始历史与 NextWords，derived indexes 为零。

原生工具给出的不同口径不能相加：

- `heap` live allocations 约 14.7 MiB，两次相差 2,208 bytes；其中 non-object 约 8.1 MiB，尚无分配栈可归因。
- `vmmap` 报 malloc dirty + swapped fragmentation 约 11.7 MiB；它不等于当前可立即归还的物理内存。
- `leaks` 报 927 个 allocation / 36,928 bytes，包括 480 个 TSMInputSource / 15,360 bytes、系统 NSXPCConnection 环和少量数组。实际输入源对象数由 494 增至 510，与源码中缺失 Copy 引用释放一致，但没有逐对象分配栈，不能将每个现场对象都确定归给同一调用。
- 未见 GPUIWindow、GPUIView、GPUIPanel、CAMetalLayer；AXUIElement 两次为 201，不支持此次仍是旧 AX 泄漏的结论。

原始证据保存在本机 `/private/tmp/fastab-runtime-12511-{heap,vmmap,leaks}-20261015.txt` 以及同期 `fastab-runtime-engine-20261015-*.json`、`fastab-runtime-memory-20261015.csv`；这些是修复前快照，不是本次 diff 的收益证明。

### 本次修复

| 所有者 | 根因与修改 | 回归边界 |
| --- | --- | --- |
| Carbon 当前输入源 | `TISCopyCurrentKeyboardLayoutInputSource` 的 +1 未归还；现在立即交给 create-rule RAII owner，借用属性期间保持存活，字符串复制后释放 | 真实 TIS 对象的引用计数覆盖正常读取、提前返回和 UTF-8 解码展开；不改 IME 启停或输入源启用状态 |
| CoreText 字体属性 | feature / fallback 数组 Create +1 遗失；feature 字典还使用已退出作用域的临时 CFString / CFNumber 裸指针，并再次手动 release | 改为 CFArray / CFDictionary / CFType RAII；验证空数组、非空 feature / fallback 的引用与内容，以及真实 native font descriptor 读回。保留已有 fallback 输出行为 |
| GPUI 窗口 / 实体追踪 | 关闭窗口与 entity Drop 没有删双向索引，旧 invalidator 和集合继续被拥有；无依赖 notify 也创建空桶 | 关闭 / Drop 同时拆除两侧关系，最后窗口退出后释放表容量；每帧只删除离开的依赖，稳定依赖复用原桶，避免新增分配抖动。共享实体的其他窗口仍正常通知 |
| GPUI 线程本地元素 arena | `clear` 析构元素但保留 1 MiB 首块、后续高水位分块和两个 Vec backing，直到线程退出 | 最后窗口实际销毁后的既有 idle hook 尝试释放空 backing；活元素或嵌套借用则跳过，不调用 clear、不推进 generation。回收后首次 alloc 懒建首块，已失效 ArenaBox 不会复活 |
| Overlay 插入预测 | 插入后未收到 edit echo 就结束输入或失去焦点，`self_insertion` 仍持有最后一份完整预测字符串 | 仅当前会话 EndInput 与焦点失效清除；后台会话结束和普通 hide 保留标记，继续识别自身插入回显，避免误当 paste 隐藏 |

`IdleCacheRelease.element_arena_bytes` 是释放的 backing 容量估算，沿用现有 debug 日志，不表示 footprint 同步下降。窗口空闲期限仍为原先的 10 秒。稳定 FontId / 原生字体表、正常输入中的活元素和必要的回显状态均保留。

### 分配器调查与验证要求

这次优先移除仍有 owner 的对象和 backing，不加入周期性的 `malloc_zone_pressure_relief`。当前证据不能证明 allocator 自身泄漏，也不能保证释放引用后全部页立即还给系统。Metal renderer 退休会释放其 buffer owner，图标、engine / outbox 的表壳有较小硬上限；字体 scratch 高水位和未归因的 non-object / fragmentation 仍需在同负载安装版上比较。

回归 fixture 与真实私有路径放在同一 Rust 模块，仅由 `test-support` feature 编入；`fastab_gpui` 的 workspace 测试入口调用 keyboard、open_type、arena 与 app 的 10 个用例。普通 workspace 测试不运行 vendored GPUI 的单测，而直接 `cargo test -p gpui` 也会因非 workspace 包带 dev-dependencies 被拒绝（[Cargo 源码](https://raw.githubusercontent.com/rust-lang/cargo/master/src/ops/cargo_compile/mod.rs)）；因此不扩大 workspace 或修改依赖锁图，主 CI 检查这 10 个真实用例均实际通过。Overlay 回归由 desktop workspace 测试执行。本机仅允许直接 rustfmt 与 diff 检查，不构建、不执行这些测试。

已完成主代理与 GPT-6 Astra ultra 的逐项及整体源码复核。review 中修正了每帧拆建稳定 tracking bucket 的分配回退、native font fixture 的 unsafe 调用、非 workspace 测试入口，以及隐藏 fixture 原定义的 missing_docs lint 范围；最终未发现未解决的高置信源码阻断。受影响 Rust 文件的直接 rustfmt 检查与 git diff 检查通过，这不是编译或执行回归通过。

后续必须先取得此次确切提交的远端编译与回归结果，再对新安装版复测：输入源切换 / 键盘布局查询、Settings 与 Overlay 反复创建销毁、插入后立即执行或切窗、10 / 60 秒空闲后的 footprint / live heap / owner。重启旧运行进程只能建立冷启动基线，不能证明这份尚未安装的修复已生效。

## 11. 首次授权与 Settings 关闭后的留存（2026-10-16）

### 用户现场与三项结论

本次保留用户手动重新安装、授权、打开并关闭设置后的进程 PID 91068，没有再开设置、自动化操作界面或重启。运行身份是 0.0.9、commit `f0134e31c87213a8775103d078cc7d9a43365125`、映射 UUID `ce373a5e9e6c3e9ea9ea9d8466e5de91`。三次 physical footprint 为 30.485 → 30.501 → 30.485 MiB，peak 77.1 MiB；live heap 为 12,315,136 → 12,300,752 bytes，未呈现持续增长。IME 约 8.6 MiB，单独计量。

同版此前冷启动 PID 88449 的 footprint 是 15.782 MiB、live heap 约 4.64 MiB；两者相差约 14.7 MiB footprint，但这是不同进程、不同经历的基线，不能把差值全部归给某个对象。此前 CUA 操作引入了窗口共享指示器，本次纯手动现场没有该对象。

1. **Metal 设备引用泄漏属实。** `CVMetalTextureCache::new(None, device.clone(), None)` 使用锁定的 `core-video` 0.4.3 包装；它把拥有引用的 `metal::Device` 移入 C 参数，而 SDK 的 `id<MTLDevice>` 参数只借用、不消费。Rust `Drop` 不会在 C 返回后执行，因此每次创建 renderer 遗失一个 retain。修复只建立这条路径的引用平衡；设备仍可能被 Metal / 系统缓存持有，不能保证释放整个设备或解释全部内存差值。
2. **设置与权限窗口已释放。** 诊断窗口数为零；没有 GPUIWindow、GPUIView、GPUIPanel、CAMetalLayer、CoreVideo texture cache 或授权引导视图对象。引擎请求及初始化次数均为零。权限轮询使用弱窗口句柄，修复任务有期限，没有发现其持续持有设置 Entity。保留这些现有关闭语义，不增加定时销毁或轮询。
3. **首次绘制仍会留下进程级和系统资源。** 字体注册表、一个已停止订阅的显示时钟、AppKit 缓存以及 allocator 高水位不能等同于未关闭窗口。现场 `CVDisplayLink` 仅一个 / 1,536 bytes；17 个 TSMInputSource 在两次 heap 间稳定。最大的 448 KiB 和另一块 112 KiB bitmap 逆向引用到系统 NSCursor / AppKit 全局缓存，合计 560 KiB；不是权限引导窗口存活。没有残留 1 MiB element arena 的迹象。

`vmmap` 的 malloc dirty + swapped fragmentation 为 8,066 KiB；此前冷启动约 4,835 KiB。此统计不是可立即归还页数，不与 live heap 或 footprint 直接相加。`leaks` 的 20,288 bytes 主要是系统 NSXPCConnection 环和数组，也检测不到仍可达的设备 retain 失衡。IOSurface 三个映射合计 48 KiB 虚拟空间，resident / dirty 为零；autorelease pool 与 backing 约 22 KiB。这些证据不支持把十几 MiB 全部称作当前不可达泄漏。

原始证据：本机 `/tmp/fastab-install-scene-91068-summary.json`、同前缀的 `build-identity.json`、`heap-{1,2}.txt`、`vmmap-1.txt`、`leaks-1.txt`、`trace-{448k,112k}.txt`、`footprint-{1,2,3}.txt`、`engine-{1,2,3}.json`。都是此次修改前的安装版快照。

### 本次修复与安全边界

| 项目 | 实施 | review / 回归边界 |
| --- | --- | --- |
| Metal Device +1 | 私有 raw FFI 使用借用的 `DeviceRef` 指针；Create 输出用 create-rule owner 接管，错误与空输出明确返回 | 对照锁定依赖和本机 SDK 核对 ABI/ownership；真实设备 fixture 检查调用边界没有多余 retain，涵盖成功与错误 |
| 无用视频缓存 | 首个非空 surface batch 才创建 CoreVideo cache；普通设置/候选帧不创建 | 缓存只创建一次并随 renderer 释放；不改 Y/CbCr 纹理编码、Metal in-flight 所有权或 MSAA。真实硬件 fixture 覆盖空批次、复用与 cache +1 释放 |
| CoreText 布局 scratch | 既有最后窗口关闭 hook 调用平台退休接口；写锁内取走 ZWNJ Vec，锁外释放 | 保留所有 FontId、字体表和旧 LineLayout；真实 CoreText fixture 校验 Unicode/fallback glyph 输出、身份、回收后重建。单一 FontRun 的普通设置文本通常不分配该 scratch，收益取决于分段文本规模 |
| 子布局 ID scratch | 最后窗口关闭后取走 App 的 `layout_id_buffer` backing，重开按需分配 | 扩展现有真实 Div 布局 fixture；有其他窗口时不释放，最后窗口退出才释放，重复退休与重开都覆盖。没有现场容量证据，不宣称 MiB 收益 |

两个 scratch 的诊断只报告容量字节（`platform_scratch_bytes`、`layout_id_buffer_bytes`），不报告为物理内存下降。释放继续使用既有的最后窗口关闭流程；overlay 隐藏 10 秒期限、设置关闭行为与权限流程不变。

### 深入检查后保留的资源

- **字体 family 全面懒加载暂不实施。** 当前 CSS weight/style 匹配会枚举全部候选 face；直接改用 CoreText 选择可能改变匹配结果，且 native fallback 的 FontId 去重依赖已注册字体快照。安全实施需单独验证 embedded / variable font、features / fallbacks 与旧布局身份，不以清空稳定表换取较低数字。
- **DisplayLink 不直接释放。** 最后订阅退出已经停止时钟；CoreVideo Stop 不 join IO 线程，永久 registry 保持回调上下文有效，直接 free 会引入 use-after-free 风险。
- **不新增 `malloc_zone_pressure_relief`。** 本机 SDK 定义其为 best-effort；还没有候选包同进程 A/B 的耗时、返回 bytes、footprint 与重开延迟证据。当前 fragmentation 不足以证明强制全 zone 回收值得进入生产热路径。
- **不清理 AppKit 全局 cursor 缓存或改系统输入源。** 这些有明确系统 owner；没有观察到本轮重复增长。权限指南持有的 16-byte action target 是既有 process-lifetime 对象。

### 验证与后续验收

新增 native scratch 用例走现有 workspace 测试桥，普通 macOS CI 必须通过 11 个 ownership 用例；两项真实 Metal/CoreVideo 用例明确标记为需要 GPU 的 ignored 测试，CI 还检查它们确实被收集。Hosted runner 的 Metal 可用性尚未证明，不能把 ignored 说成执行通过。GPU 验证机使用：

```sh
cargo test --locked -p fastab_gpui resource_ownership_tests::metal_surface_cache_ -- --ignored --test-threads=1
```

本机不构建、不执行项目测试。远端须先完成这份确切修改的编译、Clippy 与普通回归，然后在 Metal 验证机执行上述两项，再对新安装版手动测量冷启动、授权/设置关闭、反复开关与 10/60 秒空闲。检查窗口/缓存 owner、footprint、live heap、重复操作趋势及重新显示延迟；不得把旧包现场、引用计数修复或容量归零直接当作新包物理内存收益。

各项已由未参与该项实现的代理独立审阅，随后完成主代理与 GPT-6 Astra 的整体源码复核，未发现未解决的代码阻断问题。review 已补齐 Metal fixture 的 `macos-blade` feature 排除，修正文档中没有证据的“平台常驻持有设备”断言，并通过受影响 Rust 文件的直接 rustfmt 与 `git diff --check`。这些检查不代替编译、测试执行或新包内存验收。

提交前复核还发现手动 Metal 工作流没有选中新 fixture，且原 `cargo test -p gpui` 受非 workspace dev-dependencies 限制。已改为 workspace 中的两个具名 CoreVideo cache 回归，并断言恰好 2 项通过；工作流命名同步收窄。该任务仅手动运行于已注册的 `fastab-metal` GPU runner，普通 push CI 仍是 11 项执行、2 项硬件用例 ignored。旧 vendor-only GPU 回归保留独立验证副本的说明，不把本次工作流描述为已覆盖它们。
