# B 方案交接：手机端同时接多台电脑

> 状态：**未开始**。A 方案（一台电脑连多台手机）已完成并可用，本文件描述 B 的实施计划。
> 目标读者：接手此任务的开发者（或下一个 AI 会话）。

## 1. 目标

一台手机能同时被**多台电脑**连接，各自独立收发文件，互不干扰。
与 A 方案对称：A 是「电脑侧多设备」，B 是「手机侧多连接」。

## 2. 现状（为什么现在只能接一台）

`ServerService` 的运行线程里：

```
while (active) {
    HFXServer server = new HFXServer(ioService);
    HFXServer.instance = server;                       // ← 静态单例
    server.startServer(port, interfaces, ...);         // ← 内部 bind + accept + 跑完整个会话
}
```

`HFXServer.startServer()` 内部：

```java
serverSocketChannel = ServerSocketChannel.open().bind(new InetSocketAddress(port)); // 绑定
ctChannel = new DataByteChannel(serverSocketChannel.accept());                     // 接一个 PC
// …握手：逐个 accept 传输通道…
runSession(...)                                                                    // 阻塞到断开
```

问题：`accept()` 只调用一次，且`startServer` 阻塞到会话结束。第二台电脑连进来时没人 accept，
连接会停在 backlog 里，等第一台断开后才会被处理。

另外 `HFXServer.instance` 是**静态单例**，手机 UI（`ServerService.getServer()`、
`getConnectedInterfaceNames()`、`getChannelDetails()`）都依赖它，也只能反映一台电脑。

## 3. 改造清单

### 3.1 `droidcore/HFXServer.java`

- 抽出 `public static ServerSocketChannel bindShared(int port)`：只绑定一次，保存在静态字段。
- `startServer(...)` 改为：**不再自己 bind**，从共享 socket 上 `accept()`；其余握手逻辑不变。
- `public static HFXServer instance` → `private static final Map<Integer, HFXServer> INSTANCES`：
  - key 用自增 id 或对端 IP
  - 提供 `register(id, server)` / `unregister(id)`
  - 保留 `getAny()` 供"只关心有没有连接"的地方使用（`isConnected()`）

### 3.2 `ServerService.java`

- 外层循环改为：
  ```
  ServerSocketChannel shared = HFXServer.bindShared(port);
  while (active) {
      HFXServer server = new HFXServer(ioService);
      int id = register(server);
      new Thread(() -> { server.startServer(shared, ...); unregister(id); }).start();
  }
  ```
- 加**连接数上限**（建议 3，与电脑端 `max_devices` 对称）：达到上限就先不 accept。
- `getConnectedInterfaceNames()` / `getChannelDetails()` 改为**聚合所有实例**，并带上连接标识：
  `"电脑A · USB_ADB"` 这种格式，否则不同电脑的通道名会混淆。
- `isConnected()` 改为 `INSTANCES` 非空。

### 3.3 手机 UI（`compose/HfxApp.kt` + `Widgets.kt`）

- 首页状态卡：`已连接 N 台电脑`
- 展开后按电脑列出（参考电脑端的 `DevicePair` 一行两个小卡片做法）
- 每台显示：对端 IP / 通道数 / 当前传输文件 / 已接收量

### 3.4 进度浮层（`compose/Dialogs.kt`）

现为单文件单进度。多电脑时二选一：

- **A**：按连接分行显示（信息全，但高）
- **B**：显示"当前最活跃的一个 + 合计速率"（更简洁，推荐）

`ServerService.Live` 现在只有一份，需要改为按连接 id 的 Map，或在 service 层合并成一个"最活跃"。

## 4. 必须提前决定的三个点

1. **通道名冲突**：每台电脑都有 `USB_ADB`、`wlan0`，聚合展示时必须加电脑前缀，否则看不出归属。
2. **内存**：每台电脑一份缓冲区块（`localBufferCount × 1 MiB`，默认 8 MiB）。3 台 = 24 MiB，
   可以接受，但要设上限并写进设置项。
3. **接收目录**：现在电脑端推送时会带自己的目标目录（手机侧），多电脑各自带即可，无需改动。

## 5. 实施顺序与验收

| 步骤 | 内容 | 验收 |
| --- | --- | --- |
| 1 | `HFXServer`：`bindShared` + `INSTANCES` 集合 | 编译通过，**单台电脑仍能正常连接收发** |
| 2 | `ServerService`：accept 循环 + 每连接线程 + 上限 | 单台不回归；接两台电脑都能显示已连接 |
| 3 | UI：多电脑列表 | 两台电脑各发一个文件，互不串 |
| 4 | 进度浮层多连接 | 两台同时传时 UI 不崩、数据正确 |

> **注意**：第 1、2 步之间代码是编译不过的（单例引用遍布 UI 和 service），必须一口气做完这两步才能验证。

## 6. 参考：A 方案踩过的坑（B 大概率会遇到同类的）

| 坑 | A 的表现 | 对应到 B 要注意什么 |
| --- | --- | --- |
| 端口/资源冲突 | 两台手机都用 `5740` 转发，互相覆盖 | 手机端监听端口只有一个，但**每连接的传输通道 accept 要挂在正确的会话上** |
| 抢连接 | 局域网发现回退让两条会话抢同一台手机 | 手机端不要用"最近的一个"这类共享状态 |
| 单例 | `PROGRESS` 全局，多设备互相覆盖 | `HFXServer.instance`、`ServerService.Live` 都要集合化 |
| UI 模型 | 改用配对模型后忘了给 `devices` 赋值，展开入口消失 | 手机端加列表时同样要同时维护"判断用"和"渲染用"两个模型 |
| 高度计算 | 展开后内容变高，窗口底部按钮被裁 | 手机端是滚动列表，问题不大，但浮层要注意 |

## 7. A 方案已完成的部分（不要回退）

- 电脑端 `engine::run_multi` 多设备监督者（每台独立 `run_auto` 线程，`max_devices` 限流）
- 每台手机独立 adb 转发端口（`5750 + i` → 设备 `5740`）
- `Session::connect_with_local_port`：回环地址用 local port
- `EngineOptions.pinned`：绑定设备的会话跳过局域网发现回退
- `Discovery::shared()`：UDP 5741 全局单例
- `receiver::progress_for(serial)`：进度按设备隔离
- 待发送队列 `pending::Item { path, target }` + `set_hold`（多台时暂扣，选好再发）
- 界面：三卡并两卡、设备一行两个小卡片、展开显示完整通道、单实例、窗口高度自适应

相关提交可在 `git log --oneline` 里按 "多设备" 关键字查到。
