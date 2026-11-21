//! 图形界面版接收器（Slint + fluent 样式，Windows 11 观感）。

// 一律使用 Windows 子系统：GUI 版不要控制台窗口（日志写 receiver.log）
#![windows_subsystem = "windows"]

slint::include_modules!();

use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use hfx_receiver::config::{self, Config};
use hfx_receiver::engine::{self, ConnectMode, EngineOptions, EngineState};
use hfx_receiver::i18n::{self, Lang};
use hfx_receiver::log;
use slint::ComponentHandle;

#[cfg(windows)]
mod tray_win;

/// 窗口标题（托盘线程靠它找到主窗口）
const WINDOW_TITLE: &str = "多轨快传";
/// 传输窗口标题（"正在传输" / "已完成" / "接收失败" 会变，所以只在需要时按它找主窗口）
#[allow(dead_code)]
const TRANSFER_TITLE: &str = "正在传输";
/// 取消确认框的窗口标题（app.slint 里 CancelAskWindow 的 title，两边要一致）
const ASK_TITLE: &str = "取消接收";
/// 「发送到手机」小窗的窗口标题（app.slint 里 PushWindow 的 title，两边要一致）。
/// 故意**不随状态变**（「正在发送 / 已发送」都写在窗口内容里）：gui 靠标题找 hwnd 来贴
/// 右下角 + 置顶，标题一变就有一帧找不到窗口（TransferWindow 那边踩过，见 tray_win.rs）。
const PUSH_TITLE: &str = "发送到手机";

struct EngineHandle {
    stop: Arc<AtomicBool>,
    /// 引擎线程。**必须 join，不能 drop 掉就完事** ——
    /// 真机 2026-11-21：传输中途点 × 取消、再点停止、再点开始，手机上出现「电脑 A / 电脑 B」。
    /// 原因就在这：停止只把 stop 置位就把 handle 丢了，线程 detach 继续跑
    /// （它可能正卡在 handle_receive 等通道 / rebuild_transfers 的 45s 读 / read_result 的 120s 超时里，
    /// 这几个循环都不读 stop）；紧接着点开始，engine 已经是 None 于是又起一条引擎线程、
    /// 又建一条控制通道，手机端按上限允许 3 条、也从不按对端 IP 去重，就合法地注册成两台电脑。
    join: Option<JoinHandle<()>>,
}

struct State {
    config: Config,
    config_path: PathBuf,
    exe_dir: PathBuf,
    engine: Option<EngineHandle>,
    state: EngineState,
    state_rx: Receiver<(String, EngineState)>,
    state_tx: Sender<(String, EngineState)>,
    devices: std::collections::HashMap<String, EngineState>,
    device_order: Vec<String>,
    /// serial → 手机自己报的名字（「Xiaomi Pad 7 Pro」）。等到之前先用序列号前 8 位顶着，
    /// 免得设备刚连上来那一下界面上是个空名字。
    device_names: std::collections::HashMap<String, String>,
    /// 后台线程解析好的名字从这儿回来（解析要跑 adb shell getprop，不能放 UI 线程）
    name_rx: Receiver<(String, String)>,
    name_tx: Sender<(String, String)>,
    /// 已经派出去解析、但还没回来的 serial，防止每个 tick 都重复起线程
    name_pending: std::collections::HashSet<String>,
    expanded: std::collections::HashSet<String>,
    expanded_dirty: bool,
    /// 取消接收后要贴到状态卡下面的那句话，等这次接收真正收完尾（RECEIVING 落下去）
    /// 再显示出来 —— 「已删除」不能比实际删文件早一句话。
    cancel_note: Option<String>,
}

impl State {
    fn running(&self) -> bool {
        self.engine.is_some()
    }

    fn stop_engine(&mut self) {
        if let Some(mut handle) = self.engine.take() {
            handle.stop.store(true, Ordering::Relaxed);
            log::info(i18n::tr("正在停止接收…", "stopping…"));
            // 等它真的退出再往下走：不等就会出现上面注释里那两台电脑。
            // 引擎线程已经被 stop 唤醒，正常情况几十毫秒就回来；卡在长超时里的
            // 那几种收尾（重建 45s / 结论 120s）会由 session.rs 里新增的 stop 检查
            // 立刻打断，所以这里不会把界面拖住。
            if let Some(join) = handle.join.take() {
                let _ = join.join();
            }
        }
        // 停就是停：设备表、排队中的状态一起清掉。
        // 以前只把 engine 摘掉，设备表里还留着「已连接」那条，而界面上的「是否在接收」
        // 又是按设备数算的 —— 点一次停止，界面自己又跳回「已连接/停止接收」，
        // 用户得再点一次才真的停（线程这时才收完尾）。
        self.devices.clear();
        self.device_order.clear();
        // 名字留着：同一台手机再连上来不用再问一遍（序列号不会变）
        self.name_pending.clear();
        self.state = EngineState::Stopped;
        self.expanded_dirty = true;
        while self.state_rx.try_recv().is_ok() {}
    }

    fn start_engine(&mut self, port: u16, mode: ConnectMode, dir: PathBuf) {
        if self.engine.is_some() {
            return;
        }
        self.config.receive_dir = dir.clone();
        self.config.port = port;
        let _ = self.config.save(&self.config_path);

        let options = EngineOptions {
            mode,
            port,
            receive_dir: dir,
            device: self.config.device.clone(),
            component: self.config.service_component.clone(),
            adb_path: self.config.adb_path.clone(),
            adb_search_dir: self.exe_dir.clone(),
            local_port: port,
            pinned: false,
        };
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let tx = Arc::new(Mutex::new(self.state_tx.clone()));
        let sink: engine::MultiStateSink = Arc::new(move |device: &str, state| {
            if let Ok(sender) = tx.lock() {
                let _ = sender.send((device.to_string(), state));
            }
        });
        let max_devices = self.config.max_devices;
        // engine::run_multi 是阻塞的，必须自己开线程
        let join = std::thread::spawn(move || {
            engine::run_multi(options, stop_for_thread, max_devices, sink);
        });
        self.engine = Some(EngineHandle {
            stop,
            join: Some(join),
        });
        self.state = EngineState::Waiting("正在启动…".to_string());
    }
}

/// 独立传输窗口的「落位 / 置顶」状态。
///
/// 窗口只有一个实例，跨轮次复用，所以这份状态也只有一个：
///   placed_at = 这一轮把它摆在的坐标（物理像素）；和用户拖到的位置比一下就知道他拖过没有；
///   settle    = 还要补几次「摆位置 + 置顶」—— 窗口刚 show 出来时 hwnd 还没建好、
///               位置也可能还没生效，补几帧就稳了；补完就再也不动它，免得把用户拖到的位置抢回去。
#[cfg(windows)]
#[derive(Default)]
struct TwWindow {
    placed_at: Option<(i32, i32)>,
    settle: u8,
    hwnd: Option<tray_win::HwndVal>,
    /// 已经按「贴工作区右下角」摆好了（用户拖过就不再抢）
    anchored: bool,
    /// 已经置顶过（省得每帧都 SetWindowPos）
    pinned: bool,
}

/// 补几次落位（150ms 一帧 → 约 0.9 秒）。之后再动窗口就是抢用户的拖动。
#[cfg(windows)]
const TW_SETTLE_TICKS: u8 = 6;
/// 传输窗口的可见右下角离工作区右下角多少像素（和资源管理器那个弹窗一个量级）
#[cfg(windows)]
const TW_MARGIN: i32 = 24;

#[cfg(windows)]
impl TwWindow {
    fn begin(&mut self) {
        *self = TwWindow::default();
        self.settle = TW_SETTLE_TICKS;
    }

    /// 每帧调一次：把窗口摆到位 + 置顶。hwnd 要等窗口真建出来才拿得到。
    ///
    /// 摆位置分两种：
    ///   * 用户拖过（config 里有坐标）→ 照那个坐标（show 之前已经 set_position 过一次）；
    ///   * 没拖过 → 贴着工作区右下角。这一步用「量一下现在的可见右下角离工作区还差多少、
    ///     再挪多少」的写法（tray_win::nudge_to_bottom_right），不去猜标题栏和边框多高，
    ///     多显示器也是按窗口自己所在那块屏算。
    fn tick(&mut self, tw: &TransferWindow, saved: Option<(i32, i32)>) {
        if self.settle == 0 {
            return;
        }
        self.settle -= 1;
        if self.hwnd.is_none() {
            // 推送小窗也是「本进程里带标题的窗口」，不排掉会被当成传输窗口摆位置/置顶
            self.hwnd = tray_win::find_transfer_window(WINDOW_TITLE, &[PUSH_TITLE]);
        }
        let Some(hwnd) = self.hwnd else { return };
        if !self.anchored {
            if saved.is_some() {
                // 记住的位置：show 之前已经放好了，这里只是再确认一次
                tw.window().set_position(slint::PhysicalPosition::new(saved.unwrap().0, saved.unwrap().1));
                self.anchored = true;
            } else if tray_win::nudge_to_bottom_right(hwnd, TW_MARGIN) {
                self.anchored = true;
            }
        }
        if !self.pinned {
            tray_win::set_topmost(hwnd);
            self.pinned = true;
        }
        // 记下最终位置：用户之后把它拖走，save_transfer_pos 一对比就知道该存了
        if let Some(pos) = tray_win::window_position(hwnd) {
            self.placed_at = Some(pos);
        }
    }
}

/// 窗口这一轮的物理像素尺寸（逻辑尺寸 × 缩放）。拿不到就用 .slint 里的 preferred-*。
#[cfg(windows)]
fn transfer_window_px(tw: &TransferWindow) -> (i32, i32) {
    let scale = tw.window().scale_factor();
    let size = tw.window().size();
    if size.width > 0 && size.height > 0 {
        (size.width as i32, size.height as i32)
    } else {
        // 470 x 196 是 TransferWindow 的 preferred-*（收起状态）
        ((470.0 * scale) as i32, (196.0 * scale) as i32)
    }
}

/// 收起 / 关掉窗口时，如果用户把它拖到别处了，就把坐标记进 config.ini。
#[cfg(windows)]
fn save_transfer_pos(tw: &TransferWindow, state: &mut State, placed: Option<(i32, i32)>) {
    let pos = tw.window().position();
    // (0,0) 一般是「拿不到位置」而不是真的拖到了左上角
    if pos.x == 0 && pos.y == 0 {
        return;
    }
    if let Some((px, py)) = placed {
        // 和我们摆的位置一样 → 用户没拖过，不用写盘
        if (pos.x - px).abs() <= 4 && (pos.y - py).abs() <= 4 {
            return;
        }
    }
    if state.config.xfer_win_pos == Some((pos.x, pos.y)) {
        return;
    }
    state.config.xfer_win_pos = Some((pos.x, pos.y));
    let path = state.config_path.clone();
    let config = state.config.clone();
    if let Err(e) = config.save(&path) {
        log::warn(&format!("保存传输窗口位置失败: {e}"));
    }
}

// ---------------------------------------------------------------------------
// 「发送到手机」小窗：落位状态 + 三步状态机
//
// 以前右键推送 = 把整个接收器主界面弹出来，再靠里面那张「选择接收设备」卡片等你去点
// （用户的原话：推送不要跳转到接收器界面）。现在推送只弹这个 420 宽的小窗：
// 选设备 → 发送中 → 已发送，走完自己收掉，主界面一次都不露。
// ---------------------------------------------------------------------------

/// 小窗的落位 / 置顶状态，和 TwWindow 一个路子：
/// 窗口刚 show（或刚换屏、高度变了）时 hwnd 还没建好、位置也可能还没生效，
/// 补几帧「量一下现在的右下角离工作区还差多少 → 挪过去」就稳了；
/// 补完就不再动它（免得把用户拖到的位置抢回去）。
#[cfg(windows)]
#[derive(Default)]
struct PushWinState {
    hwnd: Option<tray_win::HwndVal>,
    settle: u8,
    /// 这一轮已经抢过前台（每次推送只抢一次：换屏不抢，别把用户正在敲的东西打断）
    focused: bool,
}

/// 补几次落位（150ms 一帧 → 约 1.2 秒）。换屏时高度会变，也得重新补一遍。
#[cfg(windows)]
const PUSH_SETTLE_TICKS: u8 = 8;

#[cfg(windows)]
impl PushWinState {
    /// 每次 show / 换屏都调：重新找 hwnd，补几帧落位 + 置顶
    fn begin(&mut self) {
        self.hwnd = None;
        self.settle = PUSH_SETTLE_TICKS;
    }

    fn tick(&mut self) {
        if self.settle == 0 {
            return;
        }
        self.settle -= 1;
        if self.hwnd.is_none() {
            self.hwnd = tray_win::find_window_titled(PUSH_TITLE);
        }
        let Some(hwnd) = self.hwnd else { return };
        // 传输窗口自己是 HWND_TOPMOST（§6.5）：小窗不跟着置顶就会被它盖住，
        // 用户看着小窗在下面、鼠标点下去命中的是上面那个置顶的传输窗口。
        tray_win::set_topmost(hwnd);
        if !self.focused {
            // 刚弹出来时抢一次前台：不然窗口藏在别的程序后面，等于没弹
            tray_win::bring_to_front(hwnd);
            self.focused = true;
        }
        tray_win::nudge_to_bottom_right(hwnd, TW_MARGIN);
    }
}

/// 把推送小窗摆到工作区右下角 → 显示 → 立刻补一帧落位。
///
/// 和 show_transfer_window 一个套路：先按估算尺寸摆个大概位置（省得在系统默认位置
/// 闪一下），真实边框和尺寸由 PushWinState::tick 的 nudge_to_bottom_right 量出来再吸过去。
#[cfg(windows)]
fn show_push_window(pw: &PushWindow, state: &mut PushWinState) {
    // 只在「尺寸还量不出来」时用（窗口还没显示过）
    const EST_W: f32 = 420.0;
    const EST_H: f32 = 240.0;
    let scale = pw.window().scale_factor();
    let size = pw.window().size();
    let (w, h) = if size.width > 0 && size.height > 0 {
        (size.width as i32, size.height as i32)
    } else {
        ((EST_W * scale) as i32, (EST_H * scale) as i32)
    };
    // +16 / +40 是边框和标题栏的估算值，只影响这一两帧
    let origin = tray_win::transfer_window_origin(w + 16, h + 40, None);
    pw.window()
        .set_position(slint::PhysicalPosition::new(origin.0, origin.1));
    state.begin();
    let _ = pw.show();
    state.tick();
}

/// 「发送到手机」小窗的三步状态机，存在 Arc<Mutex<..>> 里跨两处用：
/// Slint 回调（点设备 / 取消，UI 线程）只改这份状态，真正往窗口上刷
/// （set_step / set_fill / set_file_name …）统一由主循环那个 150ms 的 tick 做。
const PUSH_IDLE: i32 = 0;
const PUSH_CHOOSE: i32 = 1;
const PUSH_SENDING: i32 = 2;
const PUSH_DONE: i32 = 3;

struct PushRound {
    /// 这一轮对应的推送代次（pending::generation()）
    gen: u64,
    step: i32,
    serial: Option<String>,
    /// 这一轮要发给谁：单选是 1 个，多选就是打勾的那几台（顺序 = 设备表顺序）
    serials: Vec<String>,
    target_name: String,
    /// 这一批：文件数 + 总字节（点设备那一刻从队列量的）
    files: usize,
    bytes: u64,
    /// 手机真的开始取文件了（control::sending() 见到过 true）
    saw_sending: bool,
    /// 进第 2 屏的时刻（连不上 / 一直不来取要有个超时说法）
    started_at: Option<std::time::Instant>,
    failed: bool,
    fail_text: String,
    /// 第 3 屏出现的时刻（成功 2 秒后自动关窗）
    done_at: Option<std::time::Instant>,
    /// 速率取样：上一次的「已发字节」和时刻
    last_bytes: u64,
    last_at: std::time::Instant,
    speed: f64,
}

impl Default for PushRound {
    fn default() -> Self {
        PushRound {
            gen: 0,
            step: PUSH_IDLE,
            serial: None,
            serials: Vec::new(),
            target_name: String::new(),
            files: 0,
            bytes: 0,
            saw_sending: false,
            started_at: None,
            failed: false,
            fail_text: String::new(),
            done_at: None,
            last_bytes: 0,
            last_at: std::time::Instant::now(),
            speed: 0.0,
        }
    }
}

impl PushRound {
    /// 目标选定了（用户点的那台 / 打勾的那几台 / 只有一台时自动选）：进第 2 屏
    fn begin_sending(&mut self, serials: Vec<String>, name: String, files: usize, bytes: u64) {
        self.reset();
        self.step = PUSH_SENDING;
        self.serial = serials.first().cloned();
        self.serials = serials;
        self.target_name = name;
        self.files = files;
        self.bytes = bytes;
        self.started_at = Some(std::time::Instant::now());
        self.last_at = std::time::Instant::now();
    }

    /// 多选：队列里还剩没被取走的条目数（多台时任何一台来取都算在动）
    fn queued_left(&self) -> usize {
        if self.serials.len() > 1 {
            hfx_receiver::pending::totals().0
        } else {
            hfx_receiver::pending::totals_for(self.serial.as_deref().unwrap_or("")).0
        }
    }

    /// 进第 3 屏：failed=false 时 2 秒后自动关，true 就挂着等用户看
    fn finish(&mut self, failed: bool, text: String) {
        self.step = PUSH_DONE;
        self.failed = failed;
        self.fail_text = text;
        self.done_at = Some(std::time::Instant::now());
        self.speed = 0.0;
    }

    fn reset(&mut self) {
        *self = PushRound::default();
    }
}

/// 设备串号（排好序）：主界面、推送小窗用同一个顺序，行下标才对得上
fn sorted_serials(s: &State) -> Vec<String> {
    let mut serials: Vec<String> = s.devices.keys().cloned().collect();
    serials.sort();
    serials
}

/// 设备真名（手机自己报的）；还没解析出来就用序列号前 8 位顶着
fn device_display_name(s: &State, serial: &str) -> String {
    s.device_names
        .get(serial)
        .cloned()
        .unwrap_or_else(|| serial.chars().take(8).collect())
}

/// 推送小窗第 1 屏每台设备下面那行灰字
fn device_status_text(state: &EngineState) -> String {
    match state {
        EngineState::Connected(channels) => format!("已连接 · {} 条通道", channels.len()),
        EngineState::Waiting(_) => "正在连接…".to_string(),
        // 协议版本不匹配那种：一直连不上、还在重试。行照样列出、照样能点（用户自己决定）
        EngineState::Retrying(_) => "连不上 · 重试中".to_string(),
        EngineState::Stopped => "已断开".to_string(),
    }
}

/// 圆形头像里那个字：名字首字母（中文名就是第一个字）
fn device_avatar(name: &str) -> String {
    name.chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "·".to_string())
}

/// 推送小窗第 1 屏的设备列表（顺序和 sorted_serials 一致，点第几台就是第几个串号）
fn push_device_rows(s: &State, checked: &[String]) -> Vec<PushDevice> {
    let mut rows = Vec::with_capacity(s.devices.len());
    for serial in sorted_serials(s) {
        let name = device_display_name(s, &serial);
        let status = s
            .devices
            .get(&serial)
            .map(device_status_text)
            .unwrap_or_default();
        let selected = checked.iter().any(|c| c == &serial);
        rows.push(PushDevice {
            avatar: device_avatar(&name).into(),
            name: name.into(),
            status: status.into(),
            selected,
        });
    }
    rows
}

/// 「发送到 X、Y」那行字：一两台就点名，多了就「A 等 N 台」
/// 选完设备就把小窗收起来（用户要的「选完就可以关闭」）：
///
/// 进度和完成交给已有的「正在传输」小窗 —— 推送路径本来就不许弹接收器任何界面，
/// 再多挂一个只显示进度的窗，只会跟传输小窗叠在一起，看着像弹了两个东西。
/// 失败时才由状态机把窗口重新弹出来说明原因。
fn hide_after_choose(pw: &slint::Weak<PushWindow>) {
    if let Some(w) = pw.upgrade() {
        let _ = w.hide();
    }
}

fn push_target_label(s: &State, serials: &[String]) -> String {
    let names: Vec<String> = serials.iter().map(|x| device_display_name(s, x)).collect();
    match names.len() {
        0 => "手机".to_string(),
        1 => names[0].clone(),
        2 => format!("{}、{}", names[0], names[1]),
        n => format!("{} 等 {} 台", names[0], n),
    }
}

/// 这一批的「3 个文件 · 128.4 MB」
fn push_summary(files: usize, bytes: u64) -> String {
    if files == 0 {
        return human(bytes);
    }
    format!("{} 个文件 · {}", files, human(bytes))
}

// ---------------------------------------------------------------------------
// 「取消接收」的四个落点：主界面和独立传输窗口共用这一份
//
// 点取消 → 弹「保留 / 不保留 / 继续接收」→ 按选择收尾（停止发生在选的这一刻，不在弹框这一刻）。
// 两个窗口注册的回调都是下面这几个函数，语义不许长出第二套。
//
// 为什么弹框时**不**先暂停：暂停超过约 25 秒，手机端就判这次传输超时、直接关掉连接
// （真机日志：15:38:45「手机端暂停了传输」→ 15:39:12「接收已取消 / 重建传输通道失败:
// failed to fill whole buffer」）。用户要是盯着确认框想一会儿，框还在、传输已经没了，
// 三个选项全成了摆设。而「保留 / 不保留」只决定半截文件怎么处置，跟这几秒要不要继续收
// 没有关系 —— 先继续收，用户一选再停，反而更实在（也顺手多收了几十兆）。
// ---------------------------------------------------------------------------

/// 取消被点了一下。返回 false = 这次点击被忽略（没在接收 / 已经在取消中），
/// 界面据此不弹确认框 —— 同一轮接收只允许取消一次。
fn cancel_recv_request() -> bool {
    if !hfx_receiver::control::receiving() {
        log::warn(i18n::tr(
            "按钮：取消接收被忽略（当前没有接收在进行）",
            "cancel receiving ignored: nothing is being received",
        ));
        return false;
    }
    if hfx_receiver::control::cancel_requested() {
        // 第一次取消已经在收尾 / 重建里了：这次点击直接复用上一次的结果，
        // 绝不再往控制通道插第二帧（那会和手机端的重建握手抢帧序）。
        log::warn(i18n::tr(
            "按钮：取消接收被忽略（这次取消已经受理过）",
            "cancel receiving ignored: this cancel was already accepted",
        ));
        return false;
    }
    log::warn(i18n::tr(
        "按钮：取消接收（弹确认框；传输先不停，等用户选完再停）",
        "button: cancel receiving (dialog shown; transfer keeps running until the user chooses)",
    ));
    true
}

/// 保留：收尾时留着半截文件和 .hfxpart（下次同一个文件接着传）
fn cancel_keep(st: &Arc<Mutex<State>>) {
    if !hfx_receiver::control::cancel_receive(hfx_receiver::control::CancelChoice::Keep) {
        // 幂等：这次取消已经受理过（收尾 / 重建还没做完）时不再做任何事，
        // 上一次的结果原样复用，界面不重复提示、控制通道不插第二帧。
        log::warn(i18n::tr(
            "按钮：取消接收 · 保留进度被忽略（这次取消已经受理过）",
            "cancel · keep ignored: this cancel was already accepted",
        ));
        return;
    }
    log::warn(i18n::tr(
        "按钮：取消接收 · 保留进度（半截文件和 .hfxpart 留着，可续传）",
        "button: cancel receiving · keep the partial file (resumable)",
    ));
    if let Ok(mut s) = st.lock() {
        s.cancel_note = Some(
            i18n::tr(
                "已取消：保留已收到的部分，下次同一个文件可续传",
                "cancelled: what already arrived is kept and can be resumed",
            )
            .to_string(),
        );
    }
}

/// 不保留：收尾（Receiver::finish_with(false)）会把没传完的文件和续传记录删掉
fn cancel_discard(st: &Arc<Mutex<State>>) {
    if !hfx_receiver::control::cancel_receive(hfx_receiver::control::CancelChoice::Discard) {
        log::warn(i18n::tr(
            "按钮：取消接收 · 不保留被忽略（这次取消已经受理过）",
            "cancel · discard ignored: this cancel was already accepted",
        ));
        return;
    }
    log::warn(i18n::tr(
        "按钮：取消接收 · 不保留（收尾删掉没传完的文件和续传记录）",
        "button: cancel receiving · discard the incomplete files",
    ));
    if let Ok(mut s) = st.lock() {
        s.cancel_note = Some(
            i18n::tr(
                "已取消：已删除未传完的文件和续传记录",
                "cancelled: the incomplete files and their resume journals were removed",
            )
            .to_string(),
        );
    }
}

/// 确认框里选了「继续接收」：把刚才为了弹框加的暂停放掉，传输照常
fn cancel_dismiss() {
    // 弹框时没有暂停，所以这里也不用放暂停 —— 传照常（以前这里负责把"为了弹框而加的暂停"放掉）
    log::info(i18n::tr(
        "取消接收确认框：选了「继续接收」",
        "cancel dialog: keep receiving",
    ));
}

/// 剩余时间的说法（速率为 0 时调用方不要用这个）
fn human_eta(secs: u64) -> String {
    if secs >= 3600 {
        format!("{} 小时 {} 分", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{} 分 {} 秒", secs / 60, secs % 60)
    } else {
        format!("{secs} 秒")
    }
}

/// 传输窗口收起时的高度（逻辑像素），和 app.slint 的 preferred-height 对得上。
///
/// 2026-11-19「对齐资源管理器」那一轮从 151 改成 156，逐项是：
/// 14(上留白) + 20(主文案) + 6 + 30(百分比行) + 13(百分比行到条，原 12)
/// + 15(条) + 15(条到分隔线，原 0 —— 原来线贴着条子画) + 1(分隔线)
/// + 10(线到「详细信息」行，原 9) + 22(「详细信息」行，原 20) + 10(下留白)。
const TW_H_COLLAPSED: f32 = TW_TOP_PART + TW_BAR_BLOCK + TW_DIVIDER + TW_TOGGLE_TAIL; // = 156.0
/// 收起态里「分段进度条」那一整块：13(百分比行到条) + 15(条) + 15(条到分隔线) = 43。
/// 展开态这一块**整块不画**（app.slint 里用 if !root.details 包起来，不是 visible:false
/// —— 那还占着高度），省下的 43px 换成详情区。
const TW_BAR_BLOCK: f32 = 43.0;
/// 顶端那段和进度条无关的部分：14(上留白) + 20(主文案) + 6 + 30(百分比行) = 70。
/// 收起 / 展开两种状态都一模一样。
const TW_TOP_PART: f32 = 70.0;
/// 分隔线 1px（两条状态都有；展开态在它上面还有下面「简略信息」那条，见 TW_DETAIL_TAIL）。
const TW_DIVIDER: f32 = 1.0;
/// 展开态尾部：10(行区下留白) + 1(「简略信息」上面那条分隔线) + 10 + 22(开关行) + 10(下留白)。
const TW_DETAIL_TAIL: f32 = 10.0 + 1.0 + 10.0 + 22.0 + 10.0;
/// 展开态：分段条不画之后，百分比行到分隔线仍然留的那一口气（app.slint 里 if root.details 的 13px）。
/// 不这么留，那条分隔线会直接贴到 20px 大字的下沿。
const TW_DETAIL_GAP: f32 = 13.0;
/// 展开态详情区的固定高度（和行数无关的部分）：
///   10(分隔线到「名称：」) + 18*3(名称/剩余时间/剩余项目，原生这三行行距 18)
///   + 8(三行到速度图表条) + 80(速度图表条) + 8(图表条到第一条通道速率)
const TW_DETAIL_BODY: f32 = 10.0 + 18.0 * 3.0 + 8.0 + TW_CHART_H + 8.0;
/// 展开态那条「速度图表条」的高度，80px，和 app.slint 里 chart 的 height 一模一样。
/// 参考图 native-details.png 逐行数：y=92 外框 / 93 内侧 / 94..141 上层 48 / 142 黑基线 /
/// 143..169 下层实心 27 / 170 内侧 / 171 外框 = 80 行（y=172 起是纯白）。
/// 原先这里是 15px 的「大聚合条」，用户拍板要真 1:1 的图表条，换成 80。
const TW_CHART_H: f32 = 80.0;
/// 收起态开关行那一串：10(分隔线到行) + 22(「详细信息」行) + 10(下留白) = 42。
/// 展开态它挪到最底下、中间还多插一条分隔线，见 TW_DETAIL_TAIL。
const TW_TOGGLE_TAIL: f32 = 10.0 + 22.0 + 10.0;
/// 明细一行的高度（app.slint 里 for 循环的 height: 22px）
const TW_ROW_H: f32 = 22.0;

/// 图表速度曲线的**进度分桶数**：把 0..100% 切成这么多格，每格记那一刻的速率。
///
/// 400 格 = 每格 0.25% 的进度。600 MB 的文件一格 1.5 MB，10 MB/s 时每秒填 6~7 格 ——
/// 曲线推进的颗粒度足够细，同时格数又不会让 Path 的 commands 字符串长得离谱。
///
/// 为什么不用「环形缓冲 + 抽稀」：抽稀步长会随采样总数变化，同一个进度位置上
/// 这一帧取到的是这个点、下一帧是那个点，整条已经画过的曲线会抖、会扭。
/// 固定分桶之后第 i 格永远是进度 i/400 处的速率，写进去就不动，曲线只向右长。
const CURVE_BINS: usize = 400;
/// 相邻两次采样的最小间隔（毫秒）。
///
/// 这是曲线「有没有起伏」的关键。原生那条曲线每个完成量区间的高度都不一样
/// （naive-00 逐列量：35,35,35,19,20,16,12,13,17,20,18,13,12），
/// 也就是采样比进度细得多 —— 一个 x 像素里能放进好几个速率读数。
///
/// 以前给 200ms（和速率窗口对齐）：6GB 文件传 8 分钟，200ms 一拍总共才采 2400 次，
/// 分到 400 个桶里每桶 6 个值、但**同一时刻只填 1 个桶**，于是每桶都被写成一个常数，
/// 曲线就成了一条水平带子（真机截图正是如此）。
/// 现在给 40ms：比 16ms 的 tick 略慢，既不会重复采到同一个值，又能在 1% 进度里
/// 拿到好几个读数，曲线才会有真实的高低起伏。
const CURVE_SAMPLE_MS: u128 = 40;
/// 真正送进 Path 的折线点数上限。参考图那条曲线目视也就是这个量级，
/// 再密既看不出来，也让 commands 字符串白长。
/// 注意：抽稀的步长是 CURVE_BINS / CURVE_DRAW_POINTS 的**常数比**，不随进度变化 ——
/// 这正是「画过的曲线不会乱动」的关键（以前按当前点数算步长，每帧都在挪）。
const CURVE_DRAW_POINTS: usize = 200;

/// 传输窗口这一轮该多高：收起 156；展开 = 297 + 行数*22
/// （行数为 0 时按 1 行算：那时 app.slint 画的是「这一轮还没有分通道数据」那一行，同样 22px）。
/// 展开态的 297 = 70(上半) + 1(分隔线) + 13 + 160(详情固定部分，含 80 的图表条) + 53(尾部)，
/// 逐项见上面的常量。
/// 弹「取消」确认框时不用再撑高（它是独立窗口）。
/// 把传输窗口摆到该在的位置再显示。
///
/// 三处「要弹出来了」都走它（正常开始 / 失败 / 失败后重新弹）：先摆个大概位置（省得在系统默认
/// 位置闪一下），再让 `TwWindow::tick` 量出真实边框贴到工作区右下角 + 置顶。
/// 少走一处就是一次真机 bug —— 失败那一轮当初只 `show()` 过，窗口停在 (130,130)。
fn show_transfer_window(
    tw: &TransferWindow,
    s: &mut State,
    tw_state: &Arc<Mutex<TwWindow>>,
) {
    #[cfg(windows)]
    {
        // +16 / +40 是边框和标题栏的估算值，只影响这一两帧
        let (pw, ph) = transfer_window_px(tw);
        let origin = tray_win::transfer_window_origin(pw + 16, ph + 40, s.config.xfer_win_pos);
        if let Ok(mut g) = tw_state.lock() {
            g.begin();
            g.placed_at = Some(origin);
        }
        tw.window()
            .set_position(slint::PhysicalPosition::new(origin.0, origin.1));
    }
    #[cfg(not(windows))]
    let _ = (s, tw_state);
    let _ = tw.show();
    #[cfg(windows)]
    if let Ok(mut g) = tw_state.lock() {
        g.tick(tw, s.config.xfer_win_pos);
    }
}

fn transfer_window_height(details: bool, rows: usize, failed: bool) -> f32 {
    // 收起 / 展开两套公式，和 app.slint 的 content-h 一个字一个字对得上：
    //   收起 = 70 + 1 + 13 + 15 + 15 + 10 + 22 + 10 = 156
    //   展开 = 70(上半) + 1(分隔线) + 13(分段条不画了留的呼吸) + 160(详情固定，含 80 图表条)
    //          + 行数*22 + 53(尾部 10+1+10+22+10) = 297 + 行数*22
    // 每通道一行；rows 为 0 时画的是「这一轮还没有分通道数据」那一行，同样 22px，故 max(1)。
    let mut h = if details {
        TW_TOP_PART
            + TW_DIVIDER
            + TW_DETAIL_GAP
            + TW_DETAIL_BODY
            + rows.max(1) as f32 * TW_ROW_H
            + TW_DETAIL_TAIL
    } else {
        TW_H_COLLAPSED
    };
    // 「接收失败：<原因>」那一行（app.slint 里 height: 20px）
    if failed {
        h += 20.0;
    }
    // 取消确认框现在是独立窗口，不再需要把传输窗口撑到 216 高（TRANSFER-WINDOW.md §6.2）
    h
}

fn main() -> Result<(), slint::PlatformError> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    let config_path = config::config_path(&exe_dir);
    let config = Config::load(&config_path);
    // 307：把本机限速灌进限速器。握手时会把它报给手机，两边取非零最小值。
    // 放在这里（而不是第一次传输时）是因为**握手**就要用到它 ——
    // 而且进程启动时决定一次就够，之后改设置走 speed-limit-changed 回调再灌一次。
    hfx_receiver::ratelimit::set_configured_mb(config.speed_limit_mb as i64);
    let lang = config.lang.unwrap_or_else(detect_lang);
    i18n::set_lang(lang);

    // -v / --verbose：日志里多出调试行（比如每条通道"绑定本机哪个地址"），
    // 排查"梯子/虚拟网卡抢路由"这类问题时用得上
    let verbose = std::env::args().any(|a| a == "-v" || a == "--verbose");
    log::init(Some(exe_dir.join("receiver.log")), verbose);
    log::set_console(false);
    log::info(&format!(
        "GUI build={} pid={} exe={}",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    ));

    let (state_tx, state_rx) = mpsc::channel();
    // 设备名解析结果走这条通道回来（后台线程 → UI 线程）
    let (name_tx, name_rx) = mpsc::channel();

    // 清理上一次自我更新留下的旧文件
    let _ = std::fs::remove_file(exe_dir.join("HybridFileXfer-Receiver.exe.old"));

    remove_legacy_autostart_script();
    // Explorer「发送到 → 手机」：把选中文件投递给已运行的实例，投递成功就直接退出
    // 只要命令行里带了已存在的文件路径，就当作「发送到手机」。
    // 这样用户自己把快捷方式拖进 SendTo 即可，程序不需要拉 PowerShell 去造 .lnk
    //（无签名程序 + 拉起脚本 + 写快捷方式，很容易被杀软判成木马）。
    let mut send_files: Vec<PathBuf> = Vec::new();
    let mut send_target: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--send" || arg == "--autostart" || arg == "-v" || arg == "--verbose" {
            continue;
        }
        // --to <串号>：只把文件推给指定设备（多设备时用）
        if arg == "--to" {
            send_target = args.next();
            continue;
        }
        let path = PathBuf::from(&arg);
        if path.exists() {
            send_files.push(path);
        }
    }
    let send_mode = !send_files.is_empty();
    if send_mode {
        #[cfg(windows)]
        if tray_win::deliver_paths_with_target(&send_files, send_target.as_deref()) {
            return Ok(());
        }
        // 没有运行中的实例：这些文件先入队，本进程接着正常启动
        hfx_receiver::pending::push_with_target(send_files.clone(), send_target.clone());
    }

    // 单实例：已经有接收器在跑时，不再开第二个窗口（除非是"发送到手机"的投递进程）
    #[cfg(windows)]
    if tray_win::tray_window_handle().is_some() {
        if send_mode {
            // 走到这里说明投递失败（窗口还没建好），直接退出，别再开一个实例
            log::warn("已有实例在运行，投递未完成");
        } else {
            // 把已运行的窗口叫到前面，然后退出
            tray_win::show_main_window(WINDOW_TITLE);
        }
        return Ok(());
    }

    let autostart = autostart_enabled();
    let ip_text = std::env::var("HFX_LAST_IP").unwrap_or_else(|_| "192.168.1.100".to_string());

    let state = Arc::new(Mutex::new(State {
        config,
        config_path: config_path.clone(),
        exe_dir: exe_dir.clone(),
        engine: None,
        state: EngineState::Stopped,
        devices: std::collections::HashMap::new(),
        device_order: Vec::new(),
        device_names: std::collections::HashMap::new(),
        name_rx,
        name_tx,
        name_pending: std::collections::HashSet::new(),
        expanded: std::collections::HashSet::new(),
        expanded_dirty: false,
        cancel_note: None,
        state_rx,
        state_tx,
    }));

    let ui = AppWindow::new()?;
    // 独立传输窗口：只读同一份 PROGRESS，不参与主界面布局。
    // 传输一开始自己弹出来，结束自己收起 —— 主窗口最小化到托盘时也能看到进度。
    let xfer_win = TransferWindow::new()?;
    // 取消确认框的**独立窗口**（用户要求：不要再挤进传输窗口里）。
    // 传输窗口只负责把 cancel-ask 这个状态位翻成 true/false，这个窗口自己弹/收（见下面的定时器）。
    let ask_win = CancelAskWindow::new()?;
    // 「发送到手机」小窗：右键推送只弹它，不再把整个接收器主界面弹出来。
    // 三屏：选设备 / 发送中 / 已发送（见 app.slint 的 PushWindow + 上面的 PushRound）。
    let push_win = PushWindow::new()?;
    // 小窗的落位 / 置顶状态（Slint 回调和主循环共用）
    #[cfg(windows)]
    let push_state = Arc::new(Mutex::new(PushWinState::default()));
    // 小窗这一轮的状态机（回调和主循环共用）
    let push_round = Arc::new(Mutex::new(PushRound::default()));
    // 第 1 屏打勾的那几台（串号；顺序不重要，画的时候按设备表顺序比）
    let push_checked: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    // 用户在传输窗口上点了「收起」：这一轮就别再弹了（下一轮传输重新允许）
    let xfer_dismissed = Arc::new(AtomicBool::new(false));
    // 传输窗口的落位 / 置顶状态（收起回调也要用，所以放在外面共享）
    #[cfg(windows)]
    let tw_state = Arc::new(Mutex::new(TwWindow::default()));
    ui.set_dir_text(state.lock().unwrap().config.receive_dir.display().to_string().into());
    ui.set_port_text(state.lock().unwrap().config.port.to_string().into());
    ui.set_ip_text(ip_text.into());
    ui.set_autostart(autostart);
    ui.set_tray(state.lock().unwrap().config.minimize_to_tray);
    ui.set_sendto(state.lock().unwrap().config.sendto);
    ui.set_speed_limit_text(state.lock().unwrap().config.speed_limit_mb.to_string().into());
    ui.set_status_kind(0);
    // 之前勾选过就确保快捷方式还在（换电脑/程序挪位置后路径会变）
    if state.lock().unwrap().config.sendto {
        std::thread::spawn(|| {
            if let Err(e) = hfx_receiver::sendto::ensure() {
                log::warn(&format!("创建「发送到手机」快捷方式失败: {e}"));
            }
        });
    }

    // ---------- 回调 ----------
    {
        let ui_weak = ui.as_weak();
        ui.on_browse_clicked(move || {
            if let Some(dir) = pick_folder() {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_dir_text(dir.to_string_lossy().to_string().into());
                }
            }
        });
    }
    {
        let st = Arc::clone(&state);
        let ui_weak = ui.as_weak();
        // 开始接收：顺手把上一轮可能还开着的取消确认框（独立窗口）收掉
        let tw_weak_start = xfer_win.as_weak();
        ui.on_start_clicked(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut s = st.lock().unwrap();
            if s.running() {
                return;
            }
            let port: u16 = match ui.get_port_text().trim().parse() {
                Ok(p) => p,
                Err(_) => {
                    ui.set_message("端口号不正确".into());
                    return;
                }
            };
            let mode = if ui.get_mode_index() == 0 {
                ConnectMode::Auto
            } else {
                match ui.get_ip_text().trim().parse::<IpAddr>() {
                    Ok(ip) => ConnectMode::Ip(ip),
                    Err(_) => {
                        ui.set_message("IP 地址不正确".into());
                        return;
                    }
                }
            };
            let dir = PathBuf::from(ui.get_dir_text().trim());
            if dir.as_os_str().is_empty() {
                ui.set_message("请选择接收文件夹".into());
                return;
            }
            if let Err(e) = std::fs::create_dir_all(&dir) {
                ui.set_message(format!("无法创建接收文件夹：{}", e).into());
                return;
            }
            ui.set_message("".into());
            // 上一次取消留下的提示 / 没关掉的确认框，别带进这一次
            ui.set_notice_text("".into());
            if let Some(tw) = tw_weak_start.upgrade() {
                tw.set_cancel_ask(false);
            }
            hfx_receiver::control::pause(false);
            hfx_receiver::receiver::PROGRESS.reset();
            // 上一轮的「接收失败」提示别带进这一轮
            hfx_receiver::receiver::clear_failure();
            hfx_receiver::session::reset_send_segments();
            log::info("按钮：开始接收");
            s.start_engine(port, mode, dir);
            ui.set_running(true);
        });
    }
    {
        let st = Arc::clone(&state);
        let ui_weak = ui.as_weak();
        let tw_weak_stop = xfer_win.as_weak();
        ui.on_stop_clicked(move || {
            let mut s = st.lock().unwrap();
            log::info("按钮：停止接收");
            s.stop_engine();
            s.cancel_note = None;
            // 停就是停：弹了一半的取消确认框、为了弹框加的暂停，一起收掉
            hfx_receiver::control::pause(false);
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_running(false);
                ui.set_notice_text("".into());
            }
            if let Some(tw) = tw_weak_stop.upgrade() {
                tw.set_cancel_ask(false);
            }
        });
    }
    {
        let st = Arc::clone(&state);
        ui.on_autostart_toggled(move |enabled| {
            let s = st.lock().unwrap();
            if let Err(e) = set_autostart(enabled, &s.exe_dir) {
                eprintln!("设置开机自启失败: {e}");
            }
        });
    }
    {
        let st = Arc::clone(&state);
        ui.on_device_clicked(move |index| {
            let mut s = st.lock().unwrap();
            let Some(serial) = s.device_order.get(index as usize).cloned() else {
                return;
            };
            if !s.expanded.remove(&serial) {
                s.expanded.insert(serial);
            }
            s.expanded_dirty = true;
        });
    }
    {
        let st = Arc::clone(&state);
        ui.on_device_chosen(move |index| {
            let s = st.lock().unwrap();
            if let Some(serial) = s.device_order.get(index as usize) {
                hfx_receiver::pending::assign_all(serial);
                log::info(&format!("推送目标已选定：{}", serial));
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_toggle_pause_send(move || {
            let paused = !hfx_receiver::control::paused();
            hfx_receiver::control::pause(paused);
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_send_paused(paused);
            }
        });
    }
    {
        ui.on_cancel_send(move || {
            hfx_receiver::control::cancel();
        });
    }
    {
        // 传输窗口上那颗 × 在「电脑 -> 手机」时走的也是这里（见 app.slint 的
        // TransferWindow.sending-now）。和主窗口第 2 屏的「取消发送」是同一个动作。
        xfer_win.on_cancel_send(move || {
            hfx_receiver::control::cancel();
        });
    }
    // ---- 独立传输窗口上的两颗按钮（和主窗口同一套控制函数）----
    {
        let weak = xfer_win.as_weak();
        xfer_win.on_toggle_pause(move || {
            let paused = !hfx_receiver::control::paused();
            hfx_receiver::control::pause(paused);
            if let Some(w) = weak.upgrade() {
                w.set_paused(paused);
            }
        });
    }
    // ---- 流光的步进：照抄 Windows 原生的节奏 ----
    //
    // WPF 默认模板（ProgressBar.cs 的 UpdateAnimation）是这么摆的：
    //   startPos = -glowWidth，endPos = indicatorWidth + glowWidth，
    //   translateTime = (endPos - startPos) / 200.0   // 注释原文：travel at 200px/second
    //   pauseTime = 1.0s，Duration = translateTime + pauseTime，Linear、无限循环。
    // 也就是「匀速 200px/s 扫过整条，扫完停 1 秒，再来」。这里按同样的公式算相位，
    // flow = 亮带中心在条上的位置（单位：条宽），Slint 那边只负责按这个位置画。
    //
    // 窗口没显示 / 暂停 / 传完 / 速率为 0 时不动：进度不动还在闪，看着像卡死。
    const BAND_PX: f32 = 100.0; // 高光带宽（WPF 就是 100px）
    const SPEED_PX_S: f32 = 200.0; // 200 px/s（WPF 注释里的原值）
    const PAUSE_S: f32 = 1.0; // 扫完停 1 秒
    const BAR_PX: f32 = 428.0; // 条的实际像素宽：窗口 476 - 左右各 24 的内边距
    let flow_phase = std::cell::Cell::new(0.0f32);
    let flow_timer = slint::Timer::default();
    {
        let weak = xfer_win.as_weak();
        flow_timer.start(slint::TimerMode::Repeated, Duration::from_millis(16), move || {
            let Some(tw) = weak.upgrade() else { return };
            // details 也要算进去：展开态分段条整条隐藏、只留图表条，
            // 这时候还在每 16ms 改 flow，等于每帧无条件重绘 476x776 的整窗 ——
            // 而屏幕上一个像素都没变。实测这就是展开态吃 76% 单核的真凶
            // （收起态同一时刻只有 20%）。
            if !tw.window().is_visible()
                || tw.get_done()
                || tw.get_paused()
                || !tw.get_flow_on()
                || tw.get_details()
            {
                return;
            }
            // 填充区宽度（像素）：条宽 × 已填充比例
            let fill_px = (BAR_PX * tw.get_fill().clamp(0.0, 1.0)).max(1.0);
            // WPF：从 -带宽 扫到 填充宽 + 带宽 → 距离 = 填充宽 + 2×带宽
            let travel_px = fill_px + 2.0 * BAND_PX;
            let sweep_s = travel_px / SPEED_PX_S;
            let cycle_s = sweep_s + PAUSE_S;
            let mut phase = flow_phase.get() + 0.016;
            if phase >= cycle_s {
                phase -= cycle_s;
            }
            flow_phase.set(phase);
            // 扫描阶段线性走完 travel，剩下的时间（1 秒）停在最右边（已经在填充区外，看不见）
            let t = (phase / sweep_s).clamp(0.0, 1.0);
            let x_px = -BAND_PX + travel_px * t; // 亮带左边缘
            tw.set_flow((x_px + BAND_PX / 2.0) / BAR_PX);
        });
    }
    {
        let weak = xfer_win.as_weak();
        let dismissed = Arc::clone(&xfer_dismissed);
        #[cfg(windows)]
        let st = Arc::clone(&state);
        #[cfg(windows)]
        let tw_state = Arc::clone(&tw_state);
        xfer_win.on_close_window(move || {
            // 收起 = 藏起来，传输继续（要真停下得点窗口里的「取消」）
            // 失败那一轮点这颗 = 「我知道了」：把失败提示消费掉，下一轮正常弹回来
            hfx_receiver::receiver::clear_failure();
            dismissed.store(true, Ordering::Relaxed);
            if let Some(w) = weak.upgrade() {
                #[cfg(windows)]
                {
                    // 用户可能把窗口拖到别处了：收起时顺手记住
                    let placed = tw_state.lock().ok().and_then(|g| g.placed_at);
                    if let Ok(mut s) = st.lock() {
                        save_transfer_pos(&w, &mut s, placed);
                    }
                }
                let _ = w.hide();
            }
        });
    }
    // 标题栏的 ✕ 和窗口里的「收起」是同一件事：藏起来，传输继续。
    // （不接管的话 Slint 会当成「关窗」，窗口状态和这边的 tw_shown 就对不上了）
    {
        let weak = xfer_win.as_weak();
        let dismissed = Arc::clone(&xfer_dismissed);
        #[cfg(windows)]
        let st = Arc::clone(&state);
        #[cfg(windows)]
        let tw_state = Arc::clone(&tw_state);
        xfer_win.window().on_close_requested(move || {
            hfx_receiver::receiver::clear_failure();
            dismissed.store(true, Ordering::Relaxed);
            if let Some(w) = weak.upgrade() {
                #[cfg(windows)]
                {
                    let placed = tw_state.lock().ok().and_then(|g| g.placed_at);
                    if let Ok(mut s) = st.lock() {
                        save_transfer_pos(&w, &mut s, placed);
                    }
                }
                let _ = w.hide();
            }
            slint::CloseRequestResponse::KeepWindowShown
        });
    }
    // 「▾ 详细信息」：只翻一个 bool，窗口高度由主循环那边跟着算
    //（收起/展开都不动窗口位置：set_size 只改大小）
    {
        let weak = xfer_win.as_weak();
        xfer_win.on_toggle_details(move || {
            if let Some(w) = weak.upgrade() {
                // 传完之后明细没意义（窗口正准备收起来），别让它把窗口撑大
                if w.get_done() {
                    return;
                }
                w.set_details(!w.get_details());
            }
        });
    }
    // ---- 接收方向的「取消」（不只是暂停 / 继续）----
    // 主界面和独立传输窗口注册的是同一套函数（见上面的 cancel_recv_request / cancel_keep /
    // cancel_discard / cancel_dismiss），语义只有一份 —— 弹窗上的 ✕ 和主界面上的「取消」
    // 完全等价：先暂停（手机端跟着停发），再问「保留进度 / 不保留，删掉 / 继续接收」。
    ui.on_cancel_recv(cancel_recv_request);
    xfer_win.on_cancel_recv(cancel_recv_request);
    {
        let st = Arc::clone(&state);
        ui.on_cancel_keep(move || cancel_keep(&st));
    }
    {
        let st = Arc::clone(&state);
        xfer_win.on_cancel_keep(move || cancel_keep(&st));
    }
    {
        let st = Arc::clone(&state);
        ui.on_cancel_discard(move || cancel_discard(&st));
    }
    {
        let st = Arc::clone(&state);
        xfer_win.on_cancel_discard(move || cancel_discard(&st));
    }
    ui.on_cancel_dismiss(cancel_dismiss);
    xfer_win.on_cancel_dismiss(cancel_dismiss);
    // 独立窗口上的三个按钮：先收窗口、把传输窗口的状态位清零，再走同一套语义
    {
        let st = Arc::clone(&state);
        let tw_weak = xfer_win.as_weak();
        let aw = ask_win.as_weak();
        ask_win.on_keep(move || {
            if let Some(tw) = tw_weak.upgrade() {
                tw.set_cancel_ask(false);
            }
            if let Some(a) = aw.upgrade() {
                let _ = a.hide();
            }
            cancel_keep(&st);
        });
    }
    {
        let st = Arc::clone(&state);
        let tw_weak = xfer_win.as_weak();
        let aw = ask_win.as_weak();
        ask_win.on_discard(move || {
            if let Some(tw) = tw_weak.upgrade() {
                tw.set_cancel_ask(false);
            }
            if let Some(a) = aw.upgrade() {
                let _ = a.hide();
            }
            cancel_discard(&st);
        });
    }
    {
        // 确认框的标题栏 ✕ = 「继续接收」（不改主意），和框里那个按钮一个语义
        let tw_weak = xfer_win.as_weak();
        let aw = ask_win.as_weak();
        ask_win.window().on_close_requested(move || {
            if let Some(tw) = tw_weak.upgrade() {
                tw.set_cancel_ask(false);
            }
            if let Some(a) = aw.upgrade() {
                let _ = a.hide();
            }
            cancel_dismiss();
            slint::CloseRequestResponse::KeepWindowShown
        });
    }
    {
        let tw_weak = xfer_win.as_weak();
        let aw = ask_win.as_weak();
        ask_win.on_dismiss(move || {
            if let Some(tw) = tw_weak.upgrade() {
                tw.set_cancel_ask(false);
            }
            if let Some(a) = aw.upgrade() {
                let _ = a.hide();
            }
            cancel_dismiss();
        });
    }
    // ---- 「发送到手机」小窗上的按钮 ----
    //
    // 回调只改 PushRound（+ 立刻收窗口），窗口内容是主循环那个 150ms tick 刷的：
    // 点完到换屏最多 150ms，用户看不出来，但省掉一套「回调里也要会画界面」的重复代码。
    {
        let st = Arc::clone(&state);
        let round = Arc::clone(&push_round);
        let checked = Arc::clone(&push_checked);
        let weak = push_win.as_weak();
        push_win.on_choose(move |index| {
            let (targets, name, totals) = {
                let s = st.lock().unwrap();
                let serials = sorted_serials(&s);
                let Some(serial) = serials.get(index as usize).cloned() else {
                    log::warn(&format!(
                        "推送小窗：点了第 {} 台，但设备表里没有这一行",
                        index
                    ));
                    return;
                };
                let label = device_display_name(&s, &serial);
                let targets = vec![serial.clone()];
                // 只发这一台：数量就按「这台视角」量
                let totals = hfx_receiver::pending::totals_for(&serial);
                (targets, label, totals)
            };
            // 目标选定 = assign_to：队列交给这台手机（单选就是一份）
            hfx_receiver::pending::assign_to(&targets);
            log::info(&format!("推送目标已选定：{}", targets.join(", ")));
            hfx_receiver::pending::clear_hold();
            if let Ok(mut r) = round.lock() {
                r.begin_sending(targets.clone(), name, totals.0, totals.1);
            }
            if let Ok(mut c) = checked.lock() {
                c.clear();
            }
            hide_after_choose(&weak);
        });
    }
    {
        // 第 1 屏：点整行 / 点勾 = 切换勾选（多选）
        let st = Arc::clone(&state);
        let checked = Arc::clone(&push_checked);
        push_win.on_toggle(move |index| {
            let s = st.lock().unwrap();
            let serials = sorted_serials(&s);
            let Some(serial) = serials.get(index as usize).cloned() else {
                return;
            };
            drop(s);
            if let Ok(mut c) = checked.lock() {
                if let Some(pos) = c.iter().position(|x| x == &serial) {
                    c.remove(pos);
                    log::info(&format!("推送小窗：取消勾选 {}（剩 {}）", serial, c.len()));
                } else {
                    c.push(serial.clone());
                    log::info(&format!("推送小窗：勾选 {}（共 {}）", serial, c.len()));
                }
            }
        });
    }
    {
        // 第 1 屏：发送到勾选的那几台
        let st = Arc::clone(&state);
        let round = Arc::clone(&push_round);
        let checked = Arc::clone(&push_checked);
        let weak = push_win.as_weak();
        push_win.on_send_selected(move || {
            let targets: Vec<String> = checked
                .lock()
                .map(|c| c.clone())
                .unwrap_or_default();
            if targets.is_empty() {
                return;
            }
            let (name, files, bytes) = {
                let s = st.lock().unwrap();
                let name = push_target_label(&s, &targets);
                // 每台各来取一遍，所以总字节按「每台一份」算
                // 摘要写队列本来有多少（1 个文件 · 80.0 MB）：
                // 发给 2 台不等于 160MB，那是两台各传一遍同一份文件，不能拿字节数冒充。
                let (files, bytes) = hfx_receiver::pending::totals();
                (name, files, bytes)
            };
            hfx_receiver::pending::assign_to(&targets);
            log::info(&format!(
                "推送目标已选定：{}（{} 台）",
                targets.join(", "),
                targets.len()
            ));
            hfx_receiver::pending::clear_hold();
            if let Ok(mut r) = round.lock() {
                r.begin_sending(targets.clone(), name, files, bytes);
            }
            if let Ok(mut c) = checked.lock() {
                c.clear();
            }
            hide_after_choose(&weak);
        });
    }
    {
        // 第 1 屏「取消」/ 第 3 屏「关闭」/ 标题栏 ✕：收窗口，队列不动
        let weak = push_win.as_weak();
        let round = Arc::clone(&push_round);
        push_win.on_cancel(move || {
            log::info("推送小窗：关掉（队列保持不动，解除暂扣）");
            // 解除暂扣：文件还留在队列里（取消不解散队列），但没标目标的条目
            // 又可以「谁先来取就发给谁」了 —— 否则没人选就永远发不出去。
            hfx_receiver::pending::clear_hold();
            if let Ok(mut r) = round.lock() {
                r.reset();
            }
            if let Some(pw) = weak.upgrade() {
                let _ = pw.hide();
            }
        });
    }
    {
        // 第 2 屏「取消发送」：中断这次发送（传输线程每领一块都会看一眼这个开关）
        let round = Arc::clone(&push_round);
        push_win.on_cancel_send(move || {
            log::info("推送小窗：取消发送");
            hfx_receiver::control::cancel();
            if let Ok(mut r) = round.lock() {
                if r.step == PUSH_SENDING {
                    r.finish(true, "已取消发送".to_string());
                }
            }
        });
    }
    {
        // 设备列表为空时那颗「重试」：设备表本来就是每帧重数的，这里顺手把窗口
        // 重新贴一次右下角（换了屏 / 换了缩放时窗口可能不在右下角了）
        #[cfg(windows)]
        let st = Arc::clone(&push_state);
        push_win.on_retry(move || {
            log::info("推送小窗：重试（重新数一遍设备）");
            #[cfg(windows)]
            if let Ok(mut g) = st.lock() {
                g.begin();
                g.tick();
            }
        });
    }
    {
        // 标题栏的 ✕ 和「取消」是同一件事：收窗口，队列不动
        let weak = push_win.as_weak();
        let round = Arc::clone(&push_round);
        push_win.window().on_close_requested(move || {
            log::info("推送小窗：关闭（队列保持不动，解除暂扣）");
            hfx_receiver::pending::clear_hold();
            if let Ok(mut r) = round.lock() {
                r.reset();
            }
            if let Some(pw) = weak.upgrade() {
                let _ = pw.hide();
            }
            slint::CloseRequestResponse::KeepWindowShown
        });
    }
    {
        let st = Arc::clone(&state);
        ui.on_tray_toggled(move |enabled| {
            let mut s = st.lock().unwrap();
            s.config.minimize_to_tray = enabled;
            let path = s.config_path.clone();
            let config = s.config.clone();
            if let Err(e) = config.save(&path) {
                eprintln!("保存托盘设置失败: {e}");
            }
        });
    }
    {
        let st = Arc::clone(&state);
        ui.on_sendto_toggled(move |enabled| {
            let mut s = st.lock().unwrap();
            s.config.sendto = enabled;
            let path = s.config_path.clone();
            let config = s.config.clone();
            drop(s);
            if let Err(e) = config.save(&path) {
                eprintln!("保存「发送到」设置失败: {e}");
            }
            if enabled {
                std::thread::spawn(|| {
                    if let Err(e) = hfx_receiver::sendto::ensure() {
                        log::warn(&format!("创建「发送到手机」快捷方式失败: {e}"));
                    }
                });
            } else {
                hfx_receiver::sendto::remove();
            }
        });
    }
    {
        let st = Arc::clone(&state);
        let weak = ui.as_weak();
        ui.on_speed_limit_changed(move |text| {
            // 宽容解析：空串 / 半截输入 / 乱字符一律当 0（不限速），并且**把输入框改回来**。
            //
            // 为什么不报错拦住：限速是个随手调的设置，为了一个打错的字符把人卡在
            // 「设置不合法」前面不值得；回写成 0 至少语义明确（不限速），
            // 用户看到框里变成 0 就知道自己没填成。
            let mb = text.trim().parse::<u32>().unwrap_or(0).min(1000);
            {
                let mut s = st.lock().unwrap();
                if s.config.speed_limit_mb == mb {
                    return;
                }
                s.config.speed_limit_mb = mb;
                let path = s.config_path.clone();
                let config = s.config.clone();
                drop(s);
                if let Err(e) = config.save(&path) {
                    log::warn(&format!("保存限速设置失败: {e}"));
                }
            }
            // 本机立即生效。已经连着的手机不受影响 —— 协商值在握手时定下来，
            // 下一轮连接才按新值走（改运行中的速度会让当前传输忽快忽慢，反而更难用）。
            hfx_receiver::ratelimit::set_configured_mb(mb as i64);
            if let Some(ui) = weak.upgrade() {
                ui.set_speed_limit_text(mb.to_string().into());
            }
        });
    }
    {
        let st = Arc::clone(&state);
        ui.on_open_dir(move || {
            let s = st.lock().unwrap();
            open_in_explorer(&s.config.receive_dir.display().to_string());
        });
    }

    // ---- 推送小窗的流光：和传输窗口同一套公式（100px 带宽、200px/s、扫完停 1 秒）----
    // 只在第 2 屏（发送中）跑：窗口收起来 / 换屏了就不动（后台烧 CPU 没意义）。
    let push_flow_timer = slint::Timer::default();
    {
        const BAND_PX: f32 = 100.0;
        const SPEED_PX_S: f32 = 200.0;
        const PAUSE_S: f32 = 1.0;
        // 小窗 420 宽、左右各 16px 内边距 → 条子 388px
        const BAR_PX: f32 = 388.0;
        let phase = std::cell::Cell::new(0.0f32);
        let weak = push_win.as_weak();
        push_flow_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(16),
            move || {
                let Some(pw) = weak.upgrade() else { return };
                if !pw.window().is_visible() || pw.get_step() != 1 {
                    return;
                }
                // 不确定态：光带扫整条；有进度：只扫已经填绿的那一段
                let fill_px = if pw.get_indeterminate() {
                    BAR_PX
                } else {
                    (BAR_PX * pw.get_fill().clamp(0.0, 1.0)).max(1.0)
                };
                let travel_px = fill_px + 2.0 * BAND_PX;
                let sweep_s = travel_px / SPEED_PX_S;
                let cycle_s = sweep_s + PAUSE_S;
                let mut phase_now = phase.get() + 0.016;
                if phase_now >= cycle_s {
                    phase_now -= cycle_s;
                }
                phase.set(phase_now);
                let t = (phase_now / sweep_s).clamp(0.0, 1.0);
                let x_px = -BAND_PX + travel_px * t;
                pw.set_flow((x_px + BAND_PX / 2.0) / BAR_PX);
            },
        );
    }

    let launch_at_boot = std::env::args().any(|a| a == "--autostart");

    // ---------- 定时把线程里的状态/日志刷到界面 ----------
    let timer = slint::Timer::default();
    {
        let st = Arc::clone(&state);
        let ui_weak = ui.as_weak();
        // 独立传输窗口：这一轮有没有弹过 / 用户是不是已经把它收起来了
        let xfer_weak = xfer_win.as_weak();
        // 取消确认框那个独立窗口
        let ask_weak = ask_win.as_weak();
        let xfer_dismissed = Arc::clone(&xfer_dismissed);
        #[cfg(windows)]
        let tw_state = Arc::clone(&tw_state);
        let mut tw_shown = false;
        // 上一帧的「明细展开 / 取消框在显示」状态：只在变化的那一刻改窗口高度
        let mut tw_failed_on = false;
        // 明细行数（= 通道段数）。active 分支算出来的，给外面「每帧跟高度」那段用
        let mut last_row_count = 0usize;
        // 每截速率：拿「这一秒」和「上一秒」各截的已收字节做差。
        // PROGRESS 只记了每段占比和完成度，没有速率数据源 —— 但差值就是速率，
        // 一秒钟取一次样，既不抖也不用改协议层。
        let mut seg_rate_bytes: Vec<u64> = Vec::new();
        let mut seg_rate_prev: Vec<u64> = Vec::new();
        let mut seg_rates: Vec<f32> = Vec::new();
        let mut seg_rate_at: Option<std::time::Instant> = None;
        // 展开态图表条的速度曲线：横轴是进度，所以按**固定的进度桶**存。
        //
        // 以前是 VecDeque<(进度, 速率)> + 绘制时按索引等间距抽稀 —— 那个做法有两个要命的毛病：
        //   1) 桶满了要 pop_front → 曲线左端整段往前跳；
        //   2) 抽稀步长每帧都在变 → 同一个进度位置上一帧抽到的是这个点、下一帧是那个点，
        //      整条已经画过的曲线会跟着抖、扭（用户说的「画过的速度图示一直乱动扭曲」）。
        // 改成固定分桶之后，第 i 个桶永远就是进度 i/CURVE_BINS 处的速率：
        // 一旦写进去就不再变，曲线只会**向右长**，画过的部分一个像素都不会动。
        let mut curve_bins: Vec<f32> = vec![0.0; CURVE_BINS];
        // 已经画到哪儿了（进度 0..1）：曲线只铺到这里，未完成区间里没有面积。
        let mut curve_cur_progress = 0f32;
        let mut curve_last_at: Option<std::time::Instant> = None;
        // 上一次采样时的进度：用来认「新一轮开始了」——进度往回退就清空历史，别把上一轮的波带过来。
        let mut curve_last_progress = 0f32;
        // 图表条的纵轴量程（B/s）。新一轮复位；涨的时候立刻跟上，跌的时候极慢地收敛。
        //
        // 为什么不能「只涨不跌」：刚开传那一两秒，多通道一起把缓冲区灌满，会冒出一个
        // 远高于稳定值的水花（实测 9.5 MB/s 的传输，头一秒读到过 16 MB/s）。
        // 那个水花把量程顶死，之后整条曲线就被压在 40% 的高度上 —— 真机上和原生一比，
        // 我们的深绿只有 15px，原生是 35px。
        //
        // 所以峰值取**最近 5 秒**（滚动窗口），水花 5 秒后自己滑出窗口，量程再慢慢收敛。
        // 窗口从 30 秒缩到 5 秒：原生那条曲线是紧跟当前速率的（naive-00 峰顶就是满幅 35px），
        // 30 秒窗口会把「前一分钟的峰值」一直当量程，速度掉下来后曲线只剩一半高。
        let mut curve_max_speed = 0f32;
        // 最近 30 秒的速率采样（用于算滚动峰值）：(取样的时刻, 速率 B/s)
        // 最近若干次曲线速率（用于取 90 分位定量程）。只存值不存时刻：
        // 采样是等间隔的，按条数限长就等于按时间限长。
        let mut curve_peak_win: Vec<f32> = Vec::new();
        // 曲线自己一套速率：上一次采样时收到的字节数 + 时刻。
        //
        // **不能**拿上面那个 speed（0.2 秒窗口 + EMA 0.55/0.45）来画曲线：
        // 那个值是给「11.4 MB/s」那行大字和黑线用的，天生被压得几乎不动
        // （实测相邻两次采样 10889752 → 10348346，只差 5%，换算到 35px 振幅上只有 1.7px），
        // 画出来就是一条平线 —— 用户说的「一开始还是平的」就是这个。
        // 原生那条曲线的起伏是 ±50%（逐列实测 12..35px），那是**短间隔瞬时速率**
        // 才有的噪声。所以曲线单独按 40ms 采样间隔算一次瞬时值，两套互不干扰。
        // 滑窗：最近 2 秒的 (时刻, 累计字节)。曲线速率 = 窗口两端之差 / 时间差。
        //
        // 为什么不用「相邻两次采样之差」：PROGRESS 的字节数是按块累加的，
        // 一块收完才跳一次，于是 40ms 窗口里会读到「0、0、42761232、21MB」这种突发序列
        // （实测尖峰 66.8 MB/s，是正常值的 3 倍）—— 拿它算速率，量程立刻被顶飞，
        // 曲线反而被压成贴底；读 0 的那些格子还会在曲线上留下 0 高度的断口。
        // 滑 300ms 之后突发被摊平，又比 0.2 秒 EMA 保留更多真实起伏。
        let mut curve_win: Vec<(std::time::Instant, u64)> = Vec::new();
        // 每个分桶被写入了几次：写入用**加权平均**而不是覆盖。
        // 覆盖会让「同一个桶被连着采样 8 次」的最后一次压掉前 7 次，
        // 桶里的值就等于某一次的瞬时值 —— 相邻桶之间反而更容易跳成锯齿。
        let mut curve_bin_cnt: Vec<u16> = vec![0; CURVE_BINS];
        let mut tw_details_on = false;
        let mut tw_cancel_on = false;
        // 图表条 / 明细行的「上一帧快照」：内容没变就不碰 Slint 的 model 和 Path。
        // 传输窗口每帧整窗重绘（Slint 没有脏矩形），所以少改一个属性就是省一整帧。
        let mut rows_prev: Option<Vec<XferRow>> = None;
        let mut rows_model_keep = slint::ModelRc::new(slint::VecModel::from(Vec::<XferRow>::new()));
        let mut segs_prev: Option<Vec<XferSeg>> = None;
        let mut segs_model_keep = slint::ModelRc::new(slint::VecModel::from(Vec::<XferSeg>::new()));
        // 上一次真正重建 Path 时的进度 / 时刻：曲线按 200ms + 进度变化 0.2% 分频
        let mut curve_path_progress = -1f32;
        let mut curve_path_at: Option<std::time::Instant> = None;
        // 黑线的「显示位置」：每帧朝目标值追 22%。
        //
        // 速率本身是 0.2 秒量一次（再快就是噪声），直接把那个值喂给黑线的话，
        // 线就变成 5Hz 的跳格子 —— 用户原话「黑的速率线也卡」。
        // 这里让它在 60Hz 的拍子上做一次指数逼近：约 4~5 帧（80ms）走到位，
        // 既跟得上速率变化，又不会一格一格地跳。
        let mut guide_y_disp: Option<f32> = None;
        // tick 耗时统计（HFX_TICK_STAT=1 时每秒往日志写一行）：
        // 用来确认「卡」到底是 tick 太重，还是渲染跟不上。
        // env 查询放到循环外：std::env::var 每拍都做一次是白烧（虽然不贵，但没必要）
        let tick_stat_on = std::env::var("HFX_TICK_STAT").is_ok();
        let mut tick_stat_at = std::time::Instant::now();
        let mut tick_stat_sum = 0u128;
        let mut tick_stat_n = 0u32;
        let mut tick_stat_max = 0u128;
        // 这一轮传完的时刻（用来数「已完成」亮够 3 秒没有）
        let mut tw_finish_at: Option<std::time::Instant> = None;
        let mut last_bytes = 0u64;
        let mut last_tick = std::time::Instant::now();
        let mut speed = 0f64;

        let mut last_want_h = 0f32;
        let mut boot_hidden = false;

        // ---- 「发送到手机」小窗 ----
        let push_weak = push_win.as_weak();
        let push_round_tick = Arc::clone(&push_round);
    let push_checked_tick = Arc::clone(&push_checked);
        // 上一次看到的推送代次：变了就说明有一批新推送进来（0 = 这次起进程还没收到过）
        let mut last_push_gen = 0u64;
        #[cfg(windows)]
        let push_state_tick = Arc::clone(&push_state);

        // 16ms = 60Hz，和原生复制对话框一个节拍。
        //
        // 历史是 150ms（6.7fps）—— 用户直接说「进度一卡一卡的，黑的速率线也卡」。
        // 实测这个 tick 的 Rust 部分只要 0.5ms（HFX_TICK_STAT=1 可复现），
        // 所以限制帧率的从来不是计算，而是节拍本身：50ms 一拍就是 20fps，
        // 肉眼一样看得出来是「一跳一跳」。
        //
        // 能安全提到 60Hz 的前提是这一轮做的四件事（不然一帧光准备数据就超预算）：
        //   1) 明细行 / 分段条的 model 内容没变就沿用旧对象（否则 Slint 每帧重建整张列表）；
        //   2) 曲线 Path 降频到 150ms 且进度涨 0.2% 才重建（三角化 200 个点是整帧最贵的一步）；
        //   3) 所有属性都「变了才 set」—— 字符串尤其（中文重排版贵）；
        //   4) 展开态把流光定时器停掉（那时分段条整条隐藏，改了也看不见，却每帧重绘整窗）。
        timer.start(slint::TimerMode::Repeated, Duration::from_millis(16), move || {
            let tick_t0 = std::time::Instant::now();
            let Some(ui) = ui_weak.upgrade() else { return };
            // ---- 取消确认框：独立窗口，跟着 cancel-ask 这个状态位弹/收 ----
            // 传完 / 已经取消 / 用户选了任意一项 → 状态位被清掉 → 这里把窗口收起来。
            if let (Some(tw), Some(aw)) = (xfer_weak.upgrade(), ask_weak.upgrade()) {
                let want = tw.get_cancel_ask();
                let shown = aw.window().is_visible();
                if want && !shown {
                    let _ = aw.show();
                    // 弹在传输窗口正中（先 show 再量尺寸：没显示时尺寸还不可信）
                    let tw_pos = tw.window().position();
                    let tw_size = tw.window().size();
                    let aw_size = aw.window().size();
                    let x = tw_pos.x + (tw_size.width as i32 - aw_size.width as i32) / 2;
                    let y = tw_pos.y + (tw_size.height as i32 - aw_size.height as i32) / 2;
                    aw.window()
                        .set_position(slint::PhysicalPosition::new(x.max(0), y.max(0)));
                    // 传输窗口自己是 HWND_TOPMOST（§6.5），这个确认框也必须跟着置顶 + 抢前台，
                    // 否则用户看着它在前面、鼠标点下去命中的是后面那个置顶窗口 ——
                    // 真机 2026-11-19：点「保留进度」毫无反应，传输一路跑到 100%。
                    #[cfg(windows)]
                    if let Some(hwnd) = tray_win::find_window_titled(ASK_TITLE) {
                        tray_win::bring_to_front(hwnd);
                    }
                } else if !want && shown {
                    let _ = aw.hide();
                }
            }
            let mut s = st.lock().unwrap();

            // 「在不在接收」只看引擎：设备数不行 —— 停止过程中设备表里那条还在，
            // 界面就会自己跳回「已连接」，得按两次停止（用户报的那个 bug）
            let engine_running = s.engine.is_some();
            let mut changed = false;
            while let Ok((device, next)) = s.state_rx.try_recv() {
                changed = true;
                // 已经点了停止：引擎线程收尾时还会推几条消息过来，别再让界面跳回「已连接」
                if !engine_running || matches!(next, EngineState::Stopped) {
                    s.devices.remove(&device);
                } else {
                    s.devices.insert(device, next);
                }
            }
            // 后台解析好的设备名回来了：塞进缓存并让行重建（名字变了要重画）
            while let Ok((serial, name)) = s.name_rx.try_recv() {
                s.name_pending.remove(&serial);
                if s.device_names.get(&serial) != Some(&name) {
                    s.device_names.insert(serial, name);
                    changed = true;
                }
            }
            // 新出现的设备：派一个后台线程问它叫什么（adb shell getprop，几十到几百毫秒，
            // 不能在这条 UI 循环里等）。问过的不再问，名字缓存跟着进程活着。
            let unknown: Vec<String> = s
                .devices
                .keys()
                .filter(|serial| {
                    !s.device_names.contains_key(*serial) && !s.name_pending.contains(*serial)
                })
                .cloned()
                .collect();
            for serial in unknown {
                s.name_pending.insert(serial.clone());
                let name_tx = s.name_tx.clone();
                let exe_dir = s.exe_dir.clone();
                let adb_path = s.config.adb_path.clone();
                std::thread::spawn(move || {
                    let Some(adb) = hfx_receiver::adb::Adb::detect(&exe_dir, adb_path.as_deref()) else {
                        return;
                    };
                    if let Some(name) = adb.device_name(&serial) {
                        let _ = name_tx.send((serial, name));
                    }
                });
            }
            if changed || s.expanded_dirty {
                s.expanded_dirty = false;
                let mut connected: Vec<String> = Vec::new();
                let mut waiting: Option<String> = None;
                let mut retrying: Option<String> = None;
                for (device, state) in &s.devices {
                    match state {
                        EngineState::Connected(names) => connected.extend(names.iter().cloned()),
                        EngineState::Waiting(reason) => {
                            if waiting.is_none() {
                                let who = s
                                    .device_names
                                    .get(device)
                                    .cloned()
                                    .unwrap_or_else(|| device.clone());
                                waiting = Some(format!("{who}：{reason}"));
                            }
                        }
                        EngineState::Retrying(reason) => {
                            if retrying.is_none() {
                                let who = s
                                    .device_names
                                    .get(device)
                                    .cloned()
                                    .unwrap_or_else(|| device.clone());
                                retrying = Some(format!("{who}：{reason}"));
                            }
                        }
                        EngineState::Stopped => {}
                    }
                }
                let count = s.devices.len();
                // 只有真正连上的才算「已连接」：之前把「等待/重试」的设备也算进去，
                // 会出现电脑端显示「已连接 2 台」而手机上还在等连接的情况
                let connected_devices = s
                    .devices
                    .values()
                    .filter(|st| matches!(st, EngineState::Connected(_)))
                    .count();
                let waiting_devices = count.saturating_sub(connected_devices);
                let (kind, title, sub) = if !engine_running {
                    // 没在接收就是没在接收，别让残留的设备状态把界面顶回「已连接」
                    (
                        0,
                        i18n::tr("未开始接收", "not receiving").to_string(),
                        i18n::tr("点「开始接收」等待手机连接", "click Start to wait for a phone")
                            .to_string(),
                    )
                } else if !connected.is_empty() {
                    (
                        2,
                        if connected_devices > 1 {
                            format!("已连接 {connected_devices} 台设备")
                        } else {
                            // 这一侧是电脑，连上来的是手机/平板：「已连接电脑」读着像在说自己，
                            // 统一叫「设备」（和「已连接 N 台设备」保持一套词）
                            "已连接设备".to_string()
                        },
                        // 收起时只给概况，完整通道列表在展开后显示
                        if waiting_devices > 0 {
                            format!(
                                "{connected_devices} 台已连接 · {waiting_devices} 台等待 · {} 条通道",
                                connected.len()
                            )
                        } else {
                            format!("{connected_devices} 台设备 · {} 条通道", connected.len())
                        },
                    )
                } else if let Some(reason) = retrying {
                    (3, "连接有问题，正在重试".to_string(), reason)
                } else if let Some(reason) = waiting {
                    (1, "等待手机连接".to_string(), reason)
                } else {
                    (
                        0,
                        "未开始接收".to_string(),
                        "点「开始接收」等待手机连接".to_string(),
                    )
                };
                let full_channels = format!("通道：{}", connected.join(", "));
                s.state = if count > 0 {
                    EngineState::Connected(connected)
                } else {
                    EngineState::Stopped
                };
                ui.set_status_kind(kind);
                ui.set_status_title(title.into());
                ui.set_status_sub(sub.into());
                ui.set_status_sub_full(full_channels.clone().into());
                ui.set_running(engine_running);

                let mut serials: Vec<String> = s.devices.keys().cloned().collect();
                serials.sort();
                s.device_order = serials.clone();
                let mut rows: Vec<DeviceRow> = Vec::with_capacity(count);
                for serial in &s.device_order {
                    let state = &s.devices[serial];
                    let (label, detail) = match state {
                        EngineState::Connected(names) => (
                            "已连接",
                            format!("通道：{}", names.join(", ")),
                        ),
                        EngineState::Waiting(reason) => ("等待连接", reason.clone()),
                        EngineState::Retrying(reason) => ("重试中", reason.clone()),
                        EngineState::Stopped => ("已停止", String::new()),
                    };
                    let progress = hfx_receiver::receiver::progress_for(serial);
                    let (path, done, total, session_bytes, files) = progress.snapshot();
                    let detail = if detail.is_empty() {
                        format!("已接收 {} · {} 个文件", human(session_bytes), files)
                    } else {
                        format!("{}\n已接收 {} · {} 个文件", detail, human(session_bytes), files)
                    };
                    let _ = (path, done, total);
                    // 名字用手机自己报的（「Xiaomi Pad 7 Pro」）；还没解析出来就先顶序列号前 8 位，
                    // 一秒钟后自然会被换成真名
                    let shown_name = s
                        .device_names
                        .get(serial)
                        .cloned()
                        .unwrap_or_else(|| serial.chars().take(8).collect::<String>());
                    rows.push(DeviceRow {
                        name: shown_name.into(),
                        state: label.into(),
                        rate: human(session_bytes).into(),
                        detail: detail.into(),
                        expanded: s.expanded.contains(serial),
                    });
                }
                // 平板模型给"选择接收设备"卡片用；配对模型给一行两个的设备卡片用
                ui.set_devices(slint::ModelRc::new(slint::VecModel::from(rows.clone())));

                // 两两配对，界面一行画两个
                let mut pairs: Vec<DevicePair> = Vec::with_capacity((rows.len() + 1) / 2);
                let mut iter = rows.into_iter();
                while let Some(left) = iter.next() {
                    let right = iter.next();
                    pairs.push(DevicePair {
                        has_right: right.is_some(),
                        right: right.unwrap_or(DeviceRow {
                            name: "".into(),
                            state: "".into(),
                            rate: "".into(),
                            detail: "".into(),
                            expanded: false,
                        }),
                        left,
                    });
                }
                ui.set_device_rows(slint::ModelRc::new(slint::VecModel::from(pairs)));
            }

            // ================= 「发送到手机」小窗（PushWindow） =================
            //
            // 以前这里是「ui.set_ask_target(true) + ui.show() + show_main_window(...)」：
            // 右键推送会把整套接收器界面弹出来（用户抱怨「推送不要跳转到接收器界面」）。
            // 现在推送路径只弹那个 420 宽的小窗，主界面一次都不露 ——
            // 下面这段里**不许**出现 ui.show() / show_main_window。
            // 主界面那张「选择接收设备」卡片（app.slint 的 ask-target）保留给
            // 「从主界面里主动发」，只是不再自动触发了。

            // 代次一变 = 有一批新推送进来。
            // 两条入口（启动时命令行里的文件 / 托盘 WM_COPYDATA）最后都走
            // pending::push_with_target，那里把代次 +1；界面这边只比数字，不去解析那条消息。
            let gen_now = hfx_receiver::pending::generation();
            if gen_now != last_push_gen {
                last_push_gen = gen_now;
                let serials = sorted_serials(&s);
                let (files, bytes) = hfx_receiver::pending::totals();
                // 已经点名了目标（`--to <串号>`）就不用再问；否则只有一台时才自动选
                let sole = hfx_receiver::pending::sole_target();
                let auto: Option<String> = match sole {
                    Some(target) => Some(target),
                    None if serials.len() == 1 => Some(serials[0].clone()),
                    None => None,
                };
                let mut need_window = false;
                if let Ok(mut r) = push_round_tick.lock() {
                    r.reset();
                    r.gen = gen_now;
                    if let Some(serial) = auto {
                        // 点名 / 只有一台：没什么可选，直接发，连小窗都不弹。
                        // 进度和完成交给已有的「正在传输」小窗 —— 推送路径不许再弹
                        // 接收器的任何界面，多弹一个只显示进度的窗只会跟它叠在一起。
                        // 点名的设备不在列表里（还没连上）也照样发：连不上会在超时后
                        // 给小窗一个失败说明，比「假装没收到」强。
                        hfx_receiver::pending::assign_all(&serial);
                        let name = device_display_name(&s, &serial);
                        let totals = hfx_receiver::pending::totals_for(&serial);
                        r.begin_sending(vec![serial], name, totals.0, totals.1);
                    } else {
                        // 多台（或者一台都没连上）：必须让用户选 / 显示「还没有手机连接」。
                        // 这批文件数和字节数在这里就记下来：之后队列会被取走，
                        // 每帧再去量就会量到 0（真机上就显示过「0 B」）。
                        r.step = PUSH_CHOOSE;
                        r.files = files;
                        r.bytes = bytes;
                        need_window = true;
                    }
                }
                log::info(&format!(
                    "推送小窗：{} 个文件 · {} · 设备 {} 台",
                    files,
                    human(bytes),
                    serials.len()
                ));
                if let Ok(mut c) = push_checked_tick.lock() {
                    // 新的一批：上次勾的作废（别把上一批的选择带进来）
                    c.clear();
                }
                if need_window {
                    if let Some(pw) = push_weak.upgrade() {
                        #[cfg(windows)]
                        {
                            if let Ok(mut g) = push_state_tick.lock() {
                                // 新的一批：重新抢一次前台（不然用户压根不知道有东西推过来了）
                                g.focused = false;
                                show_push_window(&pw, &mut g);
                            }
                        }
                        #[cfg(not(windows))]
                        let _ = pw.show();
                    }
                }
            }

            // 每帧推进状态机 + 往窗口上刷内容（回调那边只改 PushRound）
            let mut push_step_now = PUSH_IDLE;
            if let Some(pw) = push_weak.upgrade() {
                let mut step_changed = false;
                if let Ok(mut r) = push_round_tick.lock() {
                    push_step_now = r.step;
                    match r.step {
                        PUSH_CHOOSE => {
                            // 设备列表每帧重建（就一两行）：名字解析回来了、连上 / 断开了都跟得上
                            let checked_now = push_checked_tick
                                .lock()
                                .map(|c| c.clone())
                                .unwrap_or_default();
                            let rows = push_device_rows(&s, &checked_now);
                            if pw.get_step() != 0 {
                                pw.set_step(0);
                                step_changed = true;
                            }
                            // 用进屏那一刻记下的数，不再每帧重量（量的时候可能已经被取走了）
                            pw.set_summary_text(push_summary(r.files, r.bytes).into());
                            let n = checked_now.len() as i32;
                            pw.set_chosen_count(n);
                            pw.set_send_label(
                                if n == 0 {
                                    "选择要发送的手机".to_string()
                                } else {
                                    format!("发送到选中的 {} 台", n)
                                }
                                .into(),
                            );
                            pw.set_devices(slint::ModelRc::new(slint::VecModel::from(rows)));
                        }
                        PUSH_SENDING => {
                            let serial = r.serial.clone().unwrap_or_default();
                            // 发送方向（电脑 → 手机）的进度记在**全局** PROGRESS 上：
                            // session.rs 的 mark_sent 写的是 crate::receiver::PROGRESS，
                            // 不是 progress_for(串号) 那一份（那一份是「手机发给我」用的）。
                            // 主窗口第 1477 行读的也是这个，两边看的是同一份数。
                            let prog = &hfx_receiver::receiver::PROGRESS;
                            let (file_path, file_done, file_total, session_all, files_started) =
                                prog.snapshot();
                            let sent = prog.received_bytes();
                            let sending_now = hfx_receiver::control::sending();
                            if sending_now {
                                r.saw_sending = true;
                            }
                            // 速率：0.4 秒取一次样（和主界面同一套平滑）；
                            // 手机那边一 reset 进度（received 变小）就重新起算，
                            // 别把上一轮的字节当成本轮的速率基准。
                            let now = std::time::Instant::now();
                            if sent < r.last_bytes {
                                r.last_bytes = sent;
                                r.speed = 0.0;
                                r.last_at = now;
                            }
                            let elapsed = now.duration_since(r.last_at).as_secs_f64();
                            if elapsed >= 0.4 {
                                let delta = sent.saturating_sub(r.last_bytes);
                                let instant = delta as f64 / elapsed;
                                if !hfx_receiver::control::paused() {
                                    r.speed = if r.speed <= 0.0 {
                                        instant
                                    } else {
                                        r.speed * 0.6 + instant * 0.4
                                    };
                                }
                                r.last_bytes = sent;
                                r.last_at = now;
                            }
                            // 进度：优先按「这一批的总字节」算（续传基线也算进去，不然永远到不了 100%），
                            // 量不出总字节时退回「当前文件」的比例；两个都没有 = 不确定态（流光条）
                            let (fill, indeterminate) = if r.bytes > 0 {
                                ((session_all.min(r.bytes)) as f32 / r.bytes as f32, false)
                            } else if file_total > 0 {
                                ((file_done.min(file_total)) as f32 / file_total as f32, false)
                            } else {
                                (0.0, true)
                            };
                            // 当前文件名（多文件时带上「第 x / n 个文件」）
                            let short = file_path
                                .rsplit(['/', '\\'])
                                .next()
                                .unwrap_or(&file_path)
                                .to_string();
                            let file_line = if !short.is_empty() {
                                if r.files > 1 {
                                    format!(
                                        "{}（第 {} / {} 个文件）",
                                        short,
                                        files_started.max(1),
                                        r.files
                                    )
                                } else {
                                    short
                                }
                            } else if r.saw_sending {
                                "正在收尾…".to_string()
                            } else {
                                "等待手机取走文件…".to_string()
                            };
                            let pct = (fill * 100.0).floor() as i32;
                            let progress_text = if file_total > 0 {
                                format!(
                                    "{} / {} · {}",
                                    human(file_done),
                                    human(file_total),
                                    human_speed(r.speed)
                                )
                            } else {
                                format!(
                                    "{} / {} · {}",
                                    human(session_all),
                                    human(r.bytes),
                                    human_speed(r.speed)
                                )
                            };
                            if pw.get_step() != 1 {
                                pw.set_step(1);
                                step_changed = true;
                            }
                            pw.set_target_name(r.target_name.clone().into());
                            pw.set_summary_text(push_summary(r.files, r.bytes).into());
                            pw.set_file_name(file_line.into());
                            pw.set_fill(fill);
                            pw.set_indeterminate(indeterminate);
                            pw.set_percent_text(
                                if indeterminate {
                                    "".into()
                                } else {
                                    format!("{}%", pct).into()
                                },
                            );
                            pw.set_progress_text(progress_text.into());

                            // ---- 结束判定 ----
                            // 手机把这一批取完并传完：sending 由 true 落回 false。
                            //
                            // 光看「sending 掉没掉」不够：小文件一秒就传完，150ms 的 tick 可能一次都
                            // 没看到 sending=true，这一轮就永远等在那儿（只能等超时）。所以补一条
                            // 「队列被取空 + 字节已经到位」—— 必须两个条件同时成立，
                            // 否则刚 take_for、send 还没起来的瞬间会被误判成「传完了」。
                            // 多选时「取空」得看整个队列（每台各取自己那份，串号视角量不全）
                            let queue_drained = r.queued_left() == 0;
                            // 多台并行时**不能**用字节判完成：发送方向的进度记在全局 PROGRESS 上，
                            // 两条会话一起 reset / 一起累加，谁也对不上谁（真机上 2 台各 80MB
                            // 传完之后被算成「140 / 160」）。多台只认「队列被取空 + 没有失败」。
                            let multi = r.serials.len() > 1;
                            let bytes_done = if multi {
                                true
                            } else {
                                r.bytes == 0
                                    || session_all >= r.bytes
                                    || (file_total > 0 && file_done >= file_total)
                            };
                            let finished_now = if multi {
                                queue_drained && !sending_now
                            } else if r.saw_sending {
                                !sending_now
                            } else {
                                queue_drained && bytes_done
                            };
                            if finished_now && !r.failed {
                                let ok = bytes_done;
                                if ok {
                                    log::info(&format!(
                                        "推送小窗：已发送 {} · {} 个文件",
                                        human(r.bytes),
                                        r.files
                                    ));
                                    r.finish(false, String::new());
                                } else {
                                    log::warn(&format!(
                                        "推送小窗：只发出一部分 {} / {}",
                                        human(session_all),
                                        human(r.bytes)
                                    ));
                                    let done_text =
                                        format!("只发出 {} / {}", human(session_all), human(r.bytes));
                                    r.finish(true, done_text);
                                }
                                step_changed = true;
                            }
                            // ---- 一直没开始的超时说法 ----
                            if !r.saw_sending && !r.failed {
                                if let Some(t0) = r.started_at {
                                    // 多选时只要有一台连上了就算「在等它来取」；
                                    // 一台都连不上才走 8 秒那条（再等也没用）
                                    let connected = r
                                        .serials
                                        .iter()
                                        .any(|x| matches!(s.devices.get(x), Some(EngineState::Connected(_))))
                                        || s.devices
                                            .get(&serial)
                                            .map(|st| matches!(st, EngineState::Connected(_)))
                                            .unwrap_or(false);
                                    let waited = t0.elapsed();
                                    if !connected && waited >= Duration::from_secs(8) {
                                        // 那台旧协议的手机：一直连不上，再等也没用
                                        log::warn(&format!(
                                            "推送小窗：{} 连不上，放弃这一轮",
                                            r.target_name
                                        ));
                                        let fail_text = format!(
                                            "连不上 {}（还没开始发送）",
                                            r.target_name
                                        );
                                        r.finish(true, fail_text);
                                        // 一个字节都没出去：把目标退回去，文件照样留在队列里，
                                        // 插上另一台手机 / 下次推送还能走
                                        hfx_receiver::pending::release_targets();
                                        step_changed = true;
                                    } else if connected && waited >= Duration::from_secs(20) {
                                        // 连上了但手机端一直不来取（比如没打开接收页）
                                        log::warn("推送小窗：手机连上了但一直没来取文件");
                                        let fail_text =
                                            format!("{} 一直没有来取文件", r.target_name);
                                        r.finish(true, fail_text);
                                        hfx_receiver::pending::release_targets();
                                        step_changed = true;
                                    }
                                }
                            }
                        }
                        PUSH_DONE => {
                            // 成功不打扰：小窗整个收着（选完就关掉了），完成由传输小窗说。
                            // 失败才把小窗重新弹出来 —— 「哪台没收到 / 为什么」必须让人看见。
                            if !r.failed {
                                if pw.window().is_visible() {
                                    let _ = pw.hide();
                                }
                                if let Some(t) = r.done_at {
                                    if t.elapsed() >= Duration::from_secs(2) {
                                        log::info("推送小窗：这一轮收尾，退出小窗状态");
                                        r.reset();
                                    }
                                }
                            }
                            if r.failed && !pw.window().is_visible() {
                                #[cfg(windows)]
                                {
                                    if let Ok(mut g) = push_state_tick.lock() {
                                        g.focused = false;
                                        g.begin();
                                    }
                                }
                                let _ = pw.show();
                                #[cfg(windows)]
                                if let Ok(mut g) = push_state_tick.lock() {
                                    g.tick();
                                }
                            }
                            if pw.get_step() != 2 {
                                pw.set_step(2);
                                step_changed = true;
                            }
                            pw.set_target_name(r.target_name.clone().into());
                            pw.set_summary_text(push_summary(r.files, r.bytes).into());
                            pw.set_failed(r.failed);
                            pw.set_fail_text(r.fail_text.clone().into());
                        }
                        _ => {
                            // 这一轮已经收掉了（取消 / 自动关窗）：窗口不该还挂在屏幕上
                            if pw.window().is_visible() {
                                let _ = pw.hide();
                            }
                        }
                    }
                }
                if step_changed {
                    // 换屏 = 高度变了：重新补几帧落位 + 置顶（窗口长好要好几十毫秒）
                    #[cfg(windows)]
                    if let Ok(mut g) = push_state_tick.lock() {
                        g.begin();
                        g.tick();
                    }
                }
            }

            // 暂扣不在这里开关：push_with_target 入队那一刻就扣上了（见 pending.rs），
            // 用户取消 / 关窗时才 clear_hold。这里只留一句提醒，说明为什么不在这一帧判断 ——
            // 之前就是「这一帧才知道要不要扣」，被手机抢了 150ms 的先手。
            let _ = push_step_now;

            // 落位 / 置顶还要补几帧（窗口刚 show / 刚换屏时位置不一定已经生效）
            #[cfg(windows)]
            if let Ok(mut g) = push_state_tick.lock() {
                g.tick();
            }

            // 开机自启：等窗口真正创建后隐藏到托盘（只做一次）
            #[cfg(windows)]
            if launch_at_boot && !boot_hidden && ui.get_tray() {
                boot_hidden = true;
                tray_win::hide_main_window(WINDOW_TITLE);
            }

            // ---- 传输进度 ----
            let (path, done, total, session_bytes, files) =
                hfx_receiver::receiver::PROGRESS.snapshot();
            // 下面 segs 那一段会把 total 这个名字借去当 f32 用（段宽合计），
            // 独立窗口那边要的还是字节总数，先留一份
            let total_bytes = total;

            // 这一轮是不是失败结束的（块账本对不上 / 手机端说指纹不一致）。
            // 失败不能亮一下「已完成」再自己收起来 —— 用户会以为传完了
            // （TRANSFER-WINDOW.md §2.1：失败/取消停在窗口上，等用户看见）。
            let failure_now = hfx_receiver::receiver::last_failure();
            let failed = failure_now.is_some();

            // 速率只按「这次真收到的字节」算：续传基线（磁盘上本来就有的一块）不算进去，
            // 否则续传一开始会跳出一个几 GB/s 的假峰值。
            let received = hfx_receiver::receiver::PROGRESS.received_bytes();
            let now = std::time::Instant::now();
            let elapsed = now.duration_since(last_tick).as_secs_f64();
            // 0.2 秒一个窗口 = 每秒更新 5 次。以前是 0.4 秒（2.5 次/秒），
            // 黑线和「速度:」那行字看着就像一秒跳两下的电子钟。
            if elapsed >= 0.2 {
                let delta = received.saturating_sub(last_bytes);
                let instant = delta as f64 / elapsed;
                // 暂停时**冻结**速率，不跟着衰减：暂停期间一个字节都不会来，
                // 照着算 delta=0 会让速率每 0.4 秒掉 40%，几秒后就从 9.6 MB/s 掉成
                // 一串无意义的数字（真机截图里「暂停」旁边一路跳到 836 KB/s、再往下跳）。
                // 原生暂停是把图表整块冻住，数字停在最后那个值上；这里照办。
                if !hfx_receiver::control::paused() {
                    // 轻微平滑，避免数字乱跳
                    // 0.55 的惯性 + 0.45 的新值：窗口短了噪声会变大，靠这个压住。
                    // 原生那条黑线也不是纯瞬时值，它是一条跟着速度走的平滑线。
                    speed = if speed <= 0.0 { instant } else { speed * 0.55 + instant * 0.45 };
                }
                last_bytes = received;
                last_tick = now;
            }

            // 没有设备真正连着时（比如手机端服务被强停）不再显示残留的传输卡片
            let any_connected = s
                .devices
                .values()
                .any(|st| matches!(st, EngineState::Connected(_)));
            let active = s.running() && total > 0 && any_connected;
            ui.set_xfer_visible(active);
            // 发送中 / 接收中都显示「暂停 / 继续」，发送时额外给「中断发送」
            ui.set_send_active(hfx_receiver::control::sending());
            ui.set_recv_active(hfx_receiver::control::receiving());
            // 这一轮接收的「取消」是否已经受理（收尾 / 重建中）：是就把取消按钮收起来，
            // 取消是幂等的 —— 同一轮接收只允许取消一次。
            ui.set_recv_cancelling(
                hfx_receiver::control::receiving() && hfx_receiver::control::cancel_requested(),
            );
            ui.set_send_paused(hfx_receiver::control::paused());

            // ---- 取消接收：确认框与结果提示 ----
            // 什么时候算「收完尾」：进度卡没了（handle_receive 在 finish_with 之后才 reset 进度），
            // 或者 RECEIVING 已经落下去。两个条件谁先满足都行 —— 「已删除」不能比实际删文件
            // 早一句话，而取消之后的重建握手（最长几十秒）不该把这句话一起拖着。
            let receiving_now = hfx_receiver::control::receiving();
            let idle_now = !receiving_now || !active;
            if let Some(note) = s.cancel_note.clone() {
                if idle_now {
                    s.cancel_note = None;
                    ui.set_notice_text(note.into());
                }
            }
            // 传输自己结束了 / 引擎被停了：确认框没意义了，收掉。
            // （弹框时已经不暂停了，这儿只处理「框还开着，传输先一步跑完」的情况）
            // 已经在取消中（或者接收已经结束）就把确认框收掉：用户第二次点「取消」
            // 时不该再看到它，更不该让那里再选一次「保留 / 不保留」。
            // 确认框现在是独立窗口、状态位挂在传输窗口上：这里只在「传输已经不在收」时
            // 把那个位清掉（定时器会把窗口收起来）。
            if let Some(tw) = xfer_weak.upgrade() {
                if tw.get_cancel_ask() && !receiving_now {
                    tw.set_cancel_ask(false);
                    if !hfx_receiver::control::cancel_requested() {
                        hfx_receiver::control::pause(false);
                    }
                }
            }
            // 新的一次传输开始了：上一次的取消提示让位（进度卡回来了，提示就不该再挂着）
            if !ui.get_notice_text().is_empty() && active {
                ui.set_notice_text("".into());
            }

            if active {
                let short = path
                    .rsplit(['/', '\\'])
                    .next()
                    .unwrap_or(&path)
                    .to_string();
                ui.set_file_name(short.clone().into());
                ui.set_file_progress(if total > 0 { done as f32 / total as f32 } else { 0.0 });
                ui.set_file_text(format!("{} / {}", human(done), human(total)).into());
                // 手机来取文件时是"已发送"，手机推过来时才是"已接收"
                let verb = if ui.get_send_active() { "已发送" } else { "已接收" };
                // 暂停时速率位置显示「已暂停」（和手机端弹窗一致）
                let speed_text = if ui.get_send_paused() {
                    "已暂停".to_string()
                } else {
                    human_speed(speed)
                };
                ui.set_session_text(format!(
                    "{} {} · {} · {} 个文件",
                    verb,
                    human(session_bytes),
                    speed_text,
                    files
                ).into());

                // IDM 式分块：和手机端一样按"哪条通道负责哪一段"画。
                // 两个方向都统一用块位图（mark_sent / mark_received 都按块记下真实通道号），
                // 段边界 = 相邻块通道归属的变化处：一个块传完就重算一次分段，
                // 通道抢走别人的段后，旧的边界会一直留在位图里（切分痕迹）。
                // 之前发送方向用 SendPlan 的任务区间：边界只在"领任务"那一刻重算，
                // 和实际发完的块对不上，看起来就是整段一起跳、没有切分痕迹。
                let mut raw: Vec<hfx_receiver::receiver::XferSeg> =
                    hfx_receiver::receiver::PROGRESS.segments();
                let total: f32 = raw.iter().map(|s| s.span).sum::<f32>().max(1.0);
                for s in raw.iter_mut() {
                    s.span = (s.span / total).clamp(0.0, 1.0);
                    s.filled = s.filled.clamp(0.0, 1.0);
                }
                // 已填充比例（按块算）：进度条画到哪、流光在哪儿扫都看它
                let fill_frac: f32 = raw.iter().map(|s| s.span * s.filled).sum::<f32>().clamp(0.0, 1.0);

                // ---- 「详细信息」里的明细行：一截一行 ----
                // 每截：占整条多少 · 这段已收 / 这段总量 · 这段自己的速率 · 完成百分比。
                // 速率是「这一秒和上一秒的已收字节做差」算出来的（见上面的 seg_rate_*）：
                // PROGRESS 没有速率数据源，但差值就是速率，不用改协议层就能有。
                let mut rows: Vec<XferRow> = Vec::with_capacity(raw.len());
                // 行名先不带序号、行的排序键先攒着：手机端那张表是**按通道号**排的
                // （1 USB_ADB、2 USB_ADB#2、3 wlan1 …），电脑这边段是按文件位置来的，
                // 直接照段序排行两边就对不上。排完再统一编号。
                let mut labels: Vec<String> = Vec::with_capacity(raw.len());
                let mut row_order: Vec<(u8, f32)> = Vec::with_capacity(raw.len());
                seg_rate_bytes.clear();
                // 暂停中：速率列统一写 0 KB/s（这一秒本来就没在传，挂个旧速率是骗人）
                let paused_now = hfx_receiver::control::paused();
                for (i, s) in raw.iter().enumerate() {
                    let seg_total = (total_bytes as f64 * s.span as f64) as u64;
                    let seg_done = (seg_total as f64 * s.filled as f64) as u64;
                    seg_rate_bytes.push(seg_done);
                    let rate = seg_rates.get(i).copied().unwrap_or(0.0);
                    // 「各通道速率」这一列的说法（这一轮定的，照原生的意思来）：
                    //   这一截收满 → 「已完成」；暂停中 / 一秒没动静 → 「0 KB/s」；
                    //   其余按真实速率（human_speed 自带 KB/s、MB/s 的量级切换）。
                    let rate_text = if s.filled >= 0.999 {
                        i18n::tr("已完成", "done").to_string()
                    } else if paused_now || rate <= 0.05 {
                        "0 KB/s".to_string()
                    } else {
                        human_speed(rate as f64)
                    };
                    // 段名 = 「谁在负责」，和手机端那条明细（「1 USB_ADB」）同一个读法（序号后加）。
                    // owner 是通道号（mark_sent / mark_received 记下的那个），从这里换成人能看的名字；
                    // 还没分配的尾段、续传捡回来的块各有各的叫法。
                    let label = if s.owner == u8::MAX {
                        "未分配".to_string()
                    } else if s.owner == hfx_receiver::receiver::OWNER_RESUMED {
                        "续传".to_string()
                    } else {
                        match hfx_receiver::receiver::PROGRESS.channel_name(s.owner) {
                            Some(n) => n,
                            None => {
                                if std::env::var("HFX_DEBUG_NAME").is_ok() {
                                    log::info(&format!(
                                        "通道名缺失 owner={} 已登记={}",
                                        s.owner,
                                        hfx_receiver::receiver::PROGRESS.channel_count()
                                    ));
                                }
                                format!("通道 {}", s.owner as u32 + 1)
                            }
                        }
                    };
                    labels.push(label);
                    row_order.push((s.owner, i as f32));
                    rows.push(XferRow {
                        name: "".into(),
                        span_text: format!("{:.0}%", s.span * 100.0).into(),
                        amount: format!("{} / {}", human(seg_done), human(seg_total)).into(),
                        rate: rate_text.into(),
                        percent: (s.filled * 100.0).round() as i32,
                    });
                }
                // 按通道号重排（和手机端那张表同序：1 USB_ADB、2 USB_ADB#2 …），再统一编号；
                // 「未分配 / 多通道合并」的段排最后（它们不对应某一条通道）。
                {
                    let mut order: Vec<usize> = (0..rows.len()).collect();
                    order.sort_by(|&a, &b| {
                        let (oa, xa) = row_order[a];
                        let (ob, xb) = row_order[b];
                        let ka = if oa == u8::MAX { (1u8, u8::MAX) } else { (0u8, oa) };
                        let kb = if ob == u8::MAX { (1u8, u8::MAX) } else { (0u8, ob) };
                        ka.cmp(&kb).then(xa.partial_cmp(&xb).unwrap_or(std::cmp::Ordering::Equal))
                    });
                    let mut sorted: Vec<XferRow> = Vec::with_capacity(rows.len());
                    for (new_i, &old_i) in order.iter().enumerate() {
                        let mut r = rows[old_i].clone();
                        r.name = format!("{}. {}", new_i + 1, labels[old_i]).into();
                        sorted.push(r);
                    }
                    rows = sorted;
                }
                let row_count = rows.len();
                // 每截速率的取样：约一秒一次。帧是 150ms 一帧，逐帧做差会把
                // 块到达的抖动放大成满屏跳动的数字；一秒取一次刚好稳。
                {
                    let now_at = std::time::Instant::now();
                    let due = match seg_rate_at {
                        Some(t) => now_at.duration_since(t).as_secs_f32() >= 0.9,
                        None => true,
                    };
                    if due {
                        if let Some(t) = seg_rate_at {
                            let dt = now_at.duration_since(t).as_secs_f32().max(0.2);
                            seg_rates = seg_rate_bytes
                                .iter()
                                .enumerate()
                                .map(|(i, &b)| {
                                    let prev = seg_rate_prev.get(i).copied().unwrap_or(b);
                                    if b >= prev {
                                        (b - prev) as f32 / dt
                                    } else {
                                        0.0
                                    }
                                })
                                .collect();
                        }
                        seg_rate_prev = seg_rate_bytes.clone();
                        seg_rate_at = Some(now_at);
                    }
                }
                // 进度条上的切分线：段是「相邻块换了通道」留下的痕迹，位置就是它占整条的比例。
                // 主条按段画（每段从自己的左边缘往右填），这里只算每段的起点和宽度。
                let mut segs: Vec<XferSeg> = Vec::with_capacity(raw.len());
                let mut seg_at = 0.0f32;
                for s in raw.iter() {
                    segs.push(XferSeg {
                        x: seg_at,
                        span: s.span,
                        filled: s.filled,
                    });
                    seg_at += s.span;
                }
                // 诊断：设了 HFX_DEBUG_SEGS=1 就把这一帧的段表写进日志（排查「条子画得不像」用）。
                if std::env::var("HFX_DEBUG_SEGS").is_ok() {
                    let mut acc = 0.0f32;
                    let mut line = format!("段表 done={} n={}", done, raw.len());
                    for s in raw.iter() {
                        line.push_str(&format!(
                            " [{:.3}+{:.3} f{:.2} o{}]",
                            acc, s.span, s.filled, s.owner
                        ));
                        acc += s.span;
                    }
                    log::debug(&line);
                }
                // ---- 关键：model 不能每拍都换 ----
                // 每 50ms 新建一个 VecModel 塞给 Slint，等于告诉它「整个列表全变了」：
                // 它会把这 10 行 Rows 全部拆掉重建（每行里还有 10 来个 Text/Rectangle），
                // 光这一步就吃掉整帧预算的大头 —— 真机上 20fps 都跑不满，看着就是卡。
                // 所以先和自己的上一帧比，内容一样就沿用旧 model，Slint 那边什么都不会动。
                let segs_model = if segs_prev.as_ref().map(|p| p.as_slice()) == Some(segs.as_slice()) {
                    segs_model_keep.clone()
                } else {
                    segs_prev = Some(segs.clone());
                    let m = slint::ModelRc::new(slint::VecModel::from(segs));
                    segs_model_keep = m.clone();
                    m
                };
                // 外面那段「每帧跟高度」要用：它跑的时候 active 可能是 false，拿不到 rows
                last_row_count = row_count;
                // 同 segs：明细行内容不变就不换 model（换一次 = Slint 重建全部行进）
                let rows_model = if rows_prev.as_ref().map(|p| p.as_slice()) == Some(rows.as_slice()) {
                    rows_model_keep.clone()
                } else {
                    rows_prev = Some(rows.clone());
                    let m = slint::ModelRc::new(slint::VecModel::from(rows));
                    rows_model_keep = m.clone();
                    m
                };

                // ---- 独立传输窗口：同一份数据再喂一遍 ----
                // 传完了没有：真机第一次跑出来的是「已完成 100%」+ 0 B/s 一直挂着不消失 ——
                // 因为 active 只看「有设备连着 + 有进度」，传完之后 PROGRESS 不清零，
                // 这两个条件依然成立。所以「结束」得单独认：字节满了就算完，
                // 亮 3 秒「已完成」再自己收起来。
                // 真机第二次跑出来：传完了 done 停在 599.0/600.0 MB（比 total 少 1MB，
                // 块计数的尾巴对不上），只按 done>=total 判永远判不到「完成」，
                // 窗口就挂着「已完成 99%」不消失。所以再认一个信号：
                // control 那边 receiving()/sending() 都落下去，就是这一轮结束了。
                let in_flight = hfx_receiver::control::receiving() || hfx_receiver::control::sending();
                let finished = (total_bytes > 0 && done >= total_bytes) || !in_flight;
                if let Some(tw) = xfer_weak.upgrade() {
                    let size_text = human(total_bytes);
                    // 界面精简：这一行只留「文件名（大小）」。
                    // 标题栏已经写了「正在传输 / 已完成 / 接收失败」，再写一遍
                    // 「正在将 … 从手机复制到 …」是把同一件事说两遍，还把条子挤下去一行。
                    // 失败原因仍然单独一行红字（那是新信息，不能省）。
                    // 字符串属性一律「变了才 set」：Slint 每次收到新的字符串都要重排版，
                    // 中文排版尤其贵。60Hz 下每帧无脑 set 一遍，CPU 就是被这里吃掉的
                    // （实测 78% 单核 → 加这层判断后降到个位数）。
                    let action_now = if failed {
                        i18n::tr("接收失败", "receiving failed").to_string()
                    } else {
                        format!("{}（{}）", short, size_text)
                    };
                    if tw.get_action_text() != action_now.as_str() {
                        tw.set_action_text(action_now.into());
                    }
                    // × 的含义按方向分（接收 = 弹保留/不保留，发送 = 直接中断），
                    // 方向由引擎的 control 标志给出，不靠界面自己猜。
                    let sending_now = hfx_receiver::control::sending();
                    if tw.get_sending_now() != sending_now {
                        tw.set_sending_now(sending_now);
                    }
                    // 失败原因挂在窗口上（红色那行），并在主窗口的状态卡下面也留一句
                    if tw.get_failed() != failed {
                        tw.set_failed(failed);
                    }
                    let fail_now = failure_now.clone().unwrap_or_default();
                    if tw.get_fail_text() != fail_now.as_str() {
                        tw.set_fail_text(fail_now.into());
                    }
                    if let Some(reason) = failure_now.as_deref() {
                        let note = format!("{}：{}", i18n::tr("这次传输失败了", "the transfer failed"), reason);
                        if ui.get_notice_text() != note {
                            ui.set_notice_text(note.into());
                        }
                    }
                    // 百分比往下取整：向上四舍五入会在「1.99 / 2.00 GB」就跳成 100%，
                    // 而这时其实还在传（第一次真机就是 100% 挂着 3.5 MB/s，看着像卡死）。
                    // 没传完最多显示 99%，满字节了才给 100%。
                    let pct = if total_bytes > 0 {
                        ((done.min(total_bytes) as f64 / total_bytes as f64) * 100.0).floor() as i32
                    } else {
                        0
                    };
                    // 失败的时候按块算：真机那次字节数明明满了、却缺 2 块，
                    // 报「未完成 100%」是自相矛盾的，按块算出来是 99%。
                    let shown_pct = if failed {
                        // 用 set_failure 那一刻拍下来的百分比：失败之后 PROGRESS 可能已经
                        // 是上一轮 / 下一轮的数字（真机撞见过「未完成 100%」）
                        hfx_receiver::receiver::failure_percent().unwrap_or(99)
                    } else if finished {
                        100
                    } else {
                        pct.min(99)
                    };
                    if tw.get_percent() != shown_pct {
                        tw.set_percent(shown_pct);
                    }
                    let detail_now = format!("{} / {} · {}", human(done), human(total_bytes), speed_text);
                    if tw.get_detail_text() != detail_now.as_str() {
                        tw.set_detail_text(detail_now.into());
                    }
                    // ---- 展开「详细信息」里那三行「标签：值」（标签在 Slint 那边写死）----
                    // 1) 名称：当前文件名（和 action-text 用的是同一个 short，不带大小）
                    if tw.get_file_name() != short.as_str() {
                        tw.set_file_name(short.clone().into());
                    }
                    // 2) 剩余时间：算得出来给「x 分 y 秒」，算不出来给「正在计算...」——
                    //    原生的写法就是这个。刚起步（速率还没稳定）、暂停、已经收满都算「算不出来」。
                    let remain_bytes = total_bytes.saturating_sub(done);
                    let eta = if !hfx_receiver::control::paused()
                        && !finished
                        && speed > 0.5
                        && remain_bytes > 0
                    {
                        human_eta((remain_bytes as f64 / speed) as u64)
                    } else {
                        i18n::tr("正在计算...", "calculating...").to_string()
                    };
                    if tw.get_eta_text() != eta.as_str() {
                        tw.set_eta_text(eta.into());
                    }
                    // 3) 剩余项目：「项目数 (剩余大小)」。这一轮一次只有一个文件在收
                    //    （PROGRESS 只记「当前文件」），所以项目数是 0 / 1，
                    //    后面的括号里是**这个文件**还剩多少字节（和参考图的「1 (17.0 MB)」同形）。
                    let remain_now = format!(
                        "{} ({})",
                        if remain_bytes > 0 { 1 } else { 0 },
                        human(remain_bytes)
                    );
                    if tw.get_remain_text() != remain_now.as_str() {
                        tw.set_remain_text(remain_now.into());
                    }
                    // ---- 展开态图表条：速度曲线采样 + 拼 Path + 「速度:」那行 ----
                    // 横轴是**进度**：把 0..1 切成 CURVE_BINS 格，每格记那一刻的速率。
                    // 写进格子的值就不再改 —— 曲线只向右长，画过的部分一个像素都不动。
                    // 暂停 / 失败时不写，波形冻在那一刻（原生暂停也是这样）。
                    let paused_now = hfx_receiver::control::paused();
                    // 回退检测要**狠一点**：进度是从 PROGRESS 里几千个块的计数算出来的，
                    // 个别块先完成后重置，会让 fill_frac 瞬间回弹几个百分点。
                    // 以前用的是「回退超过 2%」—— 于是一帧抖动就把曲线历史清空一次，
                    // 曲线上永远只剩最近几个桶，前面整段贴底（实测非零桶只有 4~10 个，
                    // 而进度已经 15%，也就是本该 60 个）。
                    // 真正的新一轮必然是「回到接近 0」，所以要求回退超过 25% 且新进度 < 10%。
                    if curve_last_progress - fill_frac > 0.25 && fill_frac < 0.10 {
                        // 进度大幅回退 = 新一轮开始（上一轮传完 / 被取消），历史作废
                        curve_bins.iter_mut().for_each(|b| *b = 0.0);
                        curve_bin_cnt.iter_mut().for_each(|c| *c = 0);
                        curve_cur_progress = 0.0;
                        curve_last_at = None;
                        curve_max_speed = 0.0;
                        curve_peak_win.clear();
                        curve_win.clear();
                        curve_bin_cnt.iter_mut().for_each(|c| *c = 0);
                    }
                    curve_last_progress = fill_frac;
                    // ---- 曲线专用速率：300ms 滑窗 ----
                    let curve_now = std::time::Instant::now();
                    curve_win.push((curve_now, received));
                    while curve_win
                        .first()
                        .map(|(t, _)| curve_now.duration_since(*t).as_secs_f32() > 2.0)
                        .unwrap_or(false)
                    {
                        curve_win.remove(0);
                    }
                    // 取「至少 300ms 前」的那个点当锚：窗口还不够长时用最老的点，
                    // 这样刚起步时也有一个（偏小的）速率可用，而不是一路 0。
                    let anchor = curve_win
                        .iter()
                        .rev()
                        .find(|(t, _)| curve_now.duration_since(*t).as_millis() >= 300)
                        .copied()
                        .unwrap_or_else(|| *curve_win.first().unwrap_or(&(curve_now, received)));
                    let curve_elapsed = curve_now.duration_since(anchor.0).as_secs_f64();
                    let curve_instant = if curve_elapsed > 0.05 {
                        (received.saturating_sub(anchor.1) as f64 / curve_elapsed) as f32
                    } else {
                        0.0
                    };
                    // ---- 量程：最近 5 秒的 90 分位 × 1.1 ----
                    //
                    // 用**分位数**而不是最大值：偶发的突发（实测 66.8 MB/s，正常 21）
                    // 会把「最大值」顶到天上，量程跟着涨 3 倍，整条曲线被压成贴底。
                    // 90 分位丢掉最高的那 10%，这些突发既看不见、也毁不掉画面。
                    curve_peak_win.push(curve_instant);
                    if curve_peak_win.len() > 125 {
                        curve_peak_win.remove(0);
                    }
                    let peak_now = {
                        let mut v: Vec<f32> = curve_peak_win.iter().copied().collect();
                        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                        let idx = ((v.len() as f32) * 0.9) as usize;
                        v.get(idx.min(v.len().saturating_sub(1))).copied().unwrap_or(0.0)
                    };
                    let want_max = chart_max_speed(peak_now);
                    if want_max > curve_max_speed {
                        // 涨：立刻跟上（不然曲线会冲出量程被削平）
                        curve_max_speed = want_max;
                    } else if curve_max_speed > want_max * 1.15 {
                        // 跌：只在水花明显偏大时收敛，每拍 0.2%
                        curve_max_speed =
                            (curve_max_speed - (curve_max_speed - want_max) * 0.002).max(want_max);
                    }
                    let curve_due = curve_last_at
                        .map(|t| curve_now.duration_since(t).as_millis() >= CURVE_SAMPLE_MS)
                        .unwrap_or(true);
                    if curve_due && !paused_now && !failed {
                        curve_last_at = Some(curve_now);
                        // 从上次写到的位置一直填到当前进度：中间的格子沿用这一拍的速率。
                        // （不填的话进度一快，格子之间会留下 0 值，曲线变成一排向下的尖刺。）
                        let upto = if finished || fill_frac >= 0.999 {
                            1.0f32
                        } else {
                            fill_frac.clamp(0.0, 1.0)
                        };
                        let v = curve_instant.max(0.0);
                        let from = curve_cur_progress;
                        // 首帧（from==0）如果速率还没算出来，**先别填**。
                        // 这是「深绿面积没有」的真凶：传输窗口在进度 13% 才建，
                        // 首帧要从 0 一口气补 53 格，可那一拍的 speed 还是 0，
                        // 于是 53 格全被 0 填死 —— 后面再填也只在 53 格之后加，
                        // 曲线左边整段贴底（实测非零桶稳定只有 5~7 个）。
                        // 速率一上来（>1KB/s）再补，那一次才是有意义的波形。
                        let ready_to_fill = from > 0.0 || v >= 1024.0;
                        if std::env::var("HFX_DEBUG_CURVE").is_ok() {
                            log::info(&format!(
                                "采样 fill={:.4} v={:.0} 量程={:.0} 峰值={:.0} from={:.4} upto={:.4}",
                                fill_frac, v, curve_max_speed, peak_now, from, upto
                            ));
                        }
                        if upto > from && ready_to_fill {
                            // 首次填桶必须从 0 开始：传输窗口是在进度非零时才出现的
                            // （「详细信息」展开 / 窗口刚建），那一刻 fill_frac 可能已经 15%，
                            // 直接按 floor(0.15*400)=58 当起点时 bins[0..57] 就永远是 0，
                            // 曲线前面整段贴底，只剩最右一小截有高度。
                            // 所以首帧把 0..i1 整段补成当前速率，曲线从左边就是完整的一条。
                            let i0 = if from <= 0.0 {
                                0usize
                            } else {
                                (from * CURVE_BINS as f32).floor() as usize
                            };
                            let i1 = ((upto * CURVE_BINS as f32).ceil() as usize).min(CURVE_BINS);

                            for i in i0..i1 {
                                if let Some(b) = curve_bins.get_mut(i) {
                                    // 加权平均：同一个桶被多次采样时取均值（见 curve_bin_cnt 的说明）
                                    let c = curve_bin_cnt.get(i).copied().unwrap_or(0) as f32;
                                    *b = (*b * c + v) / (c + 1.0);
                                    if let Some(cnt) = curve_bin_cnt.get_mut(i) {
                                        *cnt = cnt.saturating_add(1);
                                    }
                                }
                            }
                            curve_cur_progress = upto;
                        }
                    }
                    // ---- 曲线 Path 降频重建 ----
                    // Path 每次换 commands 字符串，Slint 都要把 200 个点重新三角化 + 重绘。
                    // 这是整帧里最贵的一步，跟着 50ms 拍走会直接把帧预算吃光。
                    // 实测：曲线右边界每涨 0.2%（600MB 文件约 1.2MB）才需要重画一次，
                    // 且最少间隔 150ms —— 视觉上完全看不出延迟，帧率却能提上去。
                    let path_due = curve_path_at
                        .map(|t| t.elapsed().as_millis() >= 150)
                        .unwrap_or(true)
                        && (curve_cur_progress - curve_path_progress).abs() >= 0.002;
                    let curve_cmds = if path_due {
                        curve_path_at = Some(std::time::Instant::now());
                        curve_path_progress = curve_cur_progress;

                        build_curve_path(&curve_bins, curve_cur_progress, curve_max_speed)
                    } else {
                        tw.get_curve_path().to_string()
                    };
                    // 黑线：当前速度在刻度上的位置。暂停时钉在速度 0（贴底），
                    // 和「速度: 已暂停」那行字一致。
                    let guide_y = chart_guide_y(
                        if paused_now { 0.0 } else { speed as f32 },
                        curve_max_speed,
                    );
                    // 黑线：目标值是 0.1px 精度，但**每帧插值**逼近，所以线是连续滑动的。
                    let disp = match guide_y_disp {
                        // 第一帧 / 新一轮：直接落位，别从屏外滑进来
                        None => guide_y,
                        Some(prev) => prev + (guide_y - prev) * 0.22,
                    };
                    guide_y_disp = Some(disp);
                    // ---- 量化到整数像素再写属性 ----
                    // 黑线是 1px 高的横线，亚像素位置没有意义（原生就是整数像素）。
                    // 但「速度: xx MB/秒」那行字的 y 绑在 guide-y 上，**属性一变它就重排一次**，
                    // 中文重排很贵 —— 60Hz 下每帧都重排，实测展开态吃 74% 单核
                    // （收起态只有 20%，差的就是图表条这一块）。
                    // 量化到整数：黑线仍然每帧插值逼近，只是「跨过一整像素」才写一次属性，
                    // 60Hz 下每秒最多 60 次跳、每次 1px，视觉上就是连续滑动。
                    let quantized = disp.round();
                    if tw.get_guide_y() != quantized {
                        tw.set_guide_y(quantized);
                    }
                    if tw.get_curve_path().as_str() != curve_cmds {
                        tw.set_curve_path(curve_cmds.into());
                    }
                    // 参考图右上那行是「速度: 72.9 MB/秒」——单位沿用 human_speed 的量级切换
                    // （MB → KB → B），只把 "/s" 写成 "/秒"。
                    // 暂停时写「已暂停」而不是冻住的那个旧速率：上面那行大字已经是「已暂停」、
                    // 各通道速率全是 0 KB/s，这里再摆一个 10.0 MB/秒就是自相矛盾。
                    let chart_speed = if paused_now {
                        i18n::tr("已暂停", "paused").to_string()
                    } else {
                        human_speed(speed).replace("/s", "/秒")
                    };
                    let speed_line = format!("{}: {}", i18n::tr("速度", "Speed"), chart_speed);
                    if tw.get_speed_text() != speed_line.as_str() {
                        tw.set_speed_text(speed_line.into());
                    }

                    // 布尔 / 数值也要比较：Slint 收到「和上次一样的值」仍会标脏、重绘。
                    // 展开态那一帧要重画 476x776，60Hz 下不比较就是纯浪费。
                    if tw.get_paused() != paused_now {
                        tw.set_paused(paused_now);
                    }
                    if tw.get_done() != finished {
                        tw.set_done(finished);
                    }
                    // fill 是进度条长度：量化到 0.1% 再比，避免 60Hz 下为了 0.02px 重绘整窗
                    let fill_q = (fill_frac * 1000.0).round() / 1000.0;
                    if (tw.get_fill() - fill_q).abs() > 0.0005 {
                        tw.set_fill(fill_q);
                    }
                    tw.set_rows(rows_model.clone());
                    tw.set_segs(segs_model.clone());
                    let cancelling_now = hfx_receiver::control::cancel_requested();
                    if tw.get_cancelling() != cancelling_now {
                        tw.set_cancelling(cancelling_now);
                    }
                    // 流光只在「真在传」的时候扫：暂停 / 传完 / 速率掉到 0 都停住
                    let flow_now = !finished && !hfx_receiver::control::paused() && speed > 0.5;
                    if tw.get_flow_on() != flow_now {
                        tw.set_flow_on(flow_now);
                    }
                    // 精简：界面上不再显示「合计速率 · 剩余时间」（用户选的），这里照旧算一份
                    // 交给属性（日志和其它小窗复用），只是布局里不再占那一行高度。
                    let summary = if speed > 0.5 && total_bytes > done {
                        let remain = ((total_bytes - done) as f64 / speed) as u64;
                        format!("合计 {} · 剩余约 {}", human_speed(speed), human_eta(remain))
                    } else {
                        format!("合计 {} · 剩余时间未知", human_speed(speed))
                    };
                    if tw.get_summary_text() != summary.as_str() {
                        tw.set_summary_text(summary.into());
                    }
                    // 高度同步不放在这里：暂停等用户选「保留/不保留」的时候 active 已经是 false，
                    // 这一支根本不跑，对话框就会被 151 高的窗口裁掉（真机第一次点 ✕ 就是这个下场）。
                    // 下面 active 分支之外有一段专门干这个。
                    if failed {
                        // 失败了：停在窗口上等用户看见。3 秒自动收是对「传完了」才有的礼遇，
                        // 对失败用就是骗人（用户回头只看到文件在那儿，以为传好了）。
                        // 窗口要是已经收起来过（上一轮刚自动收完 / 用户点过收起），重新弹出来。
                        if !tw_shown && !xfer_dismissed.load(Ordering::Relaxed) {
                            tw_shown = true;
                            show_transfer_window(&tw, &mut s, &tw_state);
                        }
                        #[cfg(windows)]
                        if tw_shown {
                            let saved = s.config.xfer_win_pos;
                            if let Ok(mut g) = tw_state.lock() {
                                g.tick(&tw, saved);
                            }
                        }
                        if tw.get_details() {
                            tw.set_details(false);
                        }
                        tw_finish_at = None;
                    } else if finished {
                        // 传完了就把明细收起来：窗口回到收起高度，
                        // 3 秒的「已完成」不用顶着 400px 的空白
                        if tw.get_details() {
                            tw.set_details(false);
                        }
                        // 第一眼看到「满了」的时候记时间，3 秒后收窗口
                        if tw_finish_at.is_none() {
                            tw_finish_at = Some(std::time::Instant::now());
                        }
                        if tw_shown
                            && tw_finish_at
                                .map(|t| t.elapsed() >= Duration::from_secs(3))
                                .unwrap_or(false)
                        {
                            tw_shown = false;
                            xfer_dismissed.store(false, Ordering::Relaxed);
                            #[cfg(windows)]
                            {
                                let placed = tw_state.lock().ok().and_then(|g| g.placed_at);
                                save_transfer_pos(&tw, &mut s, placed);
                            }
                            let _ = tw.hide();
                        }
                    } else {
                        // 又一轮新传输（文件换了 / 续传接上了）：允许重新弹
                        tw_finish_at = None;
                        // 新一轮传输才重新弹（用户点过「收起」就别再烦他）
                        if !tw_shown && !xfer_dismissed.load(Ordering::Relaxed) {
                            tw_shown = true;
                            // 第一次出现：贴到工作区右下角（用户拖过就按记住的位置），
                            // 窗口建出来后再补几帧落位 + 置顶
                            show_transfer_window(&tw, &mut s, &tw_state);
                        }
                        #[cfg(windows)]
                        if tw_shown {
                            let saved = s.config.xfer_win_pos;
                            if let Ok(mut g) = tw_state.lock() {
                                g.tick(&tw, saved);
                            }
                        }
                    }
                }
            } else if failed {
                // 失败但已经不在传输中（收尾那一帧就走到了这里）：保证窗口还在，
                // 别让用户错过「接收失败」这一眼。
                if let Some(tw) = xfer_weak.upgrade() {
                    if !tw.window().is_visible() && !xfer_dismissed.load(Ordering::Relaxed) {
                        tw.set_done(true);
                        tw.set_failed(true);
                        tw.set_flow_on(false);
                        tw.set_fail_text(failure_now.clone().unwrap_or_default().into());
                        tw.set_action_text(i18n::tr("接收失败", "receiving failed").into());
                        tw.set_percent(hfx_receiver::receiver::failure_percent().unwrap_or(99));
                        tw_shown = true;
                        show_transfer_window(&tw, &mut s, &tw_state);
                    }
                }
            } else if tw_shown {
                // 传输结束：窗口自己收起来，下一轮重新允许弹出
                tw_shown = false;
                xfer_dismissed.store(false, Ordering::Relaxed);
                if let Some(tw) = xfer_weak.upgrade() {
                    tw.set_done(true);
                    #[cfg(windows)]
                    {
                        let placed = tw_state.lock().ok().and_then(|g| g.placed_at);
                        save_transfer_pos(&tw, &mut s, placed);
                    }
                    let _ = tw.hide();
                }
            }

            // 传输窗口的高度：明细展开 / 「接收失败」那行会改它，**每帧都要跟**。
            // （取消确认框改成独立窗口了，不再影响这个高度。）
            if let Some(tw) = xfer_weak.upgrade() {
                if tw.window().is_visible() {
                    // 框还开着、传输自己先跑完了：框里的三个选项已经没有意义，
                    // 直接收掉（不替用户选 —— 文件已经在盘上了，选「不保留」才会删，那是骗人）
                    if tw.get_cancel_ask() && hfx_receiver::receiver::last_failure().is_none() {
                        let pct = hfx_receiver::receiver::PROGRESS.snapshot();
                        let (_, done_now, total_now, _, _) = &pct;
                        if *total_now > 0 && *done_now >= *total_now {
                            tw.set_cancel_ask(false);
                        }
                    }
                    let details_now = tw.get_details();
                    let failed_now = hfx_receiver::receiver::last_failure().is_some();
                    if details_now != tw_details_on || failed_now != tw_failed_on {
                        tw_details_on = details_now;
                        tw_failed_on = failed_now;
                        let h = transfer_window_height(details_now, last_row_count, failed_now);
                        tw.window().set_size(slint::LogicalSize::new(460.0, h));
                        // 长高之后可能探出屏幕下沿（真机：明细一展开，后 6 行跑到屏幕外），
                        // 这里只做「越界才推回来」，不抢用户拖过的位置。
                        #[cfg(windows)]
                        if let Some(hwnd) =
                            tray_win::find_transfer_window(WINDOW_TITLE, &[PUSH_TITLE])
                        {
                            tray_win::keep_on_screen(hwnd, TW_MARGIN);
                        }
                    }
                }
            }

            // 更新提示 / 设备列表出现时调整窗口高度（否则底部按钮会被裁掉）
            // 进度卡已经从主界面删掉了（进度只在独立传输窗口里看），所以这里不再为它让高度。
            // 展开设备列表后内容会变高，窗口高度要跟着加，否则底部按钮被裁掉
            // 展开后是一行两个的小卡片：每行 46 高 + 行距 6，再留出通道换行与内边距
            let devices_h = if ui.get_devices_expanded() {
                let rows = ((s.devices.len() + 1) / 2).max(1) as f32;
                rows * 52.0 + 44.0
            } else {
                0.0
            };
            // 主界面那张进度卡删掉之后，400 的基准高度会剩一大块空白。
            // 但也不能一味压小：设置卡 + 三个勾选框 + 底部按钮本身就要 ~360，
            // 压到 300 时真机实测「开始接收」按钮直接被窗口裁掉（UIA 里都找不到）。
            // 370 还是不够（真机 409 高的窗口里底部按钮被裁掉一半），用回 400。
            let want_h = 400.0
                + if ui.get_ask_target() { 60.0 } else { 0.0 }
                // 状态卡下面那行「已取消…」的提示
                + if ui.get_notice_text().is_empty() { 0.0 } else { 20.0 }
                + devices_h
                + if ui.get_update_state() == 2 || ui.get_update_state() == 5 {
                    34.0
                } else {
                    0.0
                };
            if (want_h - last_want_h).abs() > 0.5 {
                last_want_h = want_h;

                let scale = ui.window().scale_factor();
                let current = ui.window().size();
                let width = current.width as f32 / scale;
                ui.window()
                    .set_size(slint::LogicalSize::new(width, want_h));
            }

            // ---- tick 耗时统计 ----
            let tick_dt = tick_t0.elapsed();
            tick_stat_sum += tick_dt.as_micros();
            tick_stat_n += 1;
            tick_stat_max = tick_stat_max.max(tick_dt.as_micros());
            if tick_stat_on && tick_stat_at.elapsed().as_secs() >= 1 {
                log::info(&format!(
                    "tick 统计：{} 次/秒 · 平均 {}us · 最慢 {}us",
                    tick_stat_n,
                    if tick_stat_n > 0 { tick_stat_sum / tick_stat_n as u128 } else { 0 },
                    tick_stat_max
                ));
                tick_stat_at = std::time::Instant::now();
                tick_stat_sum = 0;
                tick_stat_n = 0;
                tick_stat_max = 0;
            }
        });
    }

    // ---------- 更新检查 ----------
    start_update_check(ui.as_weak());

    {
        let ui_weak = ui.as_weak();
        ui.on_update_clicked(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            ui.set_update_state(3);
            ui.set_update_text("正在下载更新，完成后会自动重启…".into());
            let ui_weak2 = ui_weak.clone();
            std::thread::spawn(move || {
                match open_release_page() {
                    Ok(()) => {
                        post_ui(ui_weak2.clone(), move |ui| {
                            ui.set_update_state(5);
                            ui.set_update_text("已打开下载页面".into());
                        });
                    }
                    Err(e) => {
                        let text = format!("更新失败：{}", e);
                        post_ui(ui_weak2.clone(), move |ui| {
                            ui.set_update_state(4);
                            ui.set_update_text(text.into());
                        });
                    }
                }
            });
        });
    }

    // ---------- 系统托盘 / 关闭行为 ----------
    #[cfg(windows)]
    {
        let ui_weak = ui.as_weak();
        ui.window().on_close_requested(move || {
            let tray_enabled = ui_weak.upgrade().map(|ui| ui.get_tray()).unwrap_or(false);
            if tray_enabled {
                tray_win::hide_main_window(WINDOW_TITLE);
                // 不真正关窗，只把原生窗口隐藏，事件循环继续跑
                slint::CloseRequestResponse::KeepWindowShown
            } else {
                slint::CloseRequestResponse::HideWindow
            }
        });

        let ui_weak = ui.as_weak();
        tray_win::init(
            move || {
                if let Some(ui) = ui_weak.upgrade() {
                    let _ = ui.show();
                }
                tray_win::show_main_window(WINDOW_TITLE);
            },
            || {
                let _ = slint::quit_event_loop();
            },
        );
    }

    // --autostart：开机自启时直接开始接收（隐藏交给轮询定时器，等窗口真正创建好）
    if launch_at_boot {
        ui.invoke_start_clicked();
    }

    ui.run()
}

// ---------------- 更新：检查 / 下载 / 自我替换 ----------------

/// Slint 的属性只能在 UI 线程改，后台线程要用这个把更新送回事件循环
fn post_ui(
    ui_weak: slint::Weak<AppWindow>,
    action: impl FnOnce(&AppWindow) + Send + 'static,
) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade() {
            action(&ui);
        }
    });
}

const REPO: &str = "yizhiruonan/HybridFileXfer";
/// Release 里单独放一个接收器版本号文件，避免和整个项目的 tag（v3.1.0）混在一起
const VERSION_ASSET: &str = "receiver.version";
const EXE_ASSET: &str = "HybridFileXfer-Receiver.exe";

fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let t = text.trim();
    let t = t.strip_prefix('v').unwrap_or(t);
    let mut parts = t.split('.');
    let a = parts.next()?.trim().parse().ok()?;
    let b = parts.next()?.trim().parse().ok()?;
    let c = parts
        .next()
        .and_then(|x| x.trim().parse().ok())
        .unwrap_or(0);
    Some((a, b, c))
}

static UPDATE_EXE_URL: std::sync::LazyLock<Mutex<String>> =
    std::sync::LazyLock::new(|| Mutex::new(String::new()));
static UPDATE_VERSION: std::sync::LazyLock<Mutex<String>> =
    std::sync::LazyLock::new(|| Mutex::new(String::new()));

fn start_update_check(ui_weak: slint::Weak<AppWindow>) {
    std::thread::spawn(move || {
        let fail = |msg: &str| {
            let text = msg.to_string();
            post_ui(ui_weak.clone(), move |ui| {
                ui.set_update_state(4);
                ui.set_update_text(text.into());
            });
        };

        let api = format!("https://api.github.com/repos/{REPO}/releases/latest");
        let release: serde_json::Value = match ureq::get(&api)
            .set("User-Agent", "HybridFileXfer-Receiver")
            .set("Accept", "application/vnd.github+json")
            .timeout(Duration::from_secs(15))
            .call()
            .and_then(|r| r.into_json::<serde_json::Value>().map_err(Into::into))
        {
            Ok(v) => v,
            Err(_) => return fail("检查更新失败（网络不可用）"),
        };

        let mut version_url = String::new();
        let mut exe_url = String::new();
        if let Some(assets) = release["assets"].as_array() {
            for asset in assets {
                let name = asset["name"].as_str().unwrap_or_default();
                let url = asset["browser_download_url"].as_str().unwrap_or_default();
                if name == VERSION_ASSET {
                    version_url = url.to_string();
                } else if name == EXE_ASSET {
                    exe_url = url.to_string();
                }
            }
        }
        if version_url.is_empty() || exe_url.is_empty() {
            return fail("更新信息不完整（缺少接收器附件）");
        }

        let latest_text = match ureq::get(&version_url)
            .timeout(Duration::from_secs(15))
            .call()
            .and_then(|r| r.into_string().map_err(Into::into))
        {
            Ok(t) => t,
            Err(_) => return fail("检查更新失败（无法读取版本号）"),
        };

        let latest = match parse_version(&latest_text) {
            Some(v) => v,
            None => return fail("更新信息格式不正确"),
        };
        let current = parse_version(env!("CARGO_PKG_VERSION")).unwrap_or((0, 0, 0));

        if latest > current {
            if let Ok(mut guard) = UPDATE_EXE_URL.lock() {
                *guard = exe_url;
            }
            if let Ok(mut guard) = UPDATE_VERSION.lock() {
                *guard = format!("{}.{}.{}", latest.0, latest.1, latest.2);
            }
            let text = format!(
                "发现新版本 v{}.{}.{}（当前 v{}.{}.{}）",
                latest.0, latest.1, latest.2, current.0, current.1, current.2
            );
            post_ui(ui_weak.clone(), move |ui| {
                ui.set_update_state(2);
                ui.set_update_text(text.into());
            });
        } else {
            post_ui(ui_weak.clone(), move |ui| ui.set_update_state(1));
        }
    });
}

/// 不下载可执行文件（会被杀软当成 dropper），只在浏览器里打开 Release 页面
fn open_release_page() -> Result<(), String> {
    let url = "https://github.com/yizhiruonan/HybridFileXfer/releases/latest";
    std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn()
        .map_err(|e| format!("打开浏览器失败：{e}"))?;
    Ok(())
}

// ---------------- 工具函数 ----------------

/// 字节数 -> 人类可读
fn human(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b >= KB * KB * KB {
        format!("{:.2} GB", b / (KB * KB * KB))
    } else if b >= KB * KB {
        format!("{:.1} MB", b / (KB * KB))
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{} B", bytes)
    }
}

/// 速度（字节/秒）-> 人类可读
fn human_speed(bps: f64) -> String {
    // 单位要跟着量级走：蓝牙那种 0.2~0.3 MB/s 的通道，按 MB 只有一位小数会显示成 0，
    // 看着像断了；慢速一律降到 KB/s，再慢给 B/s，静态时为 0 KB/s
    let mb = bps / (1024.0 * 1024.0);
    if bps <= 0.0 {
        "0 KB/s".to_string()
    } else if mb >= 1.0 {
        format!("{:.1} MB/s", mb)
    } else if bps >= 1024.0 {
        format!("{:.0} KB/s", bps / 1024.0)
    } else {
        format!("{:.0} B/s", bps)
    }
}

/// 把速度历史拼成一条 Slint `Path` 的 commands（展开态图表条里那块曲线面积）。
///
/// 坐标系是**归一化**的：x 0..1000、y 0..76。
///   * 纵向 76 = 图表主体高度（app.slint 里 body 高 76px、curve 的 viewbox-height 76），
///     纵向 1:1 的像素，y=76 正好落在主体底沿 —— 面积的下沿就在那里。
///   * 横向 1000 由 viewbox 拉成条内宽，所以 Rust 不需要知道条子到底多宽。
///
/// 纵轴量程（`max_speed`）不是「历史最大值」，而是由 [[chart_max_speed]] 算出来的
/// 一个「整齐刻度」：原生实测 25 帧，曲线峰值稳定落在量程 55%~60% 的高度，
/// 即 maxSpeed ≈ 峰值 × 1.65 再取整到 1/1.5/2/2.5/3/4/5/6/8 × 10^k。
/// 用整齐刻度而不是峰值本身，黑线（当前速度）和曲线才共用同一把尺子，
/// 两者不会因为归一化口径不同而互相打架。
///
fn build_curve_path(bins: &[f32], progress: f32, max_speed: f32) -> String {
    // 纵向坐标仍然用 0..76（= 图表主体高度，1:1 像素），但曲线的**振幅只有 35px**：
    // 原生实测（naive-00 逐列量「条底 - 深绿顶边」）稳定落在 12..35px，
    // 满高 76px 是「自己想的撑满好看」，真机一比就露馅（深绿成了一大块高墙）。
    const VW: f32 = 1000.0;
    const BODY_H: f32 = 76.0;
    const AMP: f32 = 35.0;
    let upto = progress.clamp(0.0, 1.0);
    if upto <= 0.0 {
        // 还没有采样（刚打开 / 还没开始）：给空串，Slint 那边 Path 什么都不画，
        // 剩下的网格就是这个状态该有的样子。
        return String::new();
    }
    let max_v = max_speed.max(1.0);
    let x_of = |p: f32| (p.clamp(0.0, 1.0) * VW).round();
    let y_of = |v: f32| (BODY_H - (v / max_v).clamp(0.0, 1.0) * AMP).round();
    let ends = ((upto * bins.len() as f32).ceil() as usize).min(bins.len());
    // ---- 平滑：两遍 ±2 滑动平均（等价于三角核）----
    // 原生那条是**规整的三角齿**（naive-00 逐列量：35,35,35,19,20,16,12,13,17,20,18,13,12），
    // 齿距很均匀。我们这边源数据更碎（40ms 采样 + 块计数的量化），
    // 直接用会画成一把毛刷。两遍 ±2 之后碎噪声被压下去、周期性的起伏留下来。
    //
    // 只影响绘制，不改写进格子的值（所以画过的部分依然一个像素都不动）。
    // 预先把整段平滑好：平滑是逐点独立算的，不存在「这一帧多算几格就变样」的问题。
    let mut sm: Vec<f32> = bins[..ends].to_vec();
    for _ in 0..2 {
        let prev = sm.clone();
        for i in 0..prev.len() {
            let lo = i.saturating_sub(2);
            let hi = (i + 2).min(prev.len() - 1);
            let mut sum = 0.0f32;
            for v in &prev[lo..=hi] {
                sum += *v;
            }
            sm[i] = sum / (hi - lo + 1) as f32;
        }
    }
    let smooth = |i: usize| -> f32 { sm.get(i).copied().unwrap_or(0.0) };

    // ---- 抽稀：按**固定步长**取至多 CURVE_DRAW_POINTS 个点 ----
    // 步长只由 CURVE_BINS/CURVE_DRAW_POINTS 决定，和已画了多少无关 ——
    // 所以同一格每一帧抽到的永远是自己，曲线不会因为抽稀而左右挪。
    let mut out = String::with_capacity(CURVE_DRAW_POINTS * 22 + 32);
    out.push_str(&format!("M 0 {}", BODY_H));
    if ends <= CURVE_DRAW_POINTS {
        for i in 0..ends {
            let x = x_of((i as f32 + 0.5) / bins.len() as f32);
            out.push_str(&format!(" L {} {}", x, y_of(smooth(i))));
        }
    } else {
        let step = bins.len() as f32 / CURVE_DRAW_POINTS as f32;
        for k in 0..CURVE_DRAW_POINTS {
            let i = ((k as f32 + 0.5) * step) as usize;
            if i >= ends {
                break;
            }
            let x = x_of((i as f32 + 0.5) / bins.len() as f32);
            out.push_str(&format!(" L {} {}", x, y_of(smooth(i))));
        }
    }
    // 右边界 = 当前进度：面积只铺到已完成的地方，未完成区间里没有面积。
    let last_x = x_of(upto);
    let last_y = y_of(smooth(ends.saturating_sub(1)));
    out.push_str(&format!(" L {} {}", last_x, last_y));
    out.push_str(&format!(" L {} {} Z", last_x, BODY_H));
    out
}

/// 纵轴量程：把峰值放大 1.1 倍后取到一个「整齐刻度」。
///
/// 这个系数是拿原生帧逐列量出来的：naive-00 的深绿最高点正好 35px = 满振幅，
/// 也就是说**原生让峰值顶到满幅**（余量只在整齐刻度上多出来那一点）。
/// 最早写 1.65、后来 1.35，真机上和原生并排一比，我们的曲线只有人家一半高 ——
/// 系数越小峰值越高，1.1 时峰值落在 91% 的位置，和原生一致。
///
/// 取整齐刻度（1/1.5/2/2.5/3/4/5/6/8 × 10^k）而不是精确值，是为了让量程在一段传输里
/// 只跳少数几次 —— 每帧都跟着峰值缩放，整条曲线会像呼吸一样上下漂。
fn chart_max_speed(peak: f32) -> f32 {
    let want = (peak * 1.1).max(1.0);
    let mag = 10f32.powf(want.log10().floor());
    let norm = want / mag;
    let snapped = if norm <= 1.0 {
        1.0
    } else if norm <= 1.5 {
        1.5
    } else if norm <= 2.0 {
        2.0
    } else if norm <= 2.5 {
        2.5
    } else if norm <= 3.0 {
        3.0
    } else if norm <= 4.0 {
        4.0
    } else if norm <= 5.0 {
        5.0
    } else if norm <= 6.0 {
        6.0
    } else if norm <= 8.0 {
        8.0
    } else {
        10.0
    };
    snapped * mag
}

/// 黑线（速度基准线）在图表主体里的 y 坐标，0 = 顶、76 = 底。
///
/// 和曲线共用同一把尺子（振幅 35px），所以黑线就是「当前这一刻速度」在图上的高度：
/// 速度 0 → 贴底 75，速度 = 量程 → 抬到 76-35 = 41。
/// 原生实测 25 帧，黑线的相对高度在 7..35px 之间来回跳（速度 84.7→78.1→54.3→…），
/// 严格跟着速度走 —— 这正是用户说的「黑线要会跳」。
/// 下限 41 而不是 0：曲线和黑线都只占底部 35px，上面那 41px 是留给
/// 「速度: xx MB/秒」那行字的位置（那行字恒在黑线上方 6px）。
fn chart_guide_y(speed: f32, max_speed: f32) -> f32 {
    let frac = (speed / max_speed.max(1.0)).clamp(0.0, 1.0);
    (76.0 - frac * 35.0).round().clamp(41.0, 75.0)
}

fn detect_lang() -> Lang {
    #[cfg(windows)]
    {
        extern "system" {
            fn GetUserDefaultUILanguage() -> u16;
        }
        let id = unsafe { GetUserDefaultUILanguage() };
        if (id & 0x3FF) == 0x04 {
            return Lang::Zh;
        }
    }
    if let Ok(lang) = std::env::var("LANG") {
        if lang.to_ascii_lowercase().starts_with("zh") {
            return Lang::Zh;
        }
    }
    Lang::En
}

fn open_in_explorer(path: &str) {
    let _ = std::process::Command::new("explorer").arg(path).spawn();
}

/// 开机自启用注册表 Run 项（HKCU），不要写启动目录的 .vbs 脚本——
/// 那种"释放脚本到启动目录"的行为会被杀软判为 dropper。
fn autostart_value_name() -> Vec<u16> {
    "HybridFileXfer-Receiver\0".encode_utf16().collect()
}

fn run_key_path() -> Vec<u16> {
    "Software\\Microsoft\\Windows\\CurrentVersion\\Run\0"
        .encode_utf16()
        .collect()
}

const AUTOSTART_TASK: &str = "HybridFileXfer-Receiver";

fn hidden_command(program: &str) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    command
}

#[cfg(windows)]
fn logon_task_exists() -> bool {
    hidden_command("schtasks")
        .args(["/Query", "/TN", AUTOSTART_TASK])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(windows)]
fn create_logon_task(exe: &Path) -> bool {
    let tr = format!("\"{}\" --autostart", exe.display());
    hidden_command("schtasks")
        .args([
            "/Create", "/TN", AUTOSTART_TASK, "/TR", &tr, "/SC", "ONLOGON", "/RL", "LIMITED", "/F",
        ])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(windows)]
fn delete_logon_task() {
    let _ = hidden_command("schtasks")
        .args(["/Delete", "/TN", AUTOSTART_TASK, "/F"])
        .status();
}

/// 注册表 Run 项里有没有我们的条目
#[cfg(windows)]
fn run_value_present() -> bool {
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ,
    };
    let mut key: HKEY = std::ptr::null_mut();
    let sub = run_key_path();
    let status = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, KEY_READ, &mut key) };
    if status != 0 {
        return false;
    }
    let name = autostart_value_name();
    let mut size: u32 = 0;
    let query = unsafe {
        RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    unsafe { RegCloseKey(key) };
    query == 0 && size > 0
}

fn startup_shortcut_path() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(
        PathBuf::from(appdata)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
            .join("Startup")
            .join("HybridFileXfer-Receiver.lnk"),
    )
}

fn startup_shortcut_present() -> bool {
    startup_shortcut_path().map(|p| p.exists()).unwrap_or(false)
}

fn write_startup_shortcut(exe: &Path) -> bool {
    let Some(lnk) = startup_shortcut_path() else {
        return false;
    };
    let Some(dir) = exe.parent() else {
        return false;
    };
    if let Some(parent) = lnk.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let ps = format!(
        "$s=(New-Object -ComObject WScript.Shell).CreateShortcut('{}'); $s.TargetPath='{}'; $s.Arguments='--autostart'; $s.WorkingDirectory='{}'; $s.IconLocation='{},0'; $s.Save()",
        lnk.display(),
        exe.display(),
        dir.display(),
        exe.display()
    );
    let encoded = hfx_receiver::sendto::encode_utf16le_base64(&ps);
    let ok = hidden_command("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-EncodedCommand",
            &encoded,
        ])
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    ok && lnk.exists()
}

fn remove_startup_shortcut() {
    if let Some(lnk) = startup_shortcut_path() {
        let _ = std::fs::remove_file(lnk);
    }
}

#[cfg(windows)]
fn autostart_enabled() -> bool {
    // 计划任务 / 启动目录快捷方式 / 注册表 Run 任一存在都算勾选
    logon_task_exists() || startup_shortcut_present() || run_value_present()
}

#[cfg(not(windows))]
fn autostart_enabled() -> bool {
    false
}

#[cfg(windows)]
fn write_run_value(exe: &Path) -> io::Result<()> {
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ,
    };
    let mut key: HKEY = std::ptr::null_mut();
    let sub = run_key_path();
    let status =
        unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, KEY_SET_VALUE, &mut key) };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let name = autostart_value_name();
    let value = format!("\"{}\" --autostart", exe.display());
    let data: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
    let result = unsafe {
        RegSetValueExW(
            key,
            name.as_ptr(),
            0,
            REG_SZ,
            data.as_ptr() as *const u8,
            (data.len() * 2) as u32,
        )
    };
    unsafe { RegCloseKey(key) };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    Ok(())
}

#[cfg(windows)]
fn delete_run_value() -> io::Result<()> {
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegDeleteValueW, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE,
    };
    let mut key: HKEY = std::ptr::null_mut();
    let sub = run_key_path();
    let status =
        unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, KEY_SET_VALUE, &mut key) };
    if status != 0 {
        return Ok(());
    }
    let name = autostart_value_name();
    unsafe { RegDeleteValueW(key, name.as_ptr()) };
    unsafe { RegCloseKey(key) };
    Ok(())
}

/// 优先用「登录时触发的计划任务」：Windows 11 的「启动应用」列表不会拦计划任务，
/// 比 HKCU\Run 可靠；建任务失败再退回注册表。
#[cfg(windows)]
fn set_autostart(enabled: bool, exe_dir: &Path) -> io::Result<()> {
    let exe = exe_dir.join("HybridFileXfer-Receiver.exe");
    if enabled {
        // 计划任务最稳，但非管理员建不了；退而求其次用启动目录快捷方式；
        // 都不行再退回注册表 Run。每次只保留一种，避免登录时起两次。
        if create_logon_task(&exe) {
            let _ = delete_run_value();
            remove_startup_shortcut();
            Ok(())
        } else if write_startup_shortcut(&exe) {
            let _ = delete_run_value();
            delete_logon_task();
            Ok(())
        } else {
            write_run_value(&exe)
        }
    } else {
        delete_logon_task();
        remove_startup_shortcut();
        let _ = delete_run_value();
        Ok(())
    }
}

#[cfg(not(windows))]
fn set_autostart(_enabled: bool, _exe_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// 旧版本用启动目录 .vbs 实现自启，启动时顺手清掉
fn remove_legacy_autostart_script() {
    if let Some(appdata) = std::env::var_os("APPDATA") {
        let legacy = PathBuf::from(appdata)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
            .join("Startup")
            .join("HybridFileXfer-Receiver.vbs");
        if legacy.exists() {
            let _ = std::fs::remove_file(legacy);
        }
    }
}

#[cfg(windows)]
fn pick_folder() -> Option<PathBuf> {
    use std::ffi::c_void;

    #[repr(C)]
    struct BrowseInfoW {
        hwnd_owner: isize,
        pidl_root: *mut c_void,
        psz_display_name: *mut u16,
        lpsz_title: *mut u16,
        ul_flags: u32,
        lpfn: isize,
        l_param: isize,
        i_image: i32,
    }

    #[link(name = "shell32")]
    extern "system" {
        fn SHBrowseForFolderW(info: *mut BrowseInfoW) -> *mut c_void;
        fn SHGetPathFromIDListW(pidl: *const c_void, path: *mut u16) -> i32;
    }

    #[link(name = "ole32")]
    extern "system" {
        fn CoTaskMemFree(pv: *mut c_void);
    }

    let title: Vec<u16> = "选择接收文件夹\0".encode_utf16().collect();
    let mut info = BrowseInfoW {
        hwnd_owner: 0,
        pidl_root: std::ptr::null_mut(),
        psz_display_name: std::ptr::null_mut(),
        lpsz_title: title.as_ptr() as *mut u16,
        ul_flags: 0x0001 | 0x0040 | 0x0010,
        lpfn: 0,
        l_param: 0,
        i_image: 0,
    };
    unsafe {
        let pidl = SHBrowseForFolderW(&mut info);
        if pidl.is_null() {
            return None;
        }
        let mut buffer = [0u16; 1024];
        let ok = SHGetPathFromIDListW(pidl, buffer.as_mut_ptr()) != 0;
        CoTaskMemFree(pidl);
        if !ok {
            return None;
        }
        let len = buffer.iter().position(|&c| c == 0).unwrap_or(0);
        Some(PathBuf::from(String::from_utf16_lossy(&buffer[..len])))
    }
}

#[cfg(not(windows))]
fn pick_folder() -> Option<PathBuf> {
    None
}