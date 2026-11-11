# HybridFileXfer 进度记录（2026-11-11 22:47）

> 仓库：`C:\Users\nil\Desktop\HybridFileXfer-PC\HybridFileXfer-3.0.0`
> 接续 `HANDOFF-NEXT.md`（写到 2026-11-09 23:40）。本文件记 **2026-11-10 / 11-11 这一轮**：
> 「取消不断连」的收尾 bug（bug 12~15）、真机验证中又挖出来的 5 个新 bug（bug 16~20）以及它们的修法。

## 0. 一句话现状

「取消不断连」**数据中途取消** 与 **取消后立刻再传** 两条主路径已在真机跑通（含整文件 SHA-256 校验），
传输正常完成也不再出现「PC 等到 100 秒超时断线」；本轮共修掉 9 个 bug（12~20 + 两处顺带），
**代码未提交**，PC→手机方向的两项（手机取消 / PC 端中断）还没测。

## 1. 部署状态

| 项 | 状态 | 备注 |
|---|---|---|
| 手机 APK | ✅ 已安装（2026-11-11 22:34 版） | 含 bug 12~19 全部修复 |
| 接收器 exe | ✅ 已替换运行（`--autostart --verbose`） | 含 bug 12/20 + 收尾阶段日志 |
| 协议版本 | 304（两端必须一致，混装会被握手拒绝） | |
| 工作区 | 13 个文件已改、**未提交** | 见第 4 节 |

构建/部署命令见 `HANDOFF-NEXT.md` 第 4 节（JAVA_HOME / ANDROID_HOME 必须显式给）。

## 2. 真机验证结果（手机→PC，800MB / 3.25GB 文件）

| # | 场景 | 结果 |
|---|---|---|
| 1 | 传输中「暂停发送 → 取消发送」 | ✅ 手机留在双窗口；PC：`手机端取消了传输 → 删除未接收完的文件 → 接收已取消 → 取消后已重建通道 6 条，会话保持连接`；手机 `rebuild: done 6/6` |
| 2 | 取消后空闲 100 秒+ | ✅ 不再误判心跳超时（这就是 bug 19） |
| 3 | 取消后立刻再传 | ✅ 第二传完整走完：`接收完成 … 用时 8.0s (285MB/s)` + `校验通过：1 个文件 SHA-256 与手机端一致` |
| 4 | 收尾窗口内取消 | 🟡 部分验证：PC 已回结论帧、正等手机结论时收到 CANCEL → `接收已取消` + 重建 6/6、会话保持；手机晚到的结论帧被 `空闲时收到传输结论帧（已丢弃）` 收掉，没有帧错位。**"PC 等手机结论、手机还没写结论就取消"这个精确窗口没能稳定命中**（800MB 时窗口只有 1~2 秒） |
| 5 | PC→手机 传输中手机取消 | ⬜ 未测 |
| 6 | PC 端「中断发送」 | ⬜ 未测 |
| 7 | force-stop 回归 | ✅ `am force-stop` 后 PC 立刻报 `连接已断开：手机断开了连接`，随后自动重连（不再等 120 秒） |

## 3. 本轮修的 bug

### bug 12：PC 在等收尾结论时收到 CANCEL → 整条会话断开（日志实锤）

- 现场：`receiver.log` `连接已断开： unexpected control frame 25 while waiting for RESULT`。
- 原因：`Session::read_result()` 只认 RESULT / PAUSE_STATE / PING，其余一律当帧序错位 → Err → 断线。
  而用户完全可能在这个窗口点取消（数据已传完、PC 正在算 SHA-256）。
- 修法（`src/session.rs`）：`PeerResult` 增加 `cancelled`；`read_result()` 收到 CANCEL → 置取消并返回"已取消"，
  顺手支持收尾窗口里的 LIST_FILES / DELETE_FILE / MKDIR / CHECK_PENDING（手机界面这时照样会翻目录）、SHUTDOWN 当断线；
  `handle_receive` / `handle_send` 收到"已取消" → `finish_cancelled()`。

### bug 13：手机端重建握手收到残留 RESULT(23) → 直接断线

- 现场：`hfx_pull.log` `rebuild: unexpected opcode 23`、`hfx_core.log` `readControlOpcode: unexpected 23`。
- 原因：PC 在取消前一刻已经把结论帧写出；手机进重建后被它打断，判帧序乱 → 断线。
- 修法：`HFXServer.readControlOpcodeLocked()` 读到 RESULT 时**在同一把 controlLock 里把载荷读掉丢弃**
  （新增 `HFXService.discardResultPayload()`）；`ServerService.rebuildTransfers()` 把 RESULT 放进容忍集合。

### bug 14：取消后下一次传输 `IllegalArgumentException: newPosition > limit`

- 现场：取消后再传，手机 `SendFileCall … newPosition > limit: (1048576 > 0)`，PC `数据校验失败 … CRC 不一致`。
- 原因：两层 ——
  1. `ReadFileCall.recycleAllBuffer()` 只**遍历**队列把缓冲区还池子、**没摘除**，块还在队列里，
     别的发块线程还能 take 到它，可它的缓冲区已经回池、可能正被写入；
  2. 取消时 `sendFiles()` 直接返回，**读文件线程还在读文件、占着同一批缓冲区**，下一次传输和它抢同一个 ByteBuffer。
- 修法：`ReadFileCall` 增加 `shutdown` 标志（读块循环/取到缓冲区后都检查，读线程自己退出），
  `recycleAllBuffer()` 改成 `deque.pollFirst()` 摘出来再还；
  `HFXService.sendFiles()` 取消/异常分支先 `awaitTransferThreads(...)`（打断读线程 + 有界等待）再返回。

### bug 15：取消后界面弹「复制完成」

- 原因：`TransferScreen.startTransfer()` 里 `sendFilesToRemote/Shelf` 取消时是正常返回的，协程收尾无条件写「复制完成」。
- 修法：收尾前读 `server.isTransferCancelled()`，取消时显示「已取消传输」，也不再把进度条补满。

### bug 16：数据发完后界面卡在「79x/800 MB · 0 KB/s」（用户报的"手机端卡进度"）

- 原因：数据发完后手机线程在等 PC 的结论帧（PC 此时在算整文件 SHA-256），界面没有任何提示；
  另外累计字节是用 `onSpeedInfo` 每秒增量累加出来的，最后一秒的字节没进计数，永远差一截。
- 修法：`TransferScreen` 统计 `onChannelComplete`（所有通道 EOF 发出 = 数据阶段结束）→
  进入"等待电脑校验"状态：进度条补满、状态行显示「等待电脑校验…」（`UiState.xferNote` + `Dialogs.kt`），
  通道行显示「已完成」。

### bug 17：会话断开瞬间 `pollControlOpcode` 空指针

- 现场：`sendFiles: readResultFrame failed : java.lang.NullPointerException … DataByteChannel.available() on a null object reference`。
- 修法：先把 `ctChannel` 取到局部变量并判空，为空抛 `IOException`（按"控制通道已断"收尾）。

### bug 18【关键】`DataByteChannel.available()` 在真机上永远返回 0 → 收尾永久僵持

- 现场：PC 日志 `收尾：结论帧已写出，正在等手机端`，手机 `pollControlOpcode: waiting … available=0` 一直刷，
  两边互相等；手机内核队列里其实排着 860 字节（RESULT + 一堆 PAUSE_REPLY）。
- 原因：304 为了"取消时能打断等待"，把阻塞 `readShort()` 换成了 `available() >= 2` 轮询，
  而 `available()` 走的是 Socket 的流式 `InputStream`，和 `SocketChannel` 直接读写混用时返回 0。
- 修法：新增 `HFXService.readControlHeaderPolled(timeout, abortOnCancel)`，直接对 `SocketChannel` 做非阻塞读
  （和重建握手的 `readControlOpcode` 同一套），`pollControlOpcode` / `probePeerCancel` 全部改用它；
  只在"一个字节都没读走"时才认取消/超时（半个帧头绝不丢）。**修完后一次传输 8 秒收尾、校验通过。**

### bug 19【关键】一次正常传输之后会话 100 秒后必断线（handoff 里"没解释清"的那件事）

- 现场：传输完成 `接收完成/校验通过` 后 100 秒整，PC 报 `连接已断开： 与手机的连接超时（没有收到心跳）`。
- 原因（用 jdb 挂到手机上抓线程栈才看清）：
  `DefaultDispatcher-worker`（界面协程）在 `TransferScreen.loadRemote → HFXServer.listClientFiles → doListClientFiles → readUTF` 里永久阻塞，
  **并且握着 controlLock**：它把通道里残留的 PAUSE_REPLY 当成了"列表长度"，之后等一个永远不来的 UTF；
  会话线程要发心跳必须先拿 controlLock → 拿不到 → 心跳停 → PC 100 秒判超时。
  残留的 PAUSE_REPLY 来自传输期间"会话线程只写 PAUSE_STATE、不读回报"。
- 修法：
  1. 新增 `HFXService.drainStaleControlFrames()`：发请求前（持 controlLock）先把上一轮残留的
     PAUSE_REPLY / PING / RESULT 读掉，遇到 CANCEL/REBUILD_REQ 就地处理；`listClientFiles / deleteRemoteFile /
     createRemoteDir / checkPending` 都先调它；
  2. `DataByteChannel` 增加可选的读超时，控制通道设 30 秒：帧再对不上也只是这次请求失败，不会永久卡死握锁。

### bug 20：接收器计数跨传输累加

- 现场：连续三次传输后日志写 `收尾：数据收齐，开始落盘收尾 (4 个文件 / 5.48 GB)`、`接收完成：3 个文件 2.23GB …`。
- 修法：`Receiver::reset_session_counters()`，每次 `handle_receive` 开头清零；日志里的 "bytes" 尾巴也去掉了。

### 顺带修的两处

- `HFXServer.abortTransfer()` 现在会 `resumeTransfer()`：取消后暂停闸门不再一直停在"已暂停"。
- `HFXService.probePeerCancel()` 读帧时在同一把 controlLock 里把载荷读掉（PAUSE_REPLY/REBUILD_REQ/RESULT/PING），
  只有真正不认识的帧才留 `pendingOpcode`。

## 4. 改动文件（相对 commit 0d9fbeb，13 个）

| 文件 | 本轮改动 |
|---|---|
| `HybridFileXfer-Receiver/src/session.rs` | `PeerResult.cancelled`、`read_result` 处理 CANCEL/界面帧、收尾阶段日志、收尾取消走重建、计数清零 |
| `HybridFileXfer-Receiver/src/receiver.rs` | `reset_session_counters()` |
| `HybridFileXfer-Receiver/src/proto.rs` | 304 版本 + REBUILD_REQ/ACK（上一轮） |
| `core/HFXService.java` | `readControlHeaderPolled`、`drainStaleControlFrames`、`discardResultPayload`、`awaitTransferThreads`、`probePeerCancel` 重写、CONTROL_TIMEOUT 上移 |
| `core/ReadFileCall.java` | shutdown 标志 + 读线程可停 + 回收时从队列摘除 |
| `droidcore/HFXServer.java` | 重建读帧丢弃 RESULT 载荷、`abortTransfer` 放开暂停闸门、控制通道 30s 读超时、请求前 drain |
| `ServerService.java` | 重建握手容忍 RESULT |
| `nio/DataByteChannel.java` | 可选读超时（控制通道 30s） |
| `compose/TransferScreen.kt` | 取消不再显示「复制完成」；「等待电脑校验…」状态 |
| `compose/Dialogs.kt` / `compose/HfxApp.kt` | UiState.xferNote + 进度面板显示 |
| （`ControllerIdentifiers.java` / `ServerListener.java` 为上一轮 304 改动） | |

## 5. 下一步

1. **提交**这批修复（建议 4 个提交：接收器 session 收尾 / Android 控制通道读写 / 取消与进度 UI / 计数与日志）。
2. 补测：PC→手机传输中手机取消（第 5 项）、PC 端「中断发送」（第 6 项）。
3. **断点续传**（用户 2026-11-11 追加需求）：见第 7 节的设计，协议升到 305。
4. 收尾窗口内取消的精确窗口（第 4 项）如果有条件，用更大的文件（3.25GB 以上）再试一次。

## 6. 真机测试踩坑

- **uiautomator dump 会返回旧一帧**：弹窗刚消失时会拿到上一帧内容；关键判断用 `screencap`。
- 取消后弹的「提示/已取消传输」对话框：点「知道了」；**对话框已经消失时再点它会落到左窗口的文件夹上**
  （会误进 `MIUI` 目录，白跑一次）。稳妥做法：先 dump 找「知道了」再点。
- 坐标（2136×3200）：主页「传输文件」(1068,1273) 连点两次开双窗口；长按文件
  `adb shell input swipe X Y X Y 1100`；「复制 →」(862,1428)；「暂停发送」(1069,2000)；
  暂停后「继续」(855,2000)/「取消发送」(1282,2000)；「↑ 上级」(510,3083)。
- `hfx_core.log` 是核心流程；**重建过程在 `hfx_pull.log`**；`receiver.log` 时间戳是 UTC。
- 卡死时抓线程栈：`adb forward tcp:8700 jdwp:<pid>` + jdb
  `-connect com.sun.jdi.SocketAttach:hostname=localhost,port=8700`，命令 `suspend` / `where all` / `resume`（bug 19 就是这么定位的）。

## 7. 断点续传（下一步要做的设计）

目标：同一个文件（路径 + 大小 + 修改时间一致）被中断/取消后，下一次传输只补缺失的块，而不是从 0 重传。
现有协议本身就是 1MiB 定长块 + 每块 CRC32
（`[i16 FILE][i32 fileIndex][UTF path][i64 lastModified][i64 totalSize][i32 blockIndex][i32 length][i32 crc32][data]`），
天然适合按块续传。

- **接收端**（手机→PC 时是 PC；反向是手机）为每个目标文件维护 sidecar 日志 `<文件名>.hfxpart`：
  魔数 + 块大小 + 总大小 + 源修改时间 + 已完成块的位图。每写一块就置位（可 1 秒批量落一次盘），
  收尾成功删除 sidecar。
- **控制通道新帧（版本号 305）**：
  - `RESUME_QUERY=28`（发送方 → 接收方）：`{i32 count}{ (UTF path, i64 size, i64 mtime) * count }`
  - `RESUME_REPLY=29`（接收方 → 发送方）：`{i32 count}{ (bool canResume, i32 blockCount, i32 bitmapLen, byte[] bitmap) * count }`
  - 手机→PC：`REQUEST_RECEIVE` 之后立刻发 `RESUME_QUERY`、读 `RESUME_REPLY`，再开始发块（`handle_receive` 一进来就读这一帧）。
  - PC→手机：手机发 `REQUEST_SEND` 时带 `RESUME_STATE`（自己的位图），PC 的 `handle_send` 读完请求后读它，跳过的块不发。
- **发送端**：`ReadFileCall` 增加"跳过这些块"的集合；跳过的块不计入进度（进度从"已有字节"起算）。
- **取消语义调整**：取消/断线时**保留**半截文件 + sidecar（原来 303 是删掉，避免"大小正常但有洞"的假文件；
  有位图就不会有这个问题），界面提示"已保留，可续传"；源文件大小/修改时间变了就丢弃旧 sidecar 重新来。
- **测试**：Rust 侧给 `spawn_mock_phone` 加一个"续传"用例（先传一半断开，再连上只补剩下的块，校验 CRC/SHA-256）；
  真机上"传 800MB 到一半取消 → 再传"，日志里应看到 `续传：已跳过 N 块`。
