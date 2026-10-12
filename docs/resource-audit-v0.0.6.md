# 0.0.6 多维度资源与可靠性调查

调查日期：2026-10-10（Asia/Shanghai）。源码基线：`3644e59e6d5f955e853c773ce67e087a8cf264e7`。

本文前五节保存 0.0.6 基线调查和实施计划，源码行号均指该基线；第六节记录后续实现与 review/fix。本批已实现 R1–R8 及配套节能、日志和诊断改进，并按内容拆分提交；远端格式、Clippy、workspace 测试、资源回放与 dist 发布配置构建已通过，尚未发布新安装包。遵守禁止本地构建的要求；基线运行证据来自已安装 0.0.6 二进制，不能当作新代码的验收。Astra ultra 审查 engine、native UI 和交叉生命周期，GPT-6.1 Sol 处理 PTY/输入路径。

## 1. 当前安装版事实

15:53 采样的桌面 PID 为 37046，14:08:16 启动，主 app 与 IME 均报告 0.0.6。桌面磁盘二进制 SHA-256：`ed9df034ec8036b8136f05484e6c855410009e8b69b65f89b7d82ec44c119198`。

| 指标 | 当前观测 | 可以得出的结论 |
| --- | --- | --- |
| desktop phys_footprint / peak | 38.9 / 88.4 MiB | 后续再次采样仍为 38.9 MiB；不是受控版本 A/B |
| desktop live heap | 14,571,376 bytes，约 13.90 MiB | 不能把进程 footprint 全部解释为 Rust 缓存 |
| malloc dirty+swap fragmentation | 约 13.4 MiB | 是分配器区域内的未用空间，不等于全部可立即归还的物理页 |
| 规格、三类 hook 缓存、hook catalog | 全部为 0 / unloaded | 现场没有看到这些缓存未回收 |
| 历史 | 175 行、7,311 bytes | 卸载这部分对当前内存意义很小 |
| 请求 | 23 提交、20 完成、3 取消，0 failed/watchdog/panic | 与后续采样比较时应继续核对活动量 |
| GPUIWindow / GPUIView / GPUIPanel / CAMetalLayer | heap 类型表均未列出 | 当前没有存活的补全/设置 GPUI 渲染窗口 |
| NSRunningApplication / NSLock / AXUIElement | 0 / 16 / 26 | 本次没有重现此前数百个后台 NSRunningApplication 积累 |
| autorelease pool + 独占内容 | 约 26 KiB；其中直接/间接内容约 2 KiB | 前次约 468 KiB 的池持有路径本次明显不同；时长、操作量不同，不能据此计算固定版本收益 |
| 主进程线程采样 | 主循环、engine、Tokio 等均主要等待 | 这段 3 秒样本没有发现忙转；不是长期耗电测量 |
| SQLite 文件句柄 | 一个主库 FD，另有 WAL/SHM | 与空闲池收缩一致；SHM 的映射条目不是额外连接 |
| IME footprint / live heap | 约 9.0 / 1.53 MiB | 样本无 caret sender 线程；没有发现此时仍在发送或忙转 |
| 三个 fterm | 5.8、5.4、5.4 MiB | 总计约 16.6 MiB，按终端标签数相乘仍值得优化 |

正常五个产品进程合计约 64.5 MiB。没有重启、替换或向正常 desktop / IME 注入代码，也没有操作用户现有终端。

现场证据：`/tmp/fastab-006-engine.json`、`/tmp/fastab-006-engine-later.json`、`/tmp/fastab-006-37046-{vmmap,heap,sample,pools}.txt`、`/tmp/fastab-006-37050-ime-{heap,sample}.txt`。临时证据可能被系统清理，因此重要数字也记录于本文。

## 2. 优先处理项

### R1 / P1：fterm 同步解析会阻塞发送给自己的队列

**已用安装版复现，并经独立源码复核。** `fastab_term/src/main.rs:700` 的 `main_loop` channel 只有 16 槽，唯一 consumer 在同一个 select 的 :800。PTY 输出分支 :1059 同步执行 Processor，合法 `OSC 697;NewCmd` 经 `alacritty_terminal/src/ansi.rs:1159`、`term/mod.rs:802` 到 `fastab_term/src/event_handler.rs:79`，执行阻塞 `.send(SetImmediateMode(false))`。

同一读取块填满队列后，producer 等待唯一 consumer，而 consumer 必须等 parser 返回才能运行。默认空队列且 CSI-u 关闭时第 17 个 Prompt 即可卡住；有旧消息、待插入或 CSI-u 时门槛更低。分散到不同 read 且中间消费过队列的输出不一定触发。

隔离回放使用已安装 `fterm -- <python> -c ...`，独立 PTY、临时 IPC 路径与固定小输入，没有运行用户 shell 初始化。每例有硬超时并清理进程：

| 同批 NewCmd 数量 | 能看到后续输出 | 后续输入得到 ACK | 结果 |
| --- | --- | --- | --- |
| 1 | 是 | 是 | 正常结束 |
| 16 | 是 | 是 | 正常结束 |
| 32 | 否 | 否 | 4 秒仍未返回，采样主线程停在 semaphore wait |

证据：`/tmp/fterm-006-osc-probe.json`、`/tmp/fterm-006-osc-32-sample.txt`。退出后确认无该批测试进程遗留。这是终端可用性问题，不能解释当前空闲主进程的 38.9 MiB。

**实施方向：**parser 同步产生的本地控制事件进入本地待处理集合，在适当的解析边界按原序执行；外部 IPC 生产者继续使用独立有界 channel。不要用丢弃式 `try_send`，不要每条 spawn；把所有消息改成 unbounded 虽能消除该自锁，但会引入无界积压风险。需明确本地与外部事件的先后关系。

**验收：**远端测试必须驱动真实 Processor / EventHandler 与主循环，覆盖 17/32 个同批 Prompt、PreExec、CSI-u、延迟 Insert 混合，以及之后按键能继续流动；新安装版重复上述隔离 PTY 对照。

### R2 / P2：大粘贴留下多个长期缓冲的峰值容量

**源码和锁定依赖已交叉确认；安装版已测出大粘贴后的长期占用。** `fastab_term/src/term/unix.rs:237` 的外层 `BytesMut` 从 16 KiB 开始，满时 :263 reserve；Prompt 模式连续读取会不断重置 1 ms 聚合计时。解析结束 :246 仅 `clear()`。`main.rs:1027` 又将输入批次复制到跨主循环持有的 `deferred_pty_input`，成功写出后 :91 也只 `clear()`。锁定的 bytes 1.12.1 的 clear 只清长度、保留 capacity。

进一步追踪发现第三个产品所有者：`input/mod.rs:814` 的 `raw_byte_stack`。:1641 复制消费字节，:1511 用 `split().freeze()` 交出 raw 事件。bytes 的 split 将原分配变成共享 backing，parser 留下的空尾视图仍持有整块内存；即使尾视图 capacity 为 0，也不能证明底层已经释放。仅修前两处 clear 不足以解决这条持有链。

此外，Tokio 1.50.0 的 `io::stdin` 使用可复用的内部 Blocking Buf Vec，单次读取请求最多 2 MiB；数据消费后仅清长度，分配仍随 stdin 对象保留。2 MiB 是单次读取上限，不是已经测得或严格保证的 Vec capacity，内部 `Vec::reserve` 允许额外预留。需与产品缓冲分别计账；可以通过限制单次读取大小来约束增长，不宜为释放它反复重建 stdin。

已排除：此前 InputParser 的 ReadBuffer 已有大空缓冲回收；输入分支 :888 的 write_buffer、events Vec、Paste String、raw Bytes 均在批次结束 drop；跨循环 :727 的同名 Vec 是固定 16 KiB PTY 输出缓冲。正常 Unix Paste 带 `Some(raw)`，走原始字节 fallback，并不执行 `None, Paste` 分支的字符串替换。处理期间多份复制可能共同提高峰值，不能用粘贴大小简单推算进程内存。

安装版隔离 PTY 使用不回显的 Python 子进程接收合成数据，每次核对长度及 SHA-256；读取 `proc_pid_rusage` 的 phys_footprint，不使用 RSS。普通 1 KiB 粘贴两次后约 4.77 MiB。独立 2 MiB 长时对照如下：

| 阶段 | fterm phys_footprint |
| --- | --- |
| 粘贴前 | 4.75 MiB |
| 第一次粘贴完成 | 20.88 MiB |
| 第一次完成后 12 秒 | 14.84 MiB |
| 第二次粘贴完成 | 18.86 MiB |
| 第二次完成后 12 秒 | 18.88 MiB |
| 第二次完成后 60 秒 | 18.95 MiB |

两次 2 MiB 数据均完整送达，耗时约 329/332 ms。**该实测证明处理完成后仍有显著残留，不是修复收益测量；不能将全部差额逐字节归因于三个缓冲。** 第一次完成后 12 秒的下降说明短时测量受临时对象释放、异步工作收尾及分配器行为等因素影响；现有数据未区分这些来源。源码确认的缓冲持有属于高水位保留机制，两轮实验尚不能证明整个进程已进入稳定平台，也不能据此推算修复后的回收量。证据：`/tmp/fterm-006-paste-probe.json`、`/tmp/fterm-006-paste-long-idle-probe.json`、`/tmp/fterm-006-paste-2097152-long-idle-vmmap.txt`。

**实施方向：**成功 parse 或成功 write_all 后，空外层/deferred 缓冲超过 64 KiB 时换回基准缓冲；正常按键不反复分配。产出大 raw 时用 `mem::take(...).freeze()` 将整个 backing 交给事件，小 raw 保留复用，判断不能仅依赖 split 后尾视图 capacity。不能丢弃尚未完成的粘贴、EscapeMaybeAlt、pending keys 或写失败后的数据。单次读取大小、聚合最长时间/最大字节数作为单独步骤限制，同时约束 Tokio 暂存与连续输入饿死 timer 的问题。

**验收：**远端生产输入路径覆盖大粘贴、跨块 UTF-8 与 bracketed paste 结束标记，逐字节核对输出，核对全部长期 owner；大 raw 的测试需要证明 backing 不再被 parser 持有，不能只检查 len/capacity。覆盖等待拦截结果、延迟插入与写失败。新安装包按相同两轮粘贴、12 秒/60 秒取样，再给出实际收益。

### R3 / P2：反向 IPC 无界排队，正在写入的任务不能被关闭通知打断

**两路源码复核确认，尚未运行真实慢 socket 回归。** desktop → fterm 的插入与拦截控制经 `fastab_remote_ipc/src/remote.rs:123` 的无界 command 队列，再到 :60 的无界 clientbound 队列。:330 在 select 已选中的分支内部等待 `send_message`；底层 write_all / flush 没有 deadline，关闭通知只在下一次 select 才能消费。

活着但停读的 peer 可让消息及每 5 秒 Ping 继续积压。peer 只关闭写半端时，reader 虽会移除 session，:308 仍可能永久等 outgoing task。全仓核对未发现 `last_receive` 的超时读取；Pong 更新时间不是失联检测。普通进程完整退出通常会令写入失败并收尾，不属于这个漏口。

**实施方向：**限制每会话反向消息数、payload 字节及 in-flight 字节；把在途写入与 close / deadline 放在同层 select。溢出或超时退连接，不能无声丢失 Insert。队列预算要贯穿两段，避免只限制下游、上游继续积压。健康慢连接的预算应有远端实验依据。

**关联生命周期项：**`fastab_term/src/ipc.rs:162` 的本地 reader JoinHandle 在 :194 写失败后只被丢弃，Tokio 会 detach；半关闭客户端保留发送端时可能留下读任务。采用作用域读写 futures 或退出时 abort + join。正常 EOF 已有退出，不笼统称所有断连泄漏。

**验收：**真实 Unix socket fixture 停止读取、分别半关闭、队列溢出、超时、重新握手；核对所有任务退出、队列与 in-flight 预算归零，以及新 generation 不使用旧请求。

### R4 / P2：规格特殊文件可卡在 watchdog 之外

**安装版对照已复现；主要影响自定义或异常规格目录。** `fastab_engine/src/snapshot.rs:425` 在 fstat 前用阻塞 openat 打开所有目录项。无 writer 的 FIFO 会卡在 openat，无法进入拒绝特殊文件的分支，扩展名无关。

`worker.rs:360` 的首次 rebuild_engine，以及 :308 的 ClearCaches，在 supervisor 自身同步执行；attempt watchdog 在 :387 才开始。卡住时 Complete、EndInput、诊断与空闲维护都无法前进。:232 的无界 mpsc 继续收请求；取消旧 token 不会及时移除其独立 buffer/cwd/alias 载荷，环境为 Arc 共享，不能误写为每次复制整份环境。

隔离 `ftab engine complete --specs-dir`：`index.json` 加普通无关文件 36 ms / exit 0；换 FIFO 后 4 秒超时。第二次先等待 2 秒确认仍阻塞，接入 writer 后约 9 ms 退出并报初始化错误。证据：`/tmp/fastab-006-fifo-probe.json`、`/tmp/fastab-006-fifo-unblock-probe.json`。没有修改 app bundle。

**实施方向：**capture_tree 和 `open_relative` 的最终打开加 O_NONBLOCK，保留 O_NOFOLLOW 与打开后 fstat 的实际对象检查，避免路径替换竞态。正常文件变成 FIFO 的按需重开也必须覆盖。更广的慢文件系统问题仍需要给初始化/清缓存建立受监督的执行边界，并限制待处理载荷；仅加 O_NONBLOCK 不会让普通网络文件读取具有硬超时。

**验收：**真实 FIFO 首次加载、加载后替换、ClearCaches、移除异常文件后恢复；另用可控阻塞初始化检验请求合并与控制消息顺序，不只测试独立辅助函数。

### R5 / P2：文件图标冷加载在 GPUI 主线程执行同步 I/O

**两路源码复核确认，尚未制造真实失联挂载。** `fastab_desktop/src/overlay.rs:1727` → `overlay/file_icons.rs:50` → `macos-utils/src/image.rs:37` 在主线程同步执行 exists / metadata / NSWorkspace.iconForFile / TIFF→PNG。每批最多 8 个限制的是数量，不是时间。隐藏回收清 paths 后，后续补全结果需要重新解析文件图标时会进入冷路径；显式 Tab 恢复保留行时可直接复用行持有的图标。

慢 SMB、云占位文件或慢图标服务可阻塞候选、Settings 与隐藏 timer，同时放大 host 无界事件队列。当前无窗口空闲现场不能归因为这条路径。

**实施方向：**首帧使用已有 bundled fallback；图标在有界专用任务槽生成，结果按 generation/path 校验再补入。已进入不可取消同步调用的任务必须一直占槽，不能外层 timeout 后继续堆新的 blocking job。涉及 AppKit 的调用必须核对线程约束、放入同步 autoreleasepool，并只跨线程传 owned 数据。

**验收：**可控阻塞的真实生产图标 provider，验证候选首帧、隐藏、换 generation 都能前进，旧结果不覆盖新行；远端并发/取消回归后，再测安装版冷恢复延迟。

### R6 / P2：普通脚本清理顺序有 PID 复用窗口

**静态确认的罕见竞态，未通过 PID churn 现场复现。** `fastab_engine/src/process.rs:425` 的 `try_wait()` 已回收 leader，:434 仍可能按旧 pid 发送进程组 SIGKILL。触发需要后代已 setsid 离开原组但保留 stdout，原组为空、leader 已 reap，随后该号码被新组使用。原组仍有成员时不能泛称同样会复用。完整输出路径已有避免 reap 后发信号的注释，plain 路径未保持一致。

**实施方向：**非消费地观察退出状态（例如 waitid + WNOWAIT），先完成组信号，再 reap，保留现有尽快返回语义；核对平台支持，不改成无限等待后代 stdout EOF。

**验收：**远端真实子进程覆盖正常 EOF、同组/setsid 后代持管道、超时、取消及清理顺序；不在用户机器制造大量进程来追逐概率事件。

### R7 / P2：无效 UTF-8 粘贴可以直接终止 fterm

**安装版已复现。** `input/mod.rs:1509–1510` 用 `String::from_utf8_lossy` 生成粘贴事件，然后错误地按替换后 String 的字节数消费原始 buffer。原始单字节 `0xff` 变成三字节 U+FFFD；最小 `ESC[200~ + 0xff + ESC[201~` 剩余正文与结束标记共 7 字节，却在 :1641 切片 9 字节。

隔离对照中有效 UTF-8 `é` 正常完整透传；单个 `0xff` 触发 `range end index 9 out of range for slice of length 7`，已安装 fterm 退出码为 -6（SIGABRT）。发布 dist profile 为 `panic = "abort"`，因此实际结果是整个进程退出，不只是输入任务停止。证据：`/tmp/fterm-006-paste-invalid-utf8-probe.json`。常规剪贴板文本通常是合法 UTF-8，此问题主要是原始字节流的健壮性缺口。

如果结束标记后还有足够字节，可能不 panic，而把随后字节错误并入 Paste raw，破坏按键事件边界；因 raw fallback 仍可能直通，子进程收到的总 SHA 一致不能证明 parser 正确。

**实施方向：**消费长度使用原始结束标记位置 `idx + end_paste.len()`，lossy String 仅作为事件内容。与 R2 相同模块，但独立记录正确性修复。

**验收：**真实 InputParser 覆盖无后缀、`xyZ` 后缀及分块结束标记，分别检查 raw、Paste 和后续 Key 的边界；安装版隔离 PTY 再测有效/无效 UTF-8 均不崩溃并完整透传。现有大粘贴测试均为 ASCII 且忽略 raw，不能覆盖此问题。

### R8 / P2：输入 EOF 与 channel 关闭缺少完整终止协议

**源码和锁定 Tokio 实现确认，尚未做端点关闭的运行期回放。** `term/unix.rs:259` 没有 `Ok(0)` 分支。当 stdin 持续返回 EOF、且进程尚未因 child exit 或信号退出时，当前代码会反复提交 blocking read；Immediate 模式重复解析并发送事件批次，Prompt 模式重复重置聚合计时器。循环速率和实际 CPU 影响尚未实测。读取错误虽未让 reader 自行结束，但已通过输入事件传播，并在 `main.rs:1019–1022、:1153–1155` 触发主循环退出，不属于已确认的长期错误重试忙转。

单纯结束读任务还不够：`main.rs:1041` 对 input channel 关闭仅记录后继续 select，关闭 receiver 将反复立即返回。修复需把 EOF/永久错误、剩余输入处理、channel 关闭和主循环退出连成完整生命周期。真实终端完全退出时常有 SIGHUP 或子 shell 退出先终止进程，因此不能把它说成每次关标签都会忙转。

**验收：**远端实际 I/O fixture 覆盖 EOF、永久错误、剩余 partial UTF-8/转义、child 尚活和正常 child exit，断言无持续空读/recv-error 循环且所有任务退出；不要仅测试独立 `if n == 0`。

## 3. 节能、磁盘与可观测性

| 事项 | 证据与边界 | 建议 |
| --- | --- | --- |
| fterm 空闲 16 ms timer | main.rs:732、:1136 无 gate，无插入锁仍名义每标签约 62.5 次/秒定时检查；隔离粘贴回放后的空闲窗口实测约 466–468 interrupt wakeups / 6 秒（约 78 次/秒），包含 IPC 重连等其他工作，不能全归因于此 timer | 仅有插入锁时启用，状态变化时正确重置 deadline；比较 idle wakeups 和输入恢复，而非只看内存 |
| 过期 AX 激活查询 | window_server/mod.rs:400 等 250 ms 后直接同步 focused_window，当前 frontmost 检查在消费端才做；两个 Tokio workers 都可能被过期同步查询占据 | 延迟后先查激活代际；有界 blocking 槽执行 AX；覆盖快速 A→B 切换，不能每次取消就新排任务 |
| 日志运行期无字节上限 | fastab_log/src/lib.rs:86–98 只在启动检查 10 MiB，之后 File append；IME logging.rs 也仅启动 truncate。当前 fastab.log 为 0 bytes，并无错误风暴 | writer 内按字节轮转且限制文件数；默认级别及权限不变，不能用只按日期轮转代替体积预算 |
| 内存脚本可能计入诊断 shell | memory-usage.sh 按整个 command 字符串匹配。把脚本与绝对路径 ftab 查询放同一 shell 时，实测多计入 1.8 MiB 的 zsh；主进程 38.9 MiB 不受此误差影响 | 所有候选均核对 PID 对应的实际可执行映像；comm 和 `(fterm)` 标题仅用于筛选，并覆盖包装副本与旧版驻留映像；测量工具不能因命令文本含路径就当作产品进程 |
| 构建身份不足 | 已有 build hash/date 字段，但 release 工作流不保证注入；CLI 的 BuildDetails 也不证明长期驻留 desktop/fterm/IME 的身份 | CI 注入完整提交、构建 UTC 时间、架构和 run ID；各进程报告自身 PID/启动时间/构建身份，运行 Mach-O UUID 与磁盘 SHA 分别记录 |
| dump-state 解释力不足 | 当前显示 engine 已归零，但没有 UI 窗口数、输入缓冲 capacity、双向队列 bytes、在途/放弃任务等 | 添加不触发加载的有界诊断快照；显式区分 live bytes、cache estimate、allocator 与 phys_footprint |
| GPUI 硬件测试覆盖空档 | 0.0.6 普通 CI 不执行 vendored GPUI 的 ignored Metal tests | 增设远端 Metal-capable 验证入口，显式运行并留日志；正常 CI 成功不能代替这些硬件测试 |

## 4. 不宜直接实施的方向

- **不要进一步缩短 10 秒宽限作为主要方案。** 当前规格/目录/窗口已经清掉；继续缩短只会增加冷恢复，不解决缓冲高水位、任务和分配器问题。
- **不要重启 IME 追逐数 MiB。** 现有终端绑定的是 IME 进程；当前 sender 已退出，无新增增长证据。
- **不要把 13.4 MiB 碎片当作必得收益。** `malloc_zone_pressure_relief` 是 best-effort；Apple 源码对全 zone 扫描还会持全局锁。仅作为单独的远端/候选包实验，记录耗时、返回 bytes、真实 footprint 与恢复延迟。已有计划也记录过尚未建立收益，不能盲目插入每次隐藏路径。[Apple libmalloc 实现](https://github.com/apple-oss-distributions/libmalloc/blob/main/src/malloc.c)
- **暂不整体更换 desktop allocator。** fterm/CLI 已用 mimalloc，desktop 的 Rust 与 AppKit/Swift/图形分配跨多套所有权；更换 Rust allocator 不能保证原生框架开销下降。应先构造相同输入与设置开关的可重复 A/B。
- **成功 generator 的后台后代是否必须退出，先明确契约。** daemon/credential helper 可能是工具合法行为；不统一杀掉所有后台后代。
- **不把已有 watchdog 放弃旧 attempt 的边界冒充新发现。** 旧线程及 reap 等待线程在内核不可返回场景下仍可能保留资源，已有计划承认这一点；应在新增诊断中暴露数量，再决定总量限制与降级策略。
- **不删除一行 scrollback、字体 ID 映射或常驻显示器 registry。** 前两者承担功能语义；当前 DisplayLink 对象存在不等于无窗口仍在刷帧。

## 5. 建议实施顺序与验收门槛

1. **先修 R1 自锁与 R7 输入崩溃**，按问题拆提交：真实解析链回归 + 新包隔离 PTY 对照。R1 不能仅把 bounded 改成 unbounded；R7 必须校验 raw 事件边界。
2. **修 R2 大缓冲**，单独提交：字节完整性、容量回落、pending/失败路径。与 R1 复审交叉，防止事件顺序变化吞输入。
3. **修 R3 双向 IPC 生命周期和 R8 EOF 退出**，将预算、超时和任务退出一起验证；保留 generation/Insert 的可靠性语义。关闭信号需与在途 write 同层竞争；半帧写出后超时必须断连接，不能继续发下一帧。首版可保持严格 FIFO，仅去掉重复未发 Ping，不泛化合并 Intercept 增量命令。预算覆盖排队及 in-flight；两段队列须共用预算，或删掉只做同步转换的中间队列。当前 Insert 无远端执行确认，断连时不可自动重放可能已执行的插入。
4. **修 R4 文件打开及 R6 清理顺序**，按内容拆提交；后续再做初始化监督和队列合并，避免一个补丁同时重写全部 worker 状态机。
5. **处理 R5 图标冷恢复及过期 AX 查询**，先用 fallback 保证界面可推进，再异步补结果；测真实首帧和取消，不用完成时间掩盖主线程阻塞。
6. **节能、滚动日志、诊断与远端硬件测试**分别推进。16 ms timer 的改动需证明插入锁超时仍会到期，不能仅让 timer 停止。
7. 所有实现每步 review/fix，远端运行必要回归。最终用相同工作负载记录冷启动、反复设置开关、普通输入/清空、大粘贴、多标签、断连/恢复、长时空闲的 footprint、CPU/wakeups、缓存/队列/容量和恢复延迟。对已安装真实使用验收与 CI 结果分别记账。

基线调查未证明 38.9 MiB 的正常空闲现场仍有引擎缓存或隐藏 GPUI 窗口未释放。下面的修复针对已复现的可靠性缺陷及源码确认的条件性保留；实际内存收益必须另做新包对照。


## 6. 实现记录与交付门槛

### 已落地的行为

| 项目 | 实现与边界 | 主要入口 |
| --- | --- | --- |
| R1 同批 OSC 自锁 | parser 产生的控制事件进入本地集合，解析边界按顺序交给同一 `MainLoopControl`；外部事件继续独立 bounded channel。普通字节不逐次查询 bracketed mode。 | `fastab_term/src/event_handler.rs`、`main.rs` |
| R2 粘贴峰值保留 | 单次 stdin 请求 16 KiB；聚合最多 64 KiB / 首字节后 1 ms；成功处理后大空缓冲退回基准容量；大 raw 用 `mem::take` 交出整个 backing，避免空尾视图继续持有它。失败/未完成输入保留。 | `fastab_term/src/term/unix.rs`、`input/mod.rs`、`main.rs` |
| R3 反向 IPC | 删除中间无界 command 队列；每连接 256 帧 / 4 MiB，包含 in-flight；RunProcess 等待最多 256 项并回收取消/超时项。关闭、写失败或 5 秒写超时退役整条连接；半帧后不继续写下一帧，不重放可能已执行的 Insert。本地 IPC reader/writer 使用作用域 futures；远程连接以 RAII + JoinSet 管理子任务，正常关闭先释放 socket 再通知 hook，父 future 被取消也会移除会话并终止 writer/ping。 | `fastab_remote_ipc/src/outbox.rs`、`remote.rs`、`fastab_term/src/ipc.rs` |
| R4 文件及 worker | 首次 capture 与按需重开都用 `O_NONBLOCK + O_NOFOLLOW + fstat`。初始化和 ClearCaches 进入监督边界。等待队列最多 256 jobs / 8 MiB 估算载荷，清除已取消 Complete，控制消息保持 FIFO。为已提交资源 owner 和运行中 attempt 的 EndInput 保留两槽。 | `fastab_engine/src/snapshot.rs`、`worker.rs` |
| R5 文件图标 | 首帧用 fallback，单个 std worker 异步读取；不可返回的原生调用持续占槽；仅保留一个最新 pending batch、每批最多 8 个路径；隐藏/换代后的结果不能补回 cache。AppKit 同步函数内有独立 autoreleasepool。 | `fastab_desktop/src/overlay/file_icons.rs`、`macos-utils/src/image.rs` |
| R6 脚本退出 | `waitid(WNOWAIT)` 非消费观察退出，先完成组信号，再 reap；EINTR 不跳过 deadline。保留现有脚本语义，不统一终止合法后台 daemon。 | `fastab_engine/src/process.rs` |
| R7 无效 UTF-8 | 按原始 `idx + end_paste.len()` 消费，不使用 lossy String 长度；后续按键与 Paste raw 边界保持独立。 | `fastab_term/src/input/mod.rs` |
| R8 输入结束 | EOF 和永久读错先 finish parser，再终止 input channel；主循环先结算 pending keys，处理 raw/deferred/insert，随后退出。永久错误不再遗漏结算；部分 UTF-8/escape 原字节保留。 | `fastab_term/src/term/unix.rs`、`input/mod.rs`、`main.rs` |
| 空闲 timer | 16 ms 重试只在 insertion lock 存在时活跃；重复输入不延长已有检查期限，锁超时仍能解除。 | `fastab_term/src/main.rs` |
| AX 激活查询 | 一个 std worker + Condvar，250 ms 合并到最新激活；查询前后校验 frontmost/generation；阻塞时不额外生成 worker，空闲退出。 | `macos-utils/src/window_server/mod.rs` |
| 日志体积 | std-only writer：active 10 MiB + 两份 backup；单次 writer record 最多 64 KiB；0600；通过 sidecar flock 协调本版本的跨进程轮转，争用时丢该条日志，避免阻塞产品线程。每次重开 active，避免另一进程轮转后仍写旧 inode。旧版进程直接写日志不受此协议约束。 | `fastab_log/src/rolling.rs`、IME `logging.rs` |
| 测量工具 | command/title 只筛候选，所有 PID 再核对映射的实际可执行文件与启动时间；保留旧版驻留映像和 fterm wrapper 支持；无法核实的活进程提示 unknown 并排除。 | `scripts/memory-usage.sh` |
| 构建身份 | CI/打包注入 commit、UTC 时间、target、run ID；desktop/fterm 返回自身 PID、应用入口时间和映射 Mach-O UUID。IME 启动写一次 `imk-identity.json`，读取时必须核对 PID/启动时间，文件本身不是进程仍存活的证明。 | `fastab_util/src/build_identity.rs`、`scripts/build-app.sh`、IPC diagnostics |
| 资源诊断 | Engine 返回队列条数/估算字节、实际 active/abandoned operation 数；host 返回窗口/图标工作/反向 IPC 预算；fterm 返回输入与 deferred 数值快照。查询不初始化规格或图标。 | `fastab_engine/src/diagnostics.rs`、`gpui_host.rs`、`fastab_term/src/resource_diagnostics.rs` |
| Metal 验证入口 | 单独手动 workflow，要求带 `fastab-metal` 标签的远端 macOS ARM64 GPU/GUI runner，显式执行 ignored renderer 测试并拒绝零测试通过。尚未确认此 runner 可用。 | `.github/workflows/metal-tests.yml` |

### Review/fix 中另外处理的问题

- 满队列不能吞掉结束输入通知，否则本轮资源仍可能没有释放期限。EndInput 只在确认该 session 不可能拥有资源时省略，同 session 去重不跨保留的 Complete。
- `active_operations` 与 `abandoned_operations` 在同一锁内快照，避免读出不可能的组合；ClearCaches 超时计入 watchdog，不冒充失败的 completion。
- 队列满时，acceptance 不在 GPUI 调用线程回退到同步 SQLite 写入；内存排名仍更新，持久化保持 best-effort。
- Insert 入队失败清除本地预测；Intercept/Visible 必须均成功入队才更新本地状态，否则关闭连接，避免桌面与终端状态分叉。
- macOS 没有 packaged bundle metadata；构建身份接口显式支持 `Option<String>::None`，返回可选 JSON 的协议类型保持正确。非法已有 metadata 报错，不静默替换为 null。
- EOF 的待定键测试区分“已写出但尚未 drop 的 frame”和“被取消且未写出的 frame”；不能把已写出的键再次重放。

### 回归范围与已经完成的验证

新增回归在现有 Rust tests 模块及新生产模块内，未添加空壳测试文件。关键 fixture 使用真实 Unix socket、FIFO、子进程或受控 std worker：

- 终端：真实 `Processor → Term → EventHandler → MainLoopControl`，输入 EOF/永久错误/持续流、无效 UTF-8 后缀与分块、大 raw backing、成功/失败写入容量、pending keys 结算与 insertion timer。
- IPC：在途计费/溢出/过期 response/FIFO 编码、停读导致超时、半帧关闭、旧 sender 退役、本地半关闭与 reader 生命周期；实际握手后 hook 阻塞期间关闭、cleanup hook 阻塞仍收到 EOF、父 task abort 后会话/RPC/预算归零。
- 引擎：首次及替换后的 FIFO，初始化/ClearCaches 监督、饱和队列和 EndInput 顺序，真实同组及 setsid 后代持 stdout。
- 原生：阻塞图标 provider 的单槽/最新 pending 行为，过期结果隔离，快速 AX 代际变化与 worker 重启；真实文件轮转/多 writer/权限/超长记录。
- 构建身份：JSON 转义、当前 PID/Mach-O UUID；macOS 无 packaged metadata 的真实 handler。

**已完成：**逐模块源码 review/fix 与交叉审查；直接 `rustfmt --check`、`git diff --check`；修改的 shell 脚本 `bash -n`、ShellCheck，以及改动的 TOML/lockfile、CI YAML 语法解析。新版内存脚本在本机做了只读验证：诊断 shell 命令文本含 ftab 路径时未被计入。测到 desktop 38.1 MiB、IME 9.0 MiB、两个 fterm 5.8/5.1 MiB，合计 58.0 MiB；进程数量和基线不同，且仍是旧安装代码，不能报告为本轮收益。

**远端 CI 补充（2026-10-10）：** `87fb32a7` 的 [CI 38042674452](https://github.com/codeime/Fastab/actions/runs/38042674452) 整体结论为 success：JavaScript 全 job、Rust fmt、Clippy、workspace tests、engine resource replay 及四个二进制的 dist 编译全部通过。引擎为 517 passed / 0 failed / 2 ignored，原失败测试与两条新增 checkpoint 回归均实际执行通过。此前失败逐项处理：

- `c3b8781e`：Mach-O 遍历 offset 显式使用 `usize`，修复 `checked_add` 类型推断失败。
- `cd69598a`：日志格式错误保留为 `io::Error` 的 source，满足 `map_err_ignore` 规则。
- `71f8852f`：终端测试通过 cfg(test) fixture 创建 disconnected sender，生产构造函数保持原有可见范围。
- `387af13a`：首次 attempt 超时曾丢失已经加载成功的 registry；现在用 attempt 独立的单槽 checkpoint，在 Engine 构造及补全前交回 pristine registry。接收端随 attempt 结束关闭，旧任务迟到不能覆盖新代际。原 CI 失败测试保持不变，新增构造前超时恢复、ClearCaches 后迟到 checkpoint 隔离回归。
- `87fb32a7`：workspace tests 使用 `--no-fail-fast` 收集全部包失败，保留非零退出及失败门槛。

**尚未完成：**Metal tests、安装版 A/B。没有本地构建。CI 回归通过不等于已测得新安装版的内存降幅。

### 远端验收与真实使用对照

1. 本批改动按 engine、IPC、终端输入、native worker、日志、诊断/CI、文档拆为独立提交。推送 main 后运行仓库 CI（也支持手动触发）。CI 完整执行 workspace fmt/clippy/tests 与 engine resource replay。重点测试包为 `fastab_term`、`fastab_remote_ipc`、`fastab_engine`、`fastab_desktop`、`fastab_log`、`fastab_input_method`、`fastab_util`、`macos-utils`；不能用仅筛选新测试代替现有回归。
2. 远端 Metal workflow 需匹配的 GPU/GUI runner；缺少 runner 必须记录未验证，不能用普通 macOS CI 成功替代。
3. 候选包核对各运行进程的 commit/UUID 与实际磁盘文件身份，再用相同隔离 PTY 负载对照 R1/R2/R7/R8：两次 2 MiB 粘贴、12/60 秒取样、无效 UTF-8、同批 32 OSC、EOF/child exit。核对字节/hash/事件边界和全部任务退出。
4. 普通使用覆盖候选显示→Tab→清空、设置反复开关、快速终端切换、多标签、停止读取的 socket/重新连接、冷文件图标及长时空闲。记录 phys_footprint/peak、CPU/wakeups、队列/窗口/缓存数、恢复延迟，分别报告主进程和每个 fterm，不仅给合计。
5. 不可返回的 OS 调用仍可能留下 watchdog abandoned thread；本次准确计数并限制队列/原生图标槽，没有证明可强杀线程，也没有引入全局降级阈值。数值 capacity/估算载荷不等于物理页；raw view capacity 不代表共享 backing 全大小。同步 SQLite 持久化仍位于 supervisor，慢磁盘极端场景需后续独立证据。强制 abort 无法执行异步生命周期 hook，正常关闭仍按原顺序通知；abort 的保证是会话、socket、队列及子任务不遗留。

## 7. 2026-10-12 复核与性能跟进

本节源码基线为 `921cb894`，记录后续工作区修改。没有本地编译或执行 Rust 回归，也没有替换、重启安装版。下面的运行数字属于旧安装版，不是新修改的性能验收。

### 当前安装版内存证据

10:38（Asia/Shanghai）读取同一 desktop PID 37046，启动时间仍为 10 月 10 日 14:08:16：

| 指标 | 本次观测 |
| --- | --- |
| desktop phys_footprint / peak | 38.1 / 88.4 MiB |
| heap live allocation | 14,447,920 bytes，约 13.78 MiB |
| malloc dirty+swap fragmentation | 约 13.2 MiB，不是可保证归还的页数 |
| engine registry / hook results / hook catalog | 全部为空或 unloaded |
| history | 175 行、7,311 bytes |
| completion submitted / completed / cancelled | 23 / 20 / 3，与前次记录相同 |
| AXUIElement / NSLock | 37 / 16 个；未列出 GPUIWindow、GPUIView、GPUIPanel、CAMetalLayer |
| IME / 两个 fterm footprint | 9.2 / 5.8 / 5.4 MiB |
| 四个进程合计 | 58.5 MiB |

证据来自 `scripts/memory-usage.sh --peak`、已安装 CLI 的 `dump-state engine`、`vmmap -summary 37046` 和 `/tmp/fastab-20261012-37046-heap.txt`。本次未见 engine 缓存残留，且这段时间请求数没有增长，不能据此证明活跃输入负载下没有增长。仍不加入未经新实验验证的 allocator 强制回收，也不把静态资源计数等同于物理内存降幅。

采样期间另确认一个诊断错误：沙箱拒绝 `ps` 时，旧脚本因 process substitution 未传播失败，仍输出“No Fastab processes running”。现在显式传播失败并报告 unknown；这与成功枚举后确实没有进程分开处理。

### 已实施的热路径优化

| 路径 | 原有重复工作 | 本次改动及语义约束 |
| --- | --- | --- |
| `fastab_engine/src/rank.rs` | 排序每次比较重复计算两行的大小写/模糊匹配分数，遍历历史 alias 和 acceptance | 单次排序使用 cached key，每候选至多评分一次；保留 auto-execute、逐行 query term、匹配桶、优先级、alphabetical、稳定平分及现有小列表去重。临时 key 会增加紧凑 scratch，未宣称该项降低峰值内存。 |
| `fastab_engine/src/filegen.rs` | 收集、排序全部匹配目录项，最后只取 49；文件名比较逐字符分配 String | 保留最多 49 项的 max-heap，扫描序作为稳定并列顺序；最终仅排序保留项。文件名小写用字符迭代，不改成有上下文规则的整串 Unicode 小写。过滤、隐藏项顺序、目录后缀、symlink、单独的 `../` 条件和其预留槽不变。 |
| `fastab_gpui/src/overlay.rs` | 比较新旧列表及恢复选中项时，为每行复制四个身份字段 | 借用字段比较；替换旧列表前查得新索引，保留选中项消失、无旧选中项和隐藏三个分支语义。公开 owned identity API 保留。 |
| `fastab_gpui/src/list.rs` | 每次 render 将全部同类显示名复制为小写字符串数组，再建立引用数组求前缀 | 只持有一个前缀字符串，逐字符 ASCII 不区分大小写比较，在 UTF-8 边界截断，前缀为空即结束。沿用类型和父目录过滤，不用 suggestions revision 缓存显示名。 |

目录项为 D 时，候选保存从 O(D) 降至最多 49 项，仍需遍历整个目录；排序评分次数从通常随 O(N log N) 比较增长改为 O(N)。这是源码复杂度和所有权变化，尚无新安装版耗时或物理内存对照数字。最终 suggestions 没有全局条数/字节 cap，不能将单源 49 项上限解释为整个补全列表上限。

新增回归保留在现有模块：ranking 使用旧比较器核对 Unicode、alias、query override、scoped/global acceptance 和稳定顺序；filegen 对照旧全排序，覆盖 2,000 项候选与 320 项真实目录、Unicode 和隐藏项；GPUI 覆盖 Unicode 字符边界、ASCII-only 折叠、单匹配及显示名更新，复用现有选择/滚动回归。

### CSI-u 修复与审查边界

条件回归只在 `qterm.csi-u.enabled=true`（默认 false）时触发：同一 PTY read 包含退出备用屏幕与 NewCmd，旧 HEAD 会先发送启用 CSI-u，再转发原始退出序列，将模式 push 到错误屏幕。该问题不能解释默认配置下的普通 Tab 或内存问题。

修复先转发事件之前的原始输出，再及时清空本地控制队列；只有 CSI-u 输出等待完整边界，不为每个普通字节调用 stdout write。VTE 在 OSC 的 ST 首字节 ESC 就派发事件，不能在 ESC 与反斜杠之间插入控制，因此边界标记跟随实际 parser 回调，也覆盖未完成 UTF-8 和同步输出缓冲。

独立 review 又发现，若所有事件都等待完整边界，连续未终止的 OSC 链会让本地队列无界增长。最终仅用 `desired` / `owned` 两个 bool 保留键盘模式请求：重复启停幂等，在最早安全边界发送差异，完整写入且 flush 成功后才更新 `owned`。它表示 Fastab 尚未配对退出的一次 push，不宣称跟踪终端的实际模式栈；输入编码继续读取 `owned`。正常 shell 的 PreExec 在应用切屏/push 前退出，NewCmd 在应用返回后进入。

回归检查完整输出字节序列，包括 BEL/ST 的全部两段切分和逐字节 read、应用 push/pop 与备用屏幕交接、重复启停、延迟 prompt、UTF-8 和同步 replay，以及 2,048 次未终止 OSC 链且每次 read 后队列为空。对于缺失终止符、直接被跨屏序列中止的畸形 hook，只保证原始字节不被插断和控制状态有界，不保证恢复任意外部程序改写的键盘模式栈。

### 本轮检查结果

CSI-u、engine 排序/目录扫描、GPUI 热路径分别经过独立 Astra 审查，发现的问题已修正，最终实际 diff 无新增发现。六个修改的 Rust 文件直接 `rustfmt --check`、`git diff --check` 和诊断脚本 `bash -n` 通过；脚本实际验证了进程枚举被拒绝时非零退出、可访问时正常报告。没有执行 Rust 测试或本地构建，shellcheck 也因本机无可用版本未执行。

### 待完成的验证与研究项

- 远端 CI 执行 `fastab_term`、`alacritty_terminal`、`fastab_engine`、`fastab_gpui` 现有及新增回归，以及 workspace fmt/clippy。当前工作区的测试源码不代表测试已经通过；前一批 CI success 不覆盖本节修改。
- 候选安装包用相同 50/500/10,000 行 ranking、大目录补全和候选重绘负载记录耗时、分配/footprint、恢复延迟；保持原始结果顺序与内容一致。对照须核对运行进程身份。
- Apple Terminal/iTerm 通用 AX caret 路径仍是 GPUI 前台同步查询，缺少 xterm 的总预算和失败退避。这次没有捕获到该路径变慢的运行证据，暂不改变 AX timeout，后续应先取得同一终端操作的主线程采样再决定改法。xterm 已有预算/失败退避，不重复实现；标题布局也已有 GPUI 帧缓存，不能把每次 layout 调用计为全新 CoreText shaping。

## 8. 按需执行专项研究（2026-10-12）

本节保留第七节之后的研究依据与实施方案，**实施结果和审查修复见第九节**。研究检查对象为 `921cb894` 加当时未提交的优化；本节行号是研究时位置。主代理核查展示需求、设置刷新和 host 队列；Astra ultra 核查 engine，GPT-6.1 Sol 分别核查 native UI/AX 与 fterm/IPC，并对设置与队列结论独立反证。没有本地构建、执行测试、重启或替换安装版，也没有新增运行时收益测量。

优化目标分成三类：开始前确认有消费者；执行中消费者消失则取消；结束后仅保留恢复所需状态。缓存已经有数量上限，不代表每次填充都有必要；窗口已经隐藏，也不代表所有后台工作都停止。下列优先级指实施顺序，不是缺陷严重等级。

### D1：隐藏时不生成候选，同时保留首次 Tab 的入口

**已确认的缺口。** `fastab_desktop/src/overlay.rs:785` 在 onlyShowOnTab 下设置隐藏后仍继续提交普通补全；Esc 的 `suppress_until_shown` 也只限制展示。`:994` 还允许隐藏结果的 pending generator 继续 debounce。空白输入、重复输入、无 caret、粘贴/历史召回后的单次抑制已有门控，不能说所有隐藏状态都在扫描。

**不能只添加 return。** `overlay.rs:594` 的 global intercept 依赖 `has_items && has_position`，`fastab_term/src/interceptor.rs:240` 只有开启 global intercept 才识别隐藏状态的 Tab。完全不生成行后，首次 Tab 可能绕过 Fastab；同时停止隐藏 caret 查询又可能形成“Tab 等 caret、caret 等 Tab”的循环。`showAutocompleteFromTab` 在只有一行时立即插入（`overlay.rs:1105`），而显式 Show 只展示；两者必须继续区分。

**方案。** 把“存在可以显式请求的输入”与“存在可接受的结果”分成独立状态。隐藏时只更新最新输入和上下文，不提交 Complete 或续跑 debounce；启用补全且有适用输入时，显式 Show/Tab 的请求入口同时独立于已有 rows 和已有 caret。接收意图后，缺 caret 则主动进入 WaitingForCaret 并请求位置，拿到当前 session/cursor/input generation 的结果和有效 caret 后再展示或执行原有 Tab 行为。普通自动补全保持即时。空结果、失败、取消、无 caret、切换 session 时必须结算该意图，不能永久吞掉 Tab，也不能把保留的旧行直接插进新输入。

**第二阶段。** 当前隐藏十秒后刻意保留非空 rows 和其 PNG（`fastab_gpui/src/overlay.rs:304`、`overlay/file_icons.rs:351`），用于恢复。先完成上述按需恢复协议，再考虑长期隐藏仅保留输入、选中身份等最小状态，按现有宽限释放行 payload；不能先清 rows 再发现 Tab 入口失效。该方向可能减少隐藏期间持有量，代价是下一次 Show 的冷生成延迟，不承诺固定 MiB。

**验收。** 真实 `FigtermCommand::InsertText` 路径覆盖首次 Tab、多/单/零候选、危险/自动执行行、连续 Tab、Esc 后继续输入、过期单行结果、caret 丢失恢复和跨终端切换。隐藏打字时 Complete 提交数应不增长，显式 Show 后必须能恢复。

### D2：过期的目录扫描尽早停止

**已确认。** `fastab_engine/src/filegen.rs:60` 按请求打开目标目录，不递归；`:66` 循环没有 cancellation 检查。外层 `generate.rs:340/400` 只在生成器前后检查取消，因此旧请求可能扫完整个目录才退出。49 项 heap 限制保存量，不限制目录遍历次数。默认相同路径 query 复用结果，prefix 改变会重扫（`generate.rs:577`）。

**方案。** 在打开目录前、取下一项前和昂贵步骤前检查当前 attempt 取消；生成器执行完脚本后、开始文件模板前再检查一次。取消返回必须沿用整个 attempt 的取消结算，不能发布半份结果或更新 generator cache。保留完整扫描所需的排序语义，不能读到 49 项就停。普通本地迭代可协作取消，但一次已经阻塞的 `read_dir`/metadata 系统调用不能靠布尔检查中断；慢挂载仍受现有 attempt watchdog/占槽约束。

**验收。** 真实目录迭代中途取消，确认后续项不继续消费、未发布部分结果，新请求可接替；未取消时与旧完整排序结果一致。应记录 entries visited、cancel-to-finish 和峰值候选数，不能只断言空结果。

### D3：历史数据只构造有消费者的部分

**D3a：删除无人使用的统计表，优先级高、边界小。** `rank.rs:258` 为每条历史构造完整命令、首 token、前两 token 的 `Frecency.stats`，独立复制字符串（`:275`）。唯一读取是 `score()`（`:297`），本次全仓 Rust 调用核查没有消费者；`rank` 为私有模块，Frecency 未公开 re-export。当前排序在 `apply_with_acceptance` 使用 `NextWords`，不消费这张表。可以删除 stats、专属 bump/score，保留行 intern、NextWords 和 acceptance 逻辑。收益随历史规模变化；第七节的小历史样本不支持推算大的常驻降幅。`HistoryStore` 的诊断 bytes 也不能当作所有 Frecency 辅助结构的总量。

**D3b：generator cache 命中时不要先复制 history values。** `lookup.rs:2235` 只要当前参数含 history template，就先执行 `HistoryStore::arg_values`；`history.rs:269` 为去重和返回值分别复制字符串。随后 `generate.rs:386` 可能仅 refilter 已缓存 generator rows，完全不用刚创建的 values。例如纯 history 模板的 `wscat -c w` → `wscat -c ws`，没有 trigger/debounce/path 模板时即走此路径。应将 values 改成惰性 provider，仅实际执行 History generator 时取值，同一请求多个消费者共用一次结果。

**约束与验收。** 核对完整候选次序、重复历史权重、slot 去重顺序、alias/shell 上下文、debounce 与多 generator。构建计数应证明 cache hit 不调用 arg_values。不能用 `include_history=false` 一刀切跳过全部历史，因为它只控制历史行合并，普通候选排名仍使用 NextWords。已有索引缓存、逐历史行取消继续保留。

### D4：没有实际 hook 调用时不解析整个 catalog

**已确认。** `runtime.rs:421` 首次构造加载 NativeHooks；闲置卸载后，普通请求在 lookup 前又无条件 `ensure_native`（`:811`）。单独输入 `g` 的首 token 分支不需要 hook，也可能付全 catalog 加载成本。descriptor 表达式树本来就按需解析，不能重复报告为“每次解析所有 hook”。安装版 sidecar 约 4.5 MiB 是文件体积，不是预期内存收益。

**方案与必要配套。** 按 snapshot generation 绑定惰性 owner，在第一个 `dispatch_*` 初始化。`snapshot.rs:618` 已为避免二次读/hash 截留 sidecar 原始 bytes；只删 eager load 会让 bytes 留在共享 snapshot，可能增加常驻量。首次请求使用 hook 则消费 captured bytes；请求结束仍未使用则释放原始 bytes，以后需要时按同一 snapshot digest 校验重读。初始化失败的“已加载但缺失/拒绝”与“未加载”不能混为一谈，取消/idle deadline 和跨 generation 一致性不能退化。

**验收。** 冷首 token/纯静态补全 catalog 初始化数为零，且没有未释放的 captured bytes；首次真实 hook 正确加载；sidecar 缺失、替换、拒绝、取消和空闲恢复行为不变。此方案会把第一次 hook 的成本延后，部分路径多读一次，列在低风险改动之后。

### D5：只为真正需要系统图标的可见行加载

**已确认。** `overlay.rs:1748` 对全部最终 suggestions 做 lookup；`overlay/file_icons.rs:99` 取前八个未缓存 file/folder，与 viewport 无关。`fastab_gpui/src/list.rs:1432` 优先显示有效 `icon_identifier`，但这些行仍可能生成永远不会显示的 native PNG。GPUI 的 `uniform_list` 已只渲染可见 range，不存在这里的“全列表渲染”问题。

**分两步。** 先用与 renderer 共用的纯判定排除有效显式图标；不能仅检查 `icon_identifier.is_some()`，未知 identifier 仍需要 fallback。再由实际可见 range/选中与滚动变化驱动 native 请求，立即显示 bundled fallback。保持单 native worker、每批预算、最新 pending 和 generation 检查；正在执行的同步 AppKit 调用仍占槽。隐藏后的新 batch 已有 visible gate，不能说当前隐藏会无限调用 NSWorkspace。

**验收。** 有效/未知显式图标、初始 viewport、向下滚动、快速切换目录/隐藏；provider 计数证明未展示/被显式图标覆盖的行不触发 native 查询，迟到结果不覆盖新行。隐藏十秒仍由 rows 引用的 decoded PNG 是实际持有路径，但不能将少查几个图标等同于固定 footprint 降幅。

### D6：caret 查询也服从补全需求

**已确认。** `remote_ipc/mod.rs:142` 每个非空 buffer 独立发布 EditBufferChanged；host → `platform/macos.rs:560` 刷新位置，`:1112` 无全局/单终端 disable 或 overlay demand gate。即使 completion 入口已返回，仍可能走 AX 或发送 IME 通知。已有前台 terminal 筛选、window/leaf 缓存、xterm 预算/失败退避、PID/window/epoch 校验，不能把它描述为没有任何保护。

**方案。** 先跳过明确禁用的按键驱动 refresh；然后与 D1 共用需求状态：自动展示、显式 Show 或 WaitingForCaret 才请求位置。不能直接用 `visible`，否则首帧需要 caret、caret 又等待首帧会形成循环依赖。暂停时继续维护 focus/session 身份失效；恢复请求新 caret，不复用另一窗口的位置。

**验收。** 全局禁用/单终端禁用时查询计数不增长，重新启用、首个字符、Show、切窗后正确恢复；覆盖 Otty/IME 路径和 xterm AX 路径。收益主要是输入期间主线程时间，不是空白终端每次光标闪烁都会走 AX。

### D7：设置变化按影响范围刷新，并合并重复通知

**已确认并独立复核。** `settings_ui.rs:144/152` 的设置写入都发送 ReloadCredentials；`file_watcher.rs:82` 随后的文件 Create/Modify 可能再次发送，相同内容没有短路。`gpui_host.rs:89` 在启用状态调用强制 recomplete。因此语言、字体、静默启动这类变化，在仍有可用输入/caret 时也会提交补全；不能写成“每次改设置必扫描”，因为空输入/无 caret/结束输入仍有门控，generator 也可能命中缓存。

**方案。** 传递变化 key 或比较设置快照：展示设置只 apply/notify/relayout，补全语义 key 才重算；文件事件加载后等于前快照则跳过重复业务刷新。保留外部 ftab 写配置的通知，并对 UI 直接通知与 watcher 回声使用同一版本/内容去重。写失败不能发布一次虚假的成功刷新；失败反馈作为实现时的边界一起处理，不扩展设置业务。

**验收。** 字体/语言变化更新 UI 且 Complete 计数不增加；fuzzy/history/sort 等确需刷新；UI 写入加 watcher 回声只生效一次；外部写入、删除 key、快速连改和真正结束输入均正确。

### D8：在 host 队列入口减少已经过时的输入快照

**源码确认的条件性积压风险。** `event_loop.rs:58` 是 unbounded；`remote_ipc/mod.rs:132` 为每条输入复制 buffer/cwd 入队，随后另发 caret refresh；`gpui_host.rs:408` 逐条处理。fterm 按 PTY read 批次取 buffer、其 outbox 有界、overlay 重复输入/WaitingForCaret、engine mailbox 均有保护，但没有替代 host 队列的合并。主线程被慢 AX 等工作占据时，旧文本和无用刷新可能排队；本轮未测得实际积压量，不将其归因为当前空闲内存。

**方案需要额外设计。** 先把带 session 的 buffer 与对应 refresh 意图作为一个可排序单元；只对确认可替换的连续输入合并。保留 Tab/Insert/EndInput、focus/session 变化和真实 caret 结果的顺序屏障；空白 buffer 自身会结束输入，也必须作为隐式 EndInput 屏障，默认所有非明确可替换事件均阻断合并。两次 send_event 来自多 producer，不能假定始终相邻。还要保留 typing、paste、backspace 与 self-insertion 的来源语义，否则合并连打后可能被误认为粘贴并隐藏列表。可先从相同输入/重复 refresh 去重起步。

**验收。** 控制 consumer 暂停，量化消息数和 payload 字节；输入/Tab/Insert/EndInput/切窗交错，以及删空后立即重新输入，最终文本、插入次数及抑制规则一致。不能直接换成阻塞 bounded.send，同线程回调可能重现“给自己发满队列”的死锁；不能无声丢弃控制消息。优先级低于边界清楚的惰性构造改动。

### D9：desktop 不在线时，不构造发不出去的完整 shell context

**已确认。** `fastab_term/src/event_handler.rs:85/118/132` 的 Prompt/PreExec/CommandInfo 先调用 `shell_state_to_context`，深复制 env、alias 和 context，直到 `:67` 才检查 ready sender。desktop 退出但旧 fterm 继续运行时，这些临时数据被直接丢弃；edit-buffer 路径已提前检查，不重复修改。

**方案。** 发送辅助函数接收构造闭包，先取 ready generation 再构造消息，最终入队仍校验 generation。Prompt 的 cwd 更新、本地控制、pending insert、CommandInfo 历史保存和 UpdateShellContext 不能停，确保重连后第一次补全状态正确。检查后刚好断连仍可白做一次构造，接受这个有限竞态。

**验收。** 断连时构造计数为零，但本地控制/历史正常；更新 env/alias 后重连，首个 context 正确；旧 generation 消息不可入队。主要减少断连后使用终端时的临时分配，完全空闲无事件时不会有这项收益。

### D10：空闲 IPC 维护按期限唤醒，心跳兼容单独处理

**既有事实的新节能方向。** `fastab_remote_ipc/src/remote.rs:325` 每连接五秒 maintain 加 Ping，fterm `message.rs:508` 回 Pong，desktop 更新 last_receive。本仓 last_receive 无判活消费者已在 R3 记录，不能算新泄漏。maintain 清理的是 pending RPC 取消/期限；可按最早 pending deadline 安排 timer，无 pending 不设维护 timer。

**协议限制。** 不由本仓没有读端推导所有旧 peer/转发链都不依赖 Ping。拆出期限维护后，停发或降低心跳需双方协商；旧 peer 保持既有策略，保留 Ping/Pong 接收。移除探测会改变部分半关闭连接发现时机，不能偷偷改变可靠性。现有写 deadline、outbox 预算与退役机制保留。

**验收。** 新旧 peer、转发链、早期限插入、取消、超时、半关闭、停读、重连和首次 Insert/Tab。当前名义每连接每分钟 12 次往返，收益重点是多标签 idle wakeups，未测得实际节能/内存值；列为后续实验。

### 实施顺序与统一验收

1. **第一批：边界小且可独立验证。** D3a 无用 stats、D3b history 惰性 values、D5 有效显式图标跳过、D9 断连惰性 context；D2 目录取消单独处理，保持原有完整排序。
2. **第二批：需求状态统一。** D1 显式 Show/Tab 协议与 D6 caret demand 联动；D7 设置影响分类与去重。D5 viewport 驱动、隐藏 rows 进一步释放建立在恢复协议明确之后。
3. **第三批：复杂或需兼容实验。** D4 catalog 与 captured bytes 共同设计；D8 host 输入合并；D10 心跳/维护拆分。先做可观察性与受控回放，再决定是否实施。

验收同时记录 work avoided（扫描项、history provider/catalog/native icon/AX 调用、context 构造、Complete 提交、队列字节和唤醒）与真实 phys_footprint/恢复延迟。没有需求时计数不增长，需求回来时第一次操作正确，二者都必须成立。源码回归在远端 CI 执行；已安装旧包的数字不用于证明新方案收益。


## 9. 按需执行实施与逐项审查（2026-10-12）

本节记录 D1–D10 提交前的实施和审查证据。每项完成了实施者自审与独立源码复核；D1/D6 由 Astra ultra 实现、主代理与 GPT-6.1 Sol 复核，D4 由两名独立 Astra ultra 分别实现和审查，D5/D8 由 Astra ultra 复核，D2/D3/D7 由 GPT-6.1 Sol 交叉复核，D9/D10 由主代理复核。下表的回归均为现有 Rust 模块中的测试源码，**没有在本机编译或执行，不能称为测试已通过**。

| 项目 | 实施结果 | 审查与回归重点 |
| --- | --- | --- |
| D1 隐藏需求 | onlyShowOnTab/Esc 隐藏时停止 Complete 与 debounce；Show/Tab 保存当前输入意图，主动请求新 caret；10 秒后释放隐藏 rows、文本容量、PNG，仅留选中身份。 | 真实 FigtermCommand::InsertText 路径覆盖冷 Tab、安全单行、多行/空结果、过期结果、连续 Tab、超时及切换输入；复核修复重新启用时没有恢复 global Tab 拦截入口；继续输入取消已显示 loading 的显式请求时，统一走完整隐藏/清 loading/停放窗口/10 秒回收。ShowIntent 直接拥有超时任务，结算即取消，避免长 scriptTimeout 下积累已失效计时器。 |
| D2 目录取消 | read_dir 前、next 前及分类/过滤前检查取消；脚本结束到模板执行间增加取消边界；取消丢弃全部部分行，不填充缓存。 | 真实 ReadDir 第 5 项取消（包括错误项）后不消费第 6 项；记录访问数、保留峰值和取消耗时；正常请求仍完整遍历并保持原 top-49 排序。 |
| D3 历史 | 删除无调用者的 Frecency.stats/bump/score；history values 由请求内 LazyCell 在实际 History generator 执行时构建一次。 | 真实 lookup→HistoryStore 覆盖 fish alias、shell/slot、重复历史；cache hit、debounce、trigger、多 generator 与取消不缓存；保留 NextWords 和 acceptance 排名。 |
| D4 catalog | generation 绑定的惰性 owner，首次真实 typed/native dispatch 才解析；未加载与已加载但缺失/拒绝分别记录。 | 请求正常/取消/unwind、刷新前后和 idle owner Drop 释放未消费 captured bytes；共享 pristine snapshot 不再钉住原始 sidecar。后续读取仍按 snapshot digest 验证；诊断新增 captured_bytes，兼容旧快照且不触发加载。 |
| D5 文件图标 | 有效显式图标跳过 native；由 UniformListDecoration 的真实 viewport 请求，沿用最多 8 个未缓存图标/批、64 个缓存与单 native worker；移出 viewport 的行释放 PNG 引用。 | 复核发现 render_items 也会用于离屏尺寸测量，改为 decoration 后避免第 0 行误请求及反复替换批次。真实 GPUI 绘制/滚动/重绘用例覆盖回调；provider/缓存用例覆盖显式/未知图标、迟到结果、隐藏释放。 |
| D6 caret | host 传递 overlay demand；无需求、全局禁用、单终端禁用时跳过按键驱动 AX/IME 查询，仍保留 focus/session 失效链。 | Show 先进入 WaitingForCaret 再请求位置；重新启用恢复入口；无需求/全局禁用的实际 refresh 入口断言查询计数不增加。真实 Otty/IME 和 xterm/AX 验收待新包。 |
| D7 设置 | 补全语义快照决定是否 recomplete；展示变化只 apply/notify/relayout；UI 通知与 watcher 回声共享快照，文件相同内容短路。 | 修复 watcher 二次读失败却记录成功快照的问题：直接安装首次验证成功的同一 Map。UI 写失败不发布成功刷新；展示/fuzzy/history/sort/删除 key 与重复事件均有分类回归。 |
| D8 host 输入队列 | buffer 与对应 caret refresh 组成一个有序单元，只合并尚未消费的连续完全相同非空输入。 | 所有其他事件、空白 EndInput、session/cursor/cwd/文本变化均为屏障；保留逐字符、粘贴及 self-insertion 语义。暂停消费者时 1,000 个相同输入仅占 1 个队列项；修复接收端关闭、已取出但未消费的项仍被误判为成功发送。 |
| D9 断连 context | 先取得 ready generation，再执行完整 shell context 构造闭包；最终入队继续校验 generation。 | 断连时构造闭包不执行，但 Prompt/PreExec 本地控制、pending insert、cwd 和 CommandInfo 历史保存继续；重连链沿用原 generation admission。 |
| D10 RPC 维护 | 维护任务按最早 pending RPC deadline 等待，新增/响应/取消唤醒重算；无 pending 不设置维护 timer。 | 新增更早 deadline 和 oneshot 取消真实 waker 回归；保留 outbox 预算、写期限和连接退役。现有 5 秒 Ping/Pong 保持兼容。 |

### 整体复核补充修复

整体审查确认并修复了四处单项审查未覆盖的交互问题：

- **冷 Show 旧候选。** 新 Show/Tab 在隐藏窗口后立即释放旧 rows/search/参数说明，仅保留选中身份；超过 200ms 的 loading 不再使旧候选重新可操作。
- **部分结果的 Tab 结算。** 当前输入的非空部分结果成功定位后，即消费最初的 Show/Tab 意图；空 pending 仍等待，最终空结果才回放。任何成功插入也统一消费意图，防止 Enter/点击后 hide 再补发 Tab。显式 Show 的参数展示标记独立保留到同一输入的最终结果，避免提前结算后动态参数变化又隐藏候选；输入变化、取消和成功插入均清除该标记。
- **临时模糊模式。** 保存的 fuzzy 默认值与快捷键的临时模式分开记录。字体、宽度、语言及 watcher 回声不重置临时模式或当前结果的匹配模式；保存的默认值改变时才更新模式，由补全语义快照安排重新计算。
- **隐藏输入的设置 IO。** fterm 先判断无修饰 Tab，再刷新 onlyShowOnTab 设置；普通按键和带修饰的 Tab 不触发该文件读取，长生命周期终端仍在每次符合条件的 Tab 上读取最新值。

回归源码覆盖真实 controller/InsertText outbox、部分结果到最终结果、未定位结果转为空结果、展示设置回声以及设置读取次数。GPUI 无原生窗口的测试中，成功定位通过生产结算入口建模；原生窗口定位与真实 shell 按键顺序仍需新包验收。所有新增回归均待远端 CI 编译和执行。

### 本轮明确保留的边界

- **D8 不合并不同文本。** 现有事件没有足够的 typing/paste 来源信息；粗暴取最后一条会改变隐藏/插入语义。队列整体仍无界，本轮削减相同输入突发与独立 refresh 消息，不声称已解决任意不同文本的积压，也未新增会阻塞主线程的 bounded.send。
- **D10 不停发旧心跳。** 当前握手没有 capability，Unix socket 也可能承载 SSH 转发，不能可靠区分安全的本地链路。按第八节协议实验边界只实施期限维护拆分；不宣称多标签空闲唤醒已经消失。
- **Tab 回放不是跨 IPC 的原子 compare-and-drop。** 发送前同时验证 controller 的输入和最新 FigtermSession buffer/cursor；切换输入/session 后不回放。现有 InsertText 的 insertion_buffer 会修复文本，不能拿它当条件写入。本轮保持协议，desktop 检查到 fterm 实际写入之间的用户按键竞态仍需未来的协议级条件插入处理。
- **取消仍是协作式。** 已阻塞的文件系统调用或 AppKit native icon 调用不能被布尔检查中断；既有 watchdog、单工作槽和迟到结果拒收继续生效。
- **不推算物理内存收益。** 少构造/少解析/少查询与释放引用不能直接换算为 phys_footprint；分配器页和系统框架缓存仍可能驻留。

### 验证与交付边界

本地仅做源码与实际 diff 审查、直接 rustfmt 格式检查及 git diff --check，未生成本地构建缓存，未替换运行中的 Fastab/IME/终端。远端 CI 仍需覆盖 fastab_engine、fastab_desktop、fastab_gpui、fastab_term、fastab_remote_ipc 和 alacritty_terminal 的回归，以及 workspace Clippy（-D warnings）。

新包验收应分开记录冷启动/纯静态补全/首次 hook/大目录快速改字/滚动图标/onlyShowOnTab/设置关闭后闲置 10 秒的工作计数、恢复延迟和 phys_footprint，并核对二进制身份。Otty 无 caret、连续 Tab 与切窗使用真实输入路径；多标签 RPC/半关闭/重连维持既有可靠性。本轮的源码审查结果不替代这些运行验收。
