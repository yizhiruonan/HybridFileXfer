# B 方案：手机端同时接多台电脑（已实现）

> 状态：**已完成，真机验证通过**（2026-11-01）。
> 手机端可以同时被多台电脑连接，各自独立收发文件，互不干扰。
> 与 A 方案对称：A 是「电脑侧多设备」，B 是「手机侧多连接」。

## 1. 目标

一台手机同时被多台电脑连接，每台电脑一条独立的会话：
各自的控制通道 + 传输通道 + 缓冲区块 + 进度，互不串扰。

## 2. 核心难点：控制通道和传输通道分不开

原来手机只服务一台电脑，`accept()` 一次拿控制通道，再按网卡列表逐条 accept 传输通道，
顺序天然正确。多台电脑时所有连接都落到**同一个监听端口**，协议里又没有字段能区分
「这是另一台电脑的控制通道」还是「当前电脑的传输通道」：

- 控制通道：TCP 连上后**立刻**写 `HFXC` 协议头（4 字节）+ 版本号；
- 传输通道：整场握手结束前**一个字节都不发**（见 `session.rs` / `HFXClient`）。

所以新增 `droidcore/ServerListener.java`：每 accept 一个连接先观察 400ms——

| 观察结果 | 判定 |
| --- | --- |
| 读到 `HFXC` | 控制通道（协议头顺便读掉，握手不再重复读） |
| 窗口内一直安静 | 传输通道 |
| 别的字节 / 提前断开 | 丢弃 |

握手期间收到的**别的电脑的控制通道**先暂存（最多 8 条），当前电脑的握手结束后再依次处理，
绝不会被错当成传输通道。代价是当前电脑的每条传输通道多等约 400ms（一台电脑最多 8 条，
仍在电脑端 15s 握手超时之内）。

## 3. 实际改造

### 3.1 `droidcore/HFXServer.java`

- 新增静态 `bindShared(port)` / `closeShared()`：**整个手机只绑定一次**监听端口
  （非阻塞，accept 循环靠轮询检查服务状态）。
- `startServer(...)` 改为在共享套接字上跑一次握手：
  `startServer(ServerListener, PendingControl, interfaces, buffers, callback)`，返回是否成功；
  失败只关掉本次连接，**不动监听套接字**。
- 去掉 `public static HFXServer instance`，改成 `Map<Integer, HFXServer> INSTANCES`：
  - 注册时分配显示名（电脑 A / B / C…），`getDisplayName()`
  - `register / unregister / snapshot / getAny / getById / count / disconnectAll`
  - `isAlive()`：控制通道是否还开着，给心跳和连接上限用
- `disconnect()` 加 `synchronized`（服务线程和会话线程可能同时收尾，避免重复释放 native 缓冲）；
  通道列表读取都先复制本地引用，UI 线程不会撞上 NPE。

### 3.2 `ServerService.java`

- 监听循环拆成两段：
  1. `serverLoop`：绑一次端口 → 循环 accept + 握手（**串行**，保证 accept 顺序正确）→ 成功就开会话线程；
  2. `sessionLoop`：一台电脑一条线程，心跳 + `CHECK_PENDING` 拉取推送文件，断开只影响自己。
- 状态：任意一台在线 = `STATE_CONNECTED`，全部断开回 `STATE_WAITING`，监听一直保持。
- **连接数上限**（`Config.getMaxConnections()`，默认 3，设置页可调 1–5）：
  达到上限先清理已死的会话，仍满就关掉新连接（电脑端会重连）。
- `requestFresh()`（电脑 adb 拉起服务时带 `fresh=true`）不再踢掉所有连接：
  只清理已经失效的会话，多台电脑互不影响。
- 进度 `Live` 从全局唯一改成 `Map<连接号, Live>`：
  `liveFor(id)` / `transferringCount()` / `currentTransfer()` / `receivedTotalBytes()`（累计字节，算合计速率）。
- `getConnectedInterfaceNames()` 返回所有电脑通道名的并集（旧界面用）；
  `getChannelDetails()` 返回 `电脑 A · wlan0\\t地址` 这种带归属的明细。
- 握手兜底：`HandshakeGuard` 20s 超时关掉控制通道，避免把 accept 循环堵死。

### 3.3 手机 UI：新增「连接」标签页

> 最初做成首页状态卡展开，实测后按需求改为**独立标签页**：
> 底栏 `主页 / 连接 / 设置`，原「通道」页并入「连接」，不再用展开式界面。

「连接」页从上到下：

1. **已连接电脑**：`已连接 N 台电脑 · M 条通道`；每台一张小卡，显示
   电脑 A/B/C、对端 IP（adb 显示 `USB / ADB`）、N 条通道、
   每条通道的 `名称 · 对端地址 · 协商速率`、正在接收时显示 `接收中 45% · 文件名`。
2. **限速**（原通道页）
3. **网卡 / 线路开关**（原通道页：USB 2 条、WLAN 4 条）
4. **环境**：接收目录、模式、WLAN 协商速率、USB 协议、ROOT / Shizuku 状态。

首页状态卡不再展开：点它（或点「通道」小卡）直接跳到「连接」页。

### 3.4 进度浮层：分块进度条 + 每通道明细（IDM / 迅雷 风格）

- 进度条不是"按百分比填充的一条"，而是按**真实文件块完成度**切成 96 段：
  写盘线程每写完一块回调 `WriteFileCall.onBlockWritten` → `BlockProgress` 位图；
  完成的段是主色，未完成是浅灰，一段只完成一部分时按比例填充。
  文件头一到就 `beginFile(path, totalSize)`，所以进度条一开始就是完整的一条，随后被逐渐填满。
- 下面列出**每一条传输通道**：编号、通道名、累计已接收、当前速度（或"已完成"）。
  数据来自 `SpeedMonitorThread.onSpeedInfo`，累计进 `Live.ChannelLive`。
- 多台电脑时标题显示 `正在接收 N 台电脑的文件`，正文显示最活跃的一台（`电脑 B · 文件名`），
  速率显示**合计速率**；面板最高 560dp，通道多时可滚动。

### 3.5 发送目标

- 「传输文件」在多台电脑时先弹 `发送到哪台电脑`（电脑 A/B/C + IP + 通道数），选完再开文件选择器。
- `ShareActivity` 新增 `EXTRA_CONNECTION_ID`，按连接号取指定那台电脑发送。

## 4. 验收（2026-11-01 真机实测）

环境：Android 平板 a6eb08a5（Android 15，6 条通道：USB×2 + WLAN×4），
电脑端接收器真实连接 + 两个模拟客户端同时经 adb 转发连入（相当于三台电脑）。

| 验收项 | 结果 |
| --- | --- |
| 单台电脑不回归 | ✅ 真实接收器连接正常，状态卡显示「已连接 1 台电脑」 |
| 多台同时连接 | ✅ 3 台电脑同时在线，各 6 条通道，UI 逐台列出 |
| 控制/传输通道不错配 | ✅ 两台模拟电脑同时握手（第二台被串行化），各自的传输通道全部归属正确 |
| 两台同时推送文件 | ✅ 各推 200 MB，同时进行，分别落到 `Download/HybridFileXfer/mock-*.bin`，大小完全正确 |
| 进度浮层 | ✅ 显示「正在接收 2 台电脑的文件」「合计 30.01 MB/s」，每台卡片各自显示自己的文件 |
| 分块进度条 / 通道明细 | ✅ 300 MB 单文件：整条被切成 96 段逐渐填满，下面 6 条通道各显示已收 ~40 MB、5–6 MB/s |
| 断开互不影响 | ✅ 两台模拟客户端退出后自动回收，真实接收器连接保持 |
| 连接上限 | ✅ 达到 3 台上限时新连接被干净拒绝（等旧会话回收后可再连） |

## 5. 踩坑记录（B 特有）

1. **不能靠"IP 相同"认传输通道**：adb 转发时所有电脑都是 127.0.0.1，只能靠"谁先说话"分类。
2. **握手必须串行**：两个线程同时 accept 会把 A 的传输通道塞给 B；所有 accept 只在监听线程里做。
3. **监听套接字是共享的**：单个会话结束/失败时只关自己的控制+传输通道，绝不能关监听端口。
4. **断开要幂等**：服务停止和会话线程会同时收尾，`disconnect()` 要同步，否则 native 缓冲重复释放。
5. **fresh 不能全局踢人**：电脑 adb 拉起服务时带的 `fresh=true` 以前会断开唯一连接，
   多台时那样会踢掉别人；现在只清理失效会话。
6. **UI 不要做展开**：首页卡展开在手机上占高、状态一多就乱；改成「连接」标签页后清爽很多。
7. **通道名有重名**：每台电脑都有 `USB_ADB`、`wlan0`，聚合展示必须带电脑前缀。
8. **TrafficInfo 第一秒没有 iName**：`resetCurrentTrafficInfo()` 第一次返回的是 `new TrafficInfo()`（没名字），
   按名字分组会塞进 null key，Kotlin 的非空参数直接 NPE 闪退。已在 `TransferConnection` 里补名字、
   UI 侧再过滤空名。

## 6. 与 A 方案的对应

| A（电脑侧多设备） | B（手机侧多连接） |
| --- | --- |
| `engine::run_multi` 每台手机一条线程 | `ServerService` 每台电脑一条会话线程 |
| 每台手机独立 adb 转发端口 | 单监听端口 + `ServerListener` 分类 |
| `receiver::progress_for(serial)` | `ServerService.Live` 按连接号隔离 |
| `max_devices` 限流 | `Config.getMaxConnections()`（默认 3） |
| `pending::Item.target` 定向推送 | `ShareActivity.EXTRA_CONNECTION_ID` 定向发送 |
| 设备卡片一行两个 | 「连接」页一台一张小卡（含通道明细） |
