# HybridFileXfer 电脑接收器（Rust + 图形界面）

多轨快传的手机端可以把文件直接"发送到电脑"。这个接收器在电脑后台运行，负责：

1. 自动发现插在电脑上的手机（通过 adb），端口转发后连接；
2. 用 adb 拉起手机端的后台服务，**插上数据线就自动连上**；
3. 把手机发来的文件写进你设置的接收文件夹（手机界面上显示的"电脑主目录"）；
4. 拔线 / 断线后自动回去等待，下次插上继续用。

## 连接方式（自适应）

接收器按下面的顺序自动选择，不需要你手动配置：

1. **数据线（adb）**：检测到 adb 设备就端口转发并拉起手机服务；
2. **局域网发现**：手机端每 2 秒广播一次 `HFX1 <端口> <设备名>`（UDP 5741），
   电脑收到后直接连它的 IP —— 不用数据线、不用开 USB 调试、也不用输 IP；
3. **指定 IP**：`-c 192.168.1.5` 或界面里选"指定 IP"。

连上之后，手机端会把**所有可用网卡**（USB_ADB、WLAN、以太网……）都报给电脑，
电脑能连上的通道会**同时使用**：数据线和 WIFI 一起跑；
连不上的通道（比如不在同一网段）会被自动跳过，不会影响其它通道。

## 两个程序

| 程序 | 说明 |
| --- | --- |
| `HybridFileXfer-Receiver.exe` | **图形界面版**，日常使用这个：可以选文件夹、开始/停止、看日志 |
| `HybridFileXfer-Receiver-cli.exe` | 命令行版，适合开机脚本 / 自动化 |

## 图形界面版

1. 双击 `HybridFileXfer-Receiver.exe`；
2. **接收文件夹**后面点"浏览…"选择想保存的位置（默认 `%USERPROFILE%\Downloads\HybridFileXfer`）；
3. 点 **开始接收**，状态卡片变成"已连接电脑"就说明连上了；
4. 想开机自动在后台等手机，勾选 **开机自动启动**（会在启动文件夹里放一个 vbs，开机后最小化运行）。

界面上的信息：

- 状态卡片：未开始 / 等待手机连接 / 已连接电脑（显示通道名）/ 正在重试；
- 接收设置：文件夹、端口（默认 5740）、目标设备（多台手机时用命令行的 `-s`）；
- 运行日志：实时滚动，同时写进 `receiver.log`；
- 右上角可以打开接收文件夹，或者打开日志文件。

## 命令行版

```text
HybridFileXfer-Receiver-cli.exe                自动检测 USB 手机（默认）
HybridFileXfer-Receiver-cli.exe -c 192.168.1.5  直接连接指定 IP（WIFI）
HybridFileXfer-Receiver-cli.exe -c discover     只用局域网发现（UDP 广播）
HybridFileXfer-Receiver-cli.exe -d D:\接收       设置并保存接收文件夹
HybridFileXfer-Receiver-cli.exe -s <序列号>       多台设备时只连其中一台
HybridFileXfer-Receiver-cli.exe --print-config  查看当前配置
HybridFileXfer-Receiver-cli.exe -v              输出调试信息
```

无界面的开机自启（如果你想连窗口都不要）：

- 双击 `install-autostart.bat`（把命令行版放进启动项，无窗口）
- 取消用 `uninstall-autostart.bat`

## 编译

需要 **Rust 1.85 或更新版本**（GUI 依赖 eframe，新版依赖要求 edition 2024）。

双击 `build.bat`，或手动：

```bat
cargo build --release
```

产物在 `target\release\`，`build.bat` 会自动复制到本目录。

> 如果你的 `cargo` 不是 rustup 管理的（比如独立安装的旧版），
> 请把 `%USERPROFILE%\.cargo\bin` 加到 PATH，或设置 `RUSTC` 指向新版 rustc。

## config.ini

```ini
receive_dir = C:\Users\你\Downloads\HybridFileXfer   # 接收文件夹
port = 5740                                         # 控制通道端口，与手机端一致
adb =                                               # adb.exe 路径，留空自动查找
device =                                            # 只连接这台设备，留空任意
service_component = top.weixiansen574.hybridfilexfer/.ServerService
lang = zh                                           # zh / en
```

adb 的查找顺序：本程序目录 → `platform-tools\adb.exe` → 上一级的 `HybridFileXfer-PC\adb.exe` → PATH。
仓库里已经带了 `adb.exe`，所以一般不用管。

## 手机端需要什么

- 安装带有"发送到电脑"功能的多轨快传 APK；
- 手机打开 **USB 调试**，允许这台电脑调试（只需要一次）；
- 手机端应用里允许"所有文件访问权限"，并保持设置的 **自动连接** 开启。

| 场景 | 操作 |
| --- | --- |
| 数据线连接 | 接收器开着，插上数据线 1~2 秒自动连上 |
| 同一 WIFI | 手机保持自动连接，电脑用 `-c 手机IP` 启动命令行版 |
| 发送文件 | 手机文件管理器长按文件 → 分享/发送 → **发送到电脑**；或"用其他方式打开"里选 **发送到电脑** |

## 日志

- 界面里的运行日志；
- `receiver.log`：带时间戳的完整日志，方便事后排查。

## 限制

- 接收器只做"手机 → 电脑"方向。要用双栏文件管理器（电脑 → 手机、浏览目录）继续使用原来的 Java 客户端 `HybridFileXfer-PC`。
- USB 方式依赖 adb，必须打开 USB 调试；不想开可以用同一 WIFI 的 IP 方式。
- 如果手机端只勾选了 WIFI 网卡，电脑必须能访问到该网卡的 IP 才能连上。