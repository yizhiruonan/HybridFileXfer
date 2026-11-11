# HybridFileXfer 进度记录（2026-11-11 23:10）

> 仓库：`C:\Users\nil\Desktop\HybridFileXfer-PC\HybridFileXfer-3.0.0`
> 接续 `HANDOFF-NEXT.md`（写到 2026-11-09 23:40）。本文件记 **2026-11-10 / 11-11** 这一轮。

## 0. 一句话现状

1. 「取消不断连」真机跑通：数据中途取消、取消后立刻再传、空闲 100 秒、force-stop 都验过；
2. 这一轮又挖出并修掉 9 个 bug（bug 12~20），其中两个是"收尾僵持 / 传完 100 秒断线"的真凶；
3. **发送进度条改成真实动态分段**（IDM 式切割线，通道抢段时会出现新的分割线）；
4. **断点续传（手机→电脑）已实现并真机验证**：传到 569MB 取消，再传只补 231MB，2.8 秒完成、SHA-256 一致。
   电脑→手机方向的续传还没做。

提交：`66418d9`（304 取消不断连 + 9 个 bug）→ `a07ab66`（动态分段进度条）→ `5df02b8`（305 断点续传）。

## 1. 部署状态

| 项 | 状态 | 备注 |
|---|---|---|
| 手机 APK | ✅ 已安装（305 版） | 含取消/收尾/进度条/续传全部修复 |
| 接收器 exe | ✅ 已替换运行（`--autostart --verbose`） | 305 版，收尾阶段有详细日志 |
| 协议版本 | **305**（两端必须一致） | 304 的旧端会被握手拒绝 |

## 2. 真机验证结果

| # | 场景 | 结果 |
|---|---|---|
| 1 | 传输中「暂停 → 取消」 | ✅ 手机留在双窗口；PC：`取消 → 保留未接收完的文件（可续传）→ 重建 6 条，会话保持连接` |
| 2 | 取消后空闲 100 秒+ | ✅ 不再误判心跳超时（bug 19） |
| 3 | 取消后立刻再传 | ✅ 完整走完 + SHA-256 一致（bug 14 修好后不再炸缓冲区） |
| 4 | 收尾窗口内取消 | 🟡 已验证"PC 已回结论帧、等手机结论时收到 CANCEL"→ 取消+重建、会话保持、晚到的结论帧被丢弃；**"PC 等手机结论、手机还没写结论就取消"这个精确窗口没稳定命中** |
| 5 | 正常传输 | ✅ 800MB 8~10 秒、3.25GB 47 秒，收尾日志 `接收完成 / 校验通过` 正常 |
| 6 | force-stop 回归 | ✅ 立刻断线 + 自动重连 |
| 7 | **断点续传（手机→PC）** | ✅ 569MB 取消 → 保留 `.hfxpart` → 再传 `续传：已有块 569`、只传 231MB（2.8s）、`校验通过：1 个文件 SHA-256 与手机端一致`，位图文件自动删除 |
| 8 | 发送进度条动态分段 | ✅ 截图对比：369MB 与 1.98GB 时分段边界不同（通道抢段后出现新的切割线） |
| 9 | PC→手机 传输中手机取消 | ⬜ 未测 |
| 10 | PC 端「中断发送」 | ⬜ 未测 |
| 11 | PC→手机 断点续传 | ⬜ 未做 |

## 3. 本轮修的 bug（12~20）

### bug 12：PC 在等收尾结论时收到 CANCEL → 整条会话断开
- 现场：`连接已断开： unexpected control frame 25 while waiting for RESULT`。
- 修法：`read_result()` 收到 CANCEL → `control::cancel()` + `PeerResult.cancelled`；顺手处理收尾窗口里的
  LIST_FILES/DELETE_FILE/MKDIR/CHECK_PENDING、SHUTDOWN 当断线；收发两处的 "已取消" 走 `finish_cancelled()`。

### bug 13：手机端重建握手收到残留 RESULT(23) → 直接断线
- 修法：`HFXServer.readControlOpcodeLocked()` 读到 RESULT 时在同一把 controlLock 里把载荷读掉丢弃
  （`HFXService.discardResultPayload()`）；`rebuildTransfers` 把 RESULT 放进容忍集合。

### bug 14：取消后下一次传输 `IllegalArgumentException: newPosition > limit`
- 原因：`recycleAllBuffer()` 只遍历队列不摘除（块还能被别的线程领到，缓冲区已回池）；
  取消后读文件线程还在读文件、占着同一批缓冲区。
- 修法：`ReadFileCall` 增加 `shutdown` 标志（读线程自己退出）、回收改成 `pollFirst` 摘除；
  `sendFiles` 取消/异常分支先 `awaitTransferThreads(...)`。

### bug 15：取消后界面弹「复制完成」
- 修法：收尾前读 `isTransferCancelled()`，取消时显示「已取消传输」。

### bug 16：数据发完后界面卡在「79x/800 MB · 0 KB/s」
- 修法：`onChannelComplete` 统计所有通道 EOF → 进度条补满、状态行显示「等待电脑校验…」、通道行「已完成」。

### bug 17：断开瞬间 `pollControlOpcode` 空指针
- 修法：局部取 `ctChannel` 判空，抛 `IOException`。

### bug 18【关键】`DataByteChannel.available()` 真机恒为 0 → 收尾永久僵持
- 现场：PC `收尾：结论帧已写出，正在等手机端`，手机 `pollControlOpcode: waiting … available=0` 一直刷。
- 原因：304 改成 `available() >= 2` 轮询，而它走 Socket 的流式 InputStream，和 SocketChannel 混用时返回 0。
- 修法：新增 `readControlHeaderPolled()` 直接对 SocketChannel 做非阻塞读（和重建握手同一套），
  `pollControlOpcode` / `probePeerCancel` 全部改用它；只在"一个字节都没读走"时才认取消/超时。

### bug 19【关键】一次正常传输后 100 秒必断线（handoff 里"没解释清"的那件事）
- 原因（jdb 抓线程栈）：界面协程在 `loadRemote → doListClientFiles → readUTF` 里永久阻塞并握着 controlLock
  —— 它把残留的 PAUSE_REPLY 当成了"列表长度"；会话线程拿不到锁 → 心跳停 → PC 100s 判超时。
- 修法：`drainStaleControlFrames()`（发请求前先吃掉残留的 PAUSE_REPLY/PING/RESULT，遇 CANCEL/REBUILD_REQ 就地处理），
  `listClientFiles / deleteRemoteFile / createRemoteDir / checkPending` 都先调它；
  控制通道读加 30 秒上限（`DataByteChannel.setReadTimeout`），帧再对不上也只是这次请求失败。

### bug 20：接收器计数跨传输累加
- 修法：`Receiver::reset_session_counters()`，每次 `handle_receive` 开头清零（要排在续传查询之前）。

### 顺带
- `abortTransfer()` 放开暂停闸门；`probePeerCancel()` 在同一把锁里读掉载荷。

## 4. 发送进度条：真实动态分段（commit a07ab66）

- 以前：一个共享队列谁抢到下一块谁发 → 块在通道间轮流 → 段边界每块都变 → 界面只能画"按通道等分"的假分段，
  传输过程中不会出现新的切割线。
- 现在：新增 `core/SendRanges`，文件的块区间按通道切成**连续段**，通道干完自己的段后把别人还没产出的
  最长一段再切一半过来（段数会变多）。读文件仍然是顺序读（磁盘友好），只是按计划把每块投递到
  各通道自己的队列（`ReadFileCall` 每通道一个队列）。
- `SendFileCall` 只领自己那段的块；没活了先 `trySteal`；都干完才发 EOF
  （空闲通道必须收工，否则电脑端一直等它的 EOF）。
- 双窗口发送弹窗画真实分段；多文件/接收方向/续传时退回整条连续进度。

## 5. 断点续传（305，手机→电脑，commit 5df02b8）

- 接收端每个文件旁边一份 `<文件名>.hfxpart`：魔数 + 块大小 + 总大小 + 源修改时间 + 已完成块位图
  （`src/journal.rs`）。每写一块置位；收尾成功删除；取消/中断时**半截文件和位图都保留**。
- 控制通道新帧：`RESUME_QUERY=28`（发送方问）/`RESUME_REPLY=29`（接收方回位图）。
  手机在 `REQUEST_RECEIVE` 之后立刻发查询，电脑的 `handle_receive` 一进来就读它并回位图。
- 手机 `ReadFileCall` 按位图跳过已有块（不读盘、不发网络），进度条用 `onResumeSkipped` 的基线。
- 为什么不再删半截文件：303 当初删是因为"大小对但有洞"看不出来；有位图就不会被骗，可以放心留着续传。
- 待做：电脑→手机方向的续传（手机作接收端，需要手机侧也记位图），以及"文件大小/修改时间变了就丢弃旧位图"的产品提示。

## 6. 下一步

1. 补测第 9、10 项（PC→手机时手机取消 / PC 端「中断发送」）。
2. PC→手机方向的断点续传（手机侧 `WriteFileCall` 记位图 + `REQUEST_SEND` 后回 `RESUME_STATE`）。
3. 收尾窗口内取消的精确窗口再试（用 3.25GB 文件，窗口更大）。
4. 界面细节：续传时进度条已经是"基线 + 增量"，但"各通道进度"表还是从 0 算，可选优化。

## 7. 真机测试踩坑

- `uiautomator dump` 会返回旧一帧（弹窗刚消失时），关键判断用 `screencap`。
- 「知道了」按钮：先在 dump 里找它的 bounds 再点；对话框消失后点固定坐标会落到左窗口的文件夹上。
- 坐标（2136×3200）：主页「传输文件」(1068,1273) 连点两次；长按文件 `input swipe X Y X Y 1100`；
  「复制 →」(862,1428)；「暂停发送」(1069,2000)；暂停后「继续」(855,2000)/「取消发送」(1282,2000)；
  「↑ 上级」(510,3083)。
- 日志：`hfx_core.log`（核心）、`hfx_pull.log`（重建过程）、`receiver.log`（UTC）。
- 抓线程栈：`adb forward tcp:8700 jdwp:<pid>` + jdb `-connect com.sun.jdi.SocketAttach:hostname=localhost,port=8700`，
  命令 `suspend` / `where all` / `resume`。
