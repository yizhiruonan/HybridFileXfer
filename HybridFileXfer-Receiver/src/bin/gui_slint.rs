//! 图形界面版接收器（Slint + fluent 样式，Windows 11 观感）。

// 一律使用 Windows 子系统：GUI 版不要控制台窗口（日志写 receiver.log）
#![windows_subsystem = "windows"]

slint::include_modules!();

use std::cell::RefCell;
use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::rc::Rc;
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
// VecModel / ModelRc 的 row_count / set_row_data 是 Model trait 的方法，得引进来才有
use slint::Model as _;

#[cfg(windows)]
mod tray_win;

/// 窗口标题（托盘线程靠它找到主窗口）
const WINDOW_TITLE: &str = "多轨快传";
/// 传输窗口标题（"正在传输" / "已完成" / "接收失败" 会变，所以只在需要时按它找主窗口）
#[allow(dead_code)]
const TRANSFER_TITLE: &str = "正在传输";
/// 取消确认框的窗口标题（app.slint 里 CancelAskWindow 的 title，两边要一致）
const ASK_TITLE: &str = "取消接收";
/// 发送方向的确认框标题（窗口按方向换标题，找 hwnd 时两个都要试）
const ASK_TITLE_SEND: &str = "取消发送";
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

/// 一个传输窗口的「落位 / 置顶」状态。
///
/// 一台设备一个窗口之后，这份状态也**每窗口一份**（挂在 DevXferWin 里），
/// 不再是整进程一个单例：
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
    /// `suffix` = 这个窗口标题的后缀（" · 小米 15"）。一台设备一个窗口之后**只能按后缀
    /// 认窗口**：以前那套「本进程第一个带标题的窗口」在多窗口下永远只命中最早建出来的
    /// 那个，第二台设备的窗口就永远摆不上位置（见 tray_win::find_transfer_window_of）。
    /// `slot` = 这个窗口在创建顺序里的位置（0 = 最下面那个）：第 i 个窗口往上错开
    /// i*(可见高+12)，单窗口时 slot=0 —— 和改之前「贴右下角」一个像素都不差。
    ///
    /// 摆位置分两种：
    ///   * 用户拖过（config 里有坐标，只有恰好一个窗口时才会传进来）→ 照那个坐标
    ///     （show 之前已经 set_position 过一次）；
    ///   * 没拖过 → 贴着工作区右下角（+ 往上错开）。这一步用「量一下现在的可见右下角
    ///     离工作区还差多少、再挪多少」的写法（tray_win::nudge_to_bottom_right_up），
    ///     不去猜标题栏和边框多高，多显示器也是按窗口自己所在那块屏算。
    /// `up` = 这一帧该用的错开量（调用方按「下面那些窗口有多高」算好，见 stack_up_for）。
    fn tick(&mut self, tw: &TransferWindow, suffix: &str, saved: Option<(i32, i32)>, slot: usize, up: i32) {
        if self.settle == 0 {
            return;
        }
        self.settle -= 1;
        if self.hwnd.is_none() {
            self.hwnd = tray_win::find_transfer_window_of(suffix);
        }
        let Some(hwnd) = self.hwnd else {
            // 找不到 HWND 就等于不摆位：窗口会停在 show_at 那个**估算**位置上。
            // 真机：多窗口时偶发两窗叠在一起、或者一个被摆到屏幕最顶上，日志里却干干净净 ——
            // 就是因为这条早退不吭声。只在最后一帧记，免得刷屏。
            if self.settle == 0 {
                log::debug(&format!("摆位放弃 {}：6 帧内没找到窗口", suffix));
            }
            return;
        };

        if !self.anchored {
            if let Some((x, y)) = saved {
                // 记住的位置：show 之前已经放好了，这里只是再确认一次
                tw.window().set_position(slint::PhysicalPosition::new(x, y));
                self.anchored = true;
            } else if tray_win::nudge_to_bottom_right_up(hwnd, TW_MARGIN, up) {
                self.anchored = true;
            }
            // 多窗口错开摆位出过「两个窗口叠在同一处」的真机症状（偶发），
            // 光看最终位置分不清是槽位给了 0 还是 nudge 没生效，这里把三个数一次记全。
            log::debug(&format!(
                "摆位 {}：槽位 {} 错开 {} px，nudge 后位置 {:?}",
                suffix,
                slot,
                up,
                tray_win::window_position(hwnd)
            ));
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

/// 多窗口时上下两个窗口之间留的空隙（可见边之间）。
///
/// 12 和标题栏是一个量级：贴太近看着像一整块，离太远又要吃掉半块屏幕。
#[cfg(windows)]
const TW_STACK_GAP: i32 = 12;

/// 量不到窗口高时用的估值（一张卡片那种窗口）。
#[cfg(windows)]
const TW_STACK_H_GUESS: i32 = 195;

/// 第 slot 个窗口往上错开多少像素 = **比它低的那些窗口**的可见高之和 + 每个缝隙。
///
/// 这个口径踩过两个坑，都别改回去：
///
/// 1. 按**自己**的可见高算（slot *（自己的高 + 缝隙））→ 窗口每长高一次错开量就变大一次，
///    而收尾时下边界又跟着抬高，于是窗口一级级往上爬：真机 3 单排队实测 200 → 314 → 428，
///    最后一个顶到屏幕最上沿。
/// 2. 落位那一刻量一次、之后定死 → 那时候窗口才 42~86 高，窗口后来长到 195 就压住了
///    上面那台：真机截图里小米那个窗口只露出 86px 标题栏，进度条和暂停/取消按钮全被
///    红米窗口盖着，看着像「只传了一个」。
///
/// 按「下面那些窗口现在有多高」算就没有这两个毛病：自己长高只往上长，下面那台长高
/// 上面这台才跟着抬高，两边都是确定的量，不会累积。
///
/// 量不到（窗口还没建出来）就先按 TW_STACK_H_GUESS 估，下一帧量到了自然就对了。
#[cfg(windows)]
fn stack_up_for(wins: &XferWins, keys: &[String], slot: usize) -> i32 {
    let mut acc = 0;
    for k in keys.iter().take(slot) {
        let h = wins
            .map
            .get(k)
            .and_then(|d| tray_win::find_transfer_window_of(&d.suffix))
            .and_then(|hw| tray_win::window_visible_rect(hw))
            .map(|(_, t, _, b)| b - t)
            .filter(|h| *h > 0)
            .unwrap_or(TW_STACK_H_GUESS);
        acc += h + TW_STACK_GAP;
    }
    acc
}

/// 一台设备的独立传输窗口（每台设备一个 OS 窗口，键 = 设备串号）。
///
/// 以前只有一个单例 `TransferWindow` + 一堆裸变量（tracks / ops_model / tw_shown /
/// last_tw_h / xfer_dismissed）。现在窗口、模型、滚动状态、落位状态、收起标志全按设备
/// 各存一份，键和设备串号对齐（= OpTrack.serial = receiver::progress_for 那把键）。
///
/// **Slint 对象不能跨线程**：这个结构只活在 UI 线程的那个 16ms Timer 里，
/// 被 `Rc<RefCell<XferWins>>` 包着（不 Send），建窗口 / 刷模型 / 摆位置全在那一条线程上。
struct DevXferWin {
    /// 设备串号（map 的键；也是这个窗口里的块读哪一份进度的依据）
    serial: String,
    /// 设备显示名（窗口标题里 " · " 后面那一段的来源）
    label: String,
    /// 标题后缀 " · <label>"：标题靠它唯一，找 HWND 也靠它（tray_win::find_transfer_window_of）
    suffix: String,
    /// 窗口本体。Slint 组件不 Send，所以它只出现在这里、整条链都在 UI 线程上
    win: TransferWindow,
    /// 这个窗口自己的列表 model：**全程只建这一个**。块数变了才 set_vec，否则只写内容
    /// 变了的那几行 —— 每建一个新的 VecModel 就等于告诉 Slint「整个列表全变了」，
    /// 它会把所有块连同里头上百个元素全拆了重建。
    model: Rc<slint::VecModel<XferOp>>,
    /// 这个窗口里的块：一条任务一个 OpTrack。现在一台设备一条任务；将来同一台设备并行
    /// 几条时这里会变成 N 个，窗口内的多块堆叠逻辑一个字都不用改。
    tracks: Vec<OpTrack>,
    /// 上次写进模型的那几块「等待中」（手机队列里 state == 0 的那些）。
    ///
    /// 它们接在 tracks 后面，所以模型里第 i 行不再一定对应 tracks[i] —— 这部分单独记一份，
    /// 「内容没变就不写回模型」那套比较才能照样做（Slint 收到一样的值也会重绘整窗）。
    waiting_sent: Vec<XferOp>,
    /// 这个窗口自己的落位 / 置顶状态（以前整进程只有一个实例）
    #[cfg(windows)]
    tw: TwWindow,
    /// 用户在**这个窗口**上点过收起：这一轮别再弹它（别的设备的窗口不受影响）
    dismissed: bool,
    /// 「详细信息」展开了没有（窗口最底下那行开关，一次性切换这个窗口里所有块）
    details: bool,
    /// 上一帧设过的窗口高度：变了才 set_size（set_size 会触发一次重排 + 落位）
    last_h: f32,
    /// 上一帧量到的真实窗口高度（逻辑像素）。只为了能有日志说清「谁把它改小了」。
    last_actual_h: f32,
    /// 取消前最后一张正常的卡片。
    ///
    /// 为什么要冻住：卡片上的字节数来自 PROGRESS，而取消会把 PROGRESS 清空，
    /// 重算那一帧就变成「0% · 0 B / 0 B」。用户点的是「保留进度」，卡片上就该停在
    /// 断开那一刻的数字上，而不是把一个已经传了几十 MB 的任务显示成 0。
    frozen_op: Option<XferOp>,
    /// 上次摆位用的槽位（往上错开的档位）。窗口数一变槽位就变，这时要按新槽位重排一遍；
    /// 槽位没变就一个像素都不动 —— 用户拖过的位置得保住。
    slot_seen: usize,
    /// 上次摆位时的窗口总数。1 个窗口和多个窗口的摆位规则不一样（单窗口优先用户拖过的
    /// 坐标，多窗口一律从右下角往上排），所以总数一变就得重排。
    stack_seen: usize,
    /// 这一轮弹过没有（对应以前单例上的 tw_shown）
    shown: bool,
    /// 这个窗口「上一单已经收尾、下一单还没开始」的空档是从什么时候开始的。
    ///
    /// 为什么要它：手机端多选时是一个文件一个任务、同一刻只传一个。两单之间电脑端
    /// 有一小段真空（进度被 reset，total = 0，active 落成 false）。以前这一段走到
    /// 「这一帧没有块了」的分支里，**把窗口整个销毁**，下一单再建一个新的 ——
    /// 用户看到的就是「一个任务传完，窗口消失一下又打开」（2026-11-22 真机报的）。
    /// 现在留着窗口等下一单，只有空档超过 TW_IDLE_KEEP_SECS 才真销毁。
    idle_since: Option<std::time::Instant>,
}

/// 「上一单收尾了、下一单还没来」的窗口最多留这么久（秒）。
///
/// 队列模式下两单之间的真空只有零点几秒，这个值把它整个盖住；而一次普通传输结束、
/// 用户也没再分享时，窗口在「已完成」显示 3 秒后就已经藏起来了，这里只是把 Slint
/// 对象多留一会儿（再建一个的成本 = 几十个元素的布局 + 一次 show，比一闪便宜）。
const TW_IDLE_KEEP_SECS: u64 = 30;

/// 「已完成」要等这么久才认（毫秒）。
///
/// 队列模式下两单之间只有零点几秒：一转身就把标题改成「已完成」、下一单再翻回
/// 「正在传输」，标题栏在那儿闪（真机 2026-11-22 的窗口标题采样里能抓到
/// 「已完成」只亮了 0.4 秒）。等这么久还闲着才认「这一轮传完了」。
const TW_DONE_QUIET_MS: u128 = 800;

impl DevXferWin {
    /// 这个窗口在我们摆的位置（用户拖走之后和它比，就知道该不该记进 config）
    fn placed_at(&self) -> Option<(i32, i32)> {
        #[cfg(windows)]
        {
            self.tw.placed_at
        }
        #[cfg(not(windows))]
        {
            None
        }
    }

    /// 按新槽位重排一遍（窗口数变了才调）。重置之后 settle() 会重新量、重新吸到位。
    fn re_anchor(&mut self) {
        #[cfg(windows)]
        self.tw.begin();
    }

    /// 补一帧「摆位置 + 置顶」（只有窗口刚 show 出来那几帧真的做事，见 TwWindow::tick）
    fn settle(&mut self, saved: Option<(i32, i32)>, slot: usize, up: i32) {
        #[cfg(windows)]
        self.tw.tick(&self.win, &self.suffix, saved, slot, up);
        #[cfg(not(windows))]
        let _ = (saved, slot, up);
    }

    /// 把窗口摆到该在的位置再显示，然后补一帧落位。
    ///
    /// 先按估算尺寸摆个大概位置（省得在系统默认位置闪一下），真实边框和尺寸由 settle()
    /// 量出来再吸过去 —— 和改之前那个 show_transfer_window 同一个套路。
    fn show_at(&mut self, saved: Option<(i32, i32)>, slot: usize, up: i32) {
        #[cfg(windows)]
        {
            let (pw, ph) = transfer_window_px(&self.win);
            // +16 / +40 是边框和标题栏的估算值，只影响这一两帧
            let origin = tray_win::transfer_window_origin(pw + 16, ph + 40, saved);
            // 往上错开一段再 show：否则出来那一瞬所有窗口都叠在右下角闪一下。
            // 用的是调用方按「下面那些窗口有多高」算好的 up（不是 slot*(自己的高)）。
            let origin = (origin.0, origin.1 - up);
            log::debug(&format!(
                "预摆位 {}：槽位 {} 估算 {}x{} 错开 {} px → {:?}",
                self.suffix, slot, pw, ph, up, origin
            ));
            self.tw.begin();
            self.tw.placed_at = Some(origin);
            self.win
                .window()
                .set_position(slint::PhysicalPosition::new(origin.0, origin.1));
        }
        #[cfg(not(windows))]
        let _ = (saved, slot);
        let _ = self.win.show();
        self.settle(saved, slot, up);
    }
}

/// 界面回调要交给主循环（16ms Timer）去做的那些「脏活」。
///
/// **为什么要有这一层**（2026-11-22 关窗卡死的根因）：
/// 16ms 那条 Timer 从拿 `State` 锁开始一直持到整帧结束，中间会调 Slint 的
/// `show()` / `set_size()` / `SetForegroundWindow` —— 这些都会进 Windows 的消息泵，
/// 于是**界面回调可能在 Timer 里被重入**。回调要是自己也去 `state.lock()`，
/// 同一个线程对自己已经持有的 std::Mutex 再 lock 一次 = 自死锁，
/// 主窗口「未响应」，引擎也停摆（用户看到的「两边一起卡死」）。
///
/// 所以界面回调里**一律不碰 State 锁**：只把要做的事（记窗口坐标、写取消提示）
/// 丢进这个小队列，由那条本来就持锁的 Timer 顺手做掉。
/// 这把锁本身只在极短的几行里持有，绝不跨任何 Slint / Windows 调用。
#[derive(Default)]
struct UiPending {
    /// 收起 / 关窗时要记下的窗口坐标（物理像素）：(用户拖到的位置, 我们自己摆的位置)
    pos_saves: Vec<((i32, i32), Option<(i32, i32)>)>,
    /// 取消确认框选了「保留 / 不保留」之后要写进 `State.cancel_note` 的那句话
    cancel_note: Option<String>,
}

/// 所有设备的传输窗口（UI 线程私有，理由见 DevXferWin）。
#[derive(Default)]
struct XferWins {
    map: std::collections::HashMap<String, DevXferWin>,
    /// **创建顺序**：第 i 个窗口摆位时往上错开 i*(窗口高+12)，所以顺序必须稳定 ——
    /// 照 map 的遍历顺序排的话，每帧每个窗口的位次都可能变，窗口会互相跳。
    order: Vec<String>,
    /// 最近出现过的设备串号：传输收尾、设备表也空了的失败帧还得有个窗口把红字亮出来，
    /// 窗口被移除之后照这个串号重建一个（见 tick 里的失败分支）。
    last: Option<String>,
}

impl XferWins {
    /// 登记一个新窗口（创建顺序追加在末尾 = 摆位时排在上一个上面）
    fn insert(&mut self, serial: String, d: DevXferWin) {
        if !self.order.iter().any(|k| k == &serial) {
            self.order.push(serial.clone());
        }
        self.map.insert(serial, d);
    }

    /// 这个窗口的槽位（创建顺序里的位置，0 = 最下面那个）
    fn slot_of(&self, serial: &str) -> usize {
        self.order.iter().position(|k| k == serial).unwrap_or(0)
    }
}

/// 收起一台设备的窗口并从表里移除（这台设备这一轮没有块了 / 整轮结束了）。
///
/// 顺序是「先从表里摘下来，再记坐标，再藏」：窗口的显示状态挂在 Slint 自己那个 Rc 上，
/// 表里那一条一删、强引用一断，系统窗口就没了 —— 不藏的话屏幕上会留着最后一帧。
/// 顺手把用户拖过的坐标记进 config，但**只有恰好一个窗口时**才记：多窗口是自动从右下角
/// 往上排的，一份 xfer_win_pos 说不清是哪台设备的窗口（config 格式不为多窗口改）。
fn drop_dev_xfer_win(wins: &Rc<RefCell<XferWins>>, s: &mut State, serial: &str) {
    let removed = {
        let mut g = wins.borrow_mut();
        let single = g.map.len() == 1;
        let placed = if single {
            g.map.get(serial).and_then(|d| d.placed_at())
        } else {
            None
        };
        g.order.retain(|k| k != serial);
        g.last = Some(serial.to_string());
        g.map.remove(serial).map(|d| (d, single, placed))
    };
    if let Some((d, single, placed)) = removed {
        log::debug(&format!("窗口收掉：{}（设备这一轮没有块了）", serial));
        // 只有「这一轮真弹出来过」的窗口才记坐标：从没 show 过的窗口报告的坐标是假的
        // （以前那个单例走的是同一个判断 —— 只在 tw_shown 时保存）
        if single && d.shown {
            save_transfer_pos(&d.win, s, placed);
        }
        // 先藏再丢：表里那一条已经删了，这里的最后一个强引用一落，系统窗口就跟着没了
        let _ = d.win.hide();
    }
}

/// 用户在某个传输窗口上点「收起」/ 标题栏 ✕：把**这一台**的窗口藏起来。
///
/// 传输继续（要真停下得点窗口里的「取消」）。窗口本身不删：这台设备的下一条任务还会
/// 用它 —— 删窗口是 tick 里「这块设备这一轮没块了」那件事。
fn hide_dev_xfer_win(
    wins: &std::rc::Weak<RefCell<XferWins>>,
    pending: &Arc<Mutex<UiPending>>,
    serial: &str,
    weak: &slint::Weak<TransferWindow>,
) {
    // 窗口的回调里只拿 Weak 回指 XferWins：强引用的话「XferWins ↔ 窗口」就互相抓住了
    // （回调存在窗口自己身上），两边谁也放不掉。
    if let Some(wins) = wins.upgrade() {
        if let Ok(mut g) = wins.try_borrow_mut() {
            let single = g.map.len() == 1;
            if let Some(d) = g.map.get_mut(serial) {
                d.dismissed = true;
                // 用户可能把窗口拖到别处了：收起时顺手记住（只有单窗口才记，理由见
                // drop_dev_xfer_win）。
                //
                // **这里不能顺手存**：存盘要 State 锁（config_path / config），而这条回调
                // 可能是被 Timer 重入进来的（理由见 UiPending）。所以只把坐标丢进队列，
                // 由那条本来就持锁的 Timer 写盘。
                if single {
                    let placed = d.placed_at();
                    let pos = d.win.window().position();
                    let pos = (pos.x, pos.y);
                    if let Ok(mut q) = pending.lock() {
                        q.pos_saves.push((pos, placed));
                    }
                }
            }
        }
    }
    if let Some(w) = weak.upgrade() {
        let _ = w.hide();
    }
}

/// 把**所有**传输窗口的「取消确认」状态位清零。
///
/// 确认框上那三个按钮（保留 / 不保留 / 继续接收）是共用的：取消是进程级的一件事，
/// 用户只回答一次，所以不用管是哪台设备点的 ×，一律全清。
fn clear_cancel_ask(wins: &Rc<RefCell<XferWins>>) {
    if let Ok(g) = wins.try_borrow() {
        for d in g.map.values() {
            if d.win.get_cancel_ask() {
                d.win.set_cancel_ask(false);
            }
        }
    }
}

/// 给一台设备挑一个**不重名**的标签：默认用设备显示名，重名就依次加 " 2"、" 3"。
///
/// 为什么必须唯一：窗口标题 = 状态文案 + " · " + label，而找 HWND 是靠标题后缀匹配的
/// （tray_win::find_transfer_window_of）—— 两台手机同名就会互相抢到对方的窗口。
fn unique_label(taken: &[String], base: &str) -> String {
    let base = if base.trim().is_empty() {
        i18n::tr("手机", "phone").to_string()
    } else {
        base.to_string()
    };
    if !taken.iter().any(|t| t == &base) {
        return base;
    }
    let mut n = 2;
    loop {
        let cand = format!("{} {}", base, n);
        if !taken.iter().any(|t| t == &cand) {
            return cand;
        }
        n += 1;
    }
}

/// 屏幕可用高度（逻辑像素，0 = 没量到）。工作区高是物理像素，除以缩放才是 Slint 的
/// 逻辑像素 —— 传输窗口的高度、块高（114 / 244）都是逻辑像素，混着用会在 150% 缩放的
/// 屏幕上封错值（1920x1080@150% 会把 1032 当成 1032 逻辑像素，等于多封进去 500）。
#[cfg(windows)]
fn screen_height_logical(win: &TransferWindow) -> i32 {
    let scale = win.window().scale_factor().max(0.1);
    match tray_win::cursor_work_area_height() {
        Some(px) if px > 0 => (px as f32 / scale).round() as i32,
        _ => 0,
    }
}

/// 建一台设备的独立传输窗口，并把它的回调挂上。
///
/// 只能在 UI 线程调（Slint 对象不 Send），所以只从那个 16ms Timer 里进来。
fn make_dev_xfer_win(
    wins: &Rc<RefCell<XferWins>>,
    pending: &Arc<Mutex<UiPending>>,
    serial: &str,
    label: &str,
) -> Option<DevXferWin> {
    let win = match TransferWindow::new() {
        Ok(w) => w,
        Err(e) => {
            log::warn(&format!("建传输窗口失败（{}）：{}", label, e));
            return None;
        }
    };
    // 标题后缀要在 show 之前设好：settle() 是拿它去找 hwnd 的
    let suffix = format!(" · {}", label);
    win.set_title_suffix(suffix.as_str().into());
    // 高度封顶要用的屏幕可用高度（逻辑像素）：Slint 自己问不到屏幕尺寸，只能这边量了喂进去。
    // 量的是「鼠标所在那块屏」的工作区 —— 窗口第一次出现就贴那块屏的右下角
    // （tray_win::transfer_window_origin 用的是同一个点），而这时候 hwnd 还没建出来、
    // 量不到窗口自己在哪块屏上。量不到就给 0，Slint 那边退化到固定 720。
    #[cfg(windows)]
    win.set_screen_h(screen_height_logical(&win));
    let model = Rc::new(slint::VecModel::from(Vec::<XferOp>::new()));
    win.set_ops(slint::ModelRc::new(model.clone()));
    wire_dev_xfer_win(&win, serial, wins, pending);
    Some(DevXferWin {
        serial: serial.to_string(),
        label: label.to_string(),
        suffix,
        win,
        model,
        tracks: Vec::new(),
        waiting_sent: Vec::new(),
        #[cfg(windows)]
        tw: TwWindow::default(),
        dismissed: false,
        details: false,
        last_h: 0.0,
        last_actual_h: 0.0,
        frozen_op: None,
        slot_seen: 0,
        // 1 = 「刚建出来时默认是单窗口摆法」：真多窗口时下面的判断会立刻重排一次
        stack_seen: 1,
        shown: false,
        idle_since: None,
    })
}

/// 给一个新传输窗口挂上它那一套回调。
///
/// 以前这些回调挂在唯一那个窗口上；现在每台设备一个窗口，所以建窗口时挂一遍。
/// 回调分两类：
///   * 「这台设备自己的事」（收起 / 明细展开）→ 按串号回查 map，只改这一条；
///   * 「进程级的事」（暂停 / 取消 / 失败提示）→ 和改之前调的是同一套全局函数。
fn wire_dev_xfer_win(
    win: &TransferWindow,
    serial: &str,
    wins: &Rc<RefCell<XferWins>>,
    pending: &Arc<Mutex<UiPending>>,
) {
    // 窗口回调里一律用 Weak 回指（理由见 hide_dev_xfer_win）
    let wins_weak: std::rc::Weak<RefCell<XferWins>> = Rc::downgrade(wins);

    // × 的语义按方向分（接收 = 弹「保留 / 不保留」，发送 = 直接中断），方向由引擎的
    // control 标志给出，不靠界面自己猜 —— 所有窗口共用同一套判断。
    win.on_cancel_send(cancel_send_request);
    // 暂停位是**进程级**的（control.rs 里那个 static AtomicBool）：一个窗口按暂停，
    // 所有块都该停，所以这里没有「按窗口」的记录。
    win.on_toggle_pause(|| {
        // 没有在传的时候按它一律不生效：这两个标志是**进程级**的，空闲时按一下
        // 会把它带到下一单（下一单刚起步就是「已暂停」，用户以为卡住了）。
        // 真机上这颗按钮在收尾那一两帧里可能还看得见，所以这里必须挡一道。
        if !hfx_receiver::control::receiving() && !hfx_receiver::control::sending() {
            log::warn(i18n::tr(
                "按键无效：没有正在传输的任务（暂停）",
                "ignored: nothing is transferring (pause)",
            ));
            return;
        }
        let paused = !hfx_receiver::control::paused();
        log::warn(i18n::tr(
            if paused { "按钮：暂停" } else { "按钮：继续" },
            if paused { "button: pause" } else { "button: resume" },
        ));
        hfx_receiver::control::pause(paused);
    });
    win.on_cancel_recv(cancel_recv_request);
    win.on_cancel_dismiss(cancel_dismiss);
    {
        let q = Arc::clone(pending);
        win.on_cancel_keep(move || cancel_keep(&q));
    }
    {
        let q = Arc::clone(pending);
        win.on_cancel_discard(move || cancel_discard(&q));
    }
    // 收起（窗口里那颗按钮）：藏起来，传输继续。失败那一轮点它 = 「我知道了」——
    // 把失败提示消费掉，下一轮正常弹回来。
    {
        let key = serial.to_string();
        let wins = wins_weak.clone();
        let q = Arc::clone(pending);
        let weak = win.as_weak();
        win.on_close_window(move || {
            hfx_receiver::receiver::clear_failure();
            hide_dev_xfer_win(&wins, &q, &key, &weak);
        });
    }
    // 标题栏的 ✕ 和窗口里的「收起」是同一件事：藏起来，传输继续。
    // （不接管的话 Slint 会当成「关窗」，窗口状态和这边的 shown 就对不上了）
    {
        let key = serial.to_string();
        let wins = wins_weak.clone();
        let q = Arc::clone(pending);
        let weak = win.as_weak();
        win.window().on_close_requested(move || {
            hfx_receiver::receiver::clear_failure();
            hide_dev_xfer_win(&wins, &q, &key, &weak);
            slint::CloseRequestResponse::KeepWindowShown
        });
    }
    // 「▾ 详细信息」：只翻这个窗口的 bool，窗口高度由主循环那边跟着算
    //（收起/展开都不动窗口位置：set_size 只改大小）
    {
        let key = serial.to_string();
        let wins = wins_weak;
        let weak = win.as_weak();
        win.on_toggle_details(move || {
            let Some(w) = weak.upgrade() else { return };
            // 传完之后明细没意义（窗口正准备收起来），别让它把窗口撑大
            if w.get_done() {
                return;
            }
            let next = !w.get_details();
            w.set_details(next);
            // 高度公式读的是 map 里那一份：同一个 bool 的两面必须一起改
            if let Some(wins) = wins.upgrade() {
                if let Ok(mut g) = wins.try_borrow_mut() {
                    if let Some(d) = g.map.get_mut(&key) {
                        d.details = next;
                    }
                }
            }
        });
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
///
/// 不碰任何 Windows API（只读窗口坐标 + 写 config），所以不加 cfg —— 多窗口改造之后
/// 它是从「跟平台无关」的那几段代码里调的，加 cfg 反而要在调用处再套一层。
fn save_transfer_pos(tw: &TransferWindow, state: &mut State, placed: Option<(i32, i32)>) {
    let pos = tw.window().position();
    save_transfer_pos_at((pos.x, pos.y), state, placed);
}

/// `save_transfer_pos` 的「坐标已经在手上」版本。
///
/// 界面回调拿到坐标后不能自己存（要 State 锁，见 UiPending），只能把坐标丢进队列，
/// 由 Timer 调这一个 —— 那时候窗口可能已经藏起来了，但坐标早就量好了。
fn save_transfer_pos_at(pos: (i32, i32), state: &mut State, placed: Option<(i32, i32)>) {
    // (0,0) 一般是「拿不到位置」而不是真的拖到了左上角
    if pos.0 == 0 && pos.1 == 0 {
        return;
    }
    if let Some((px, py)) = placed {
        // 和我们摆的位置一样 → 用户没拖过，不用写盘
        if (pos.0 - px).abs() <= 4 && (pos.1 - py).abs() <= 4 {
            return;
        }
    }
    if state.config.xfer_win_pos == Some(pos) {
        return;
    }
    state.config.xfer_win_pos = Some(pos);
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
/// 和 DevXferWin::show_at 一个套路：先按估算尺寸摆个大概位置（省得在系统默认位置
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
    /// 这一轮是用户自己点了「取消发送」结束的（不是失败）。
    ///
    /// 必须和 failed 分开：取消时「已发字节」必然小于总量，用「发没发完」判结果
    /// 就会把用户主动取消说成「发送失败」（真机 2026-11-21 用户截图报的就是这个）。
    cancelled: bool,
    /// 这一轮里见过「取消已经受理」（control::cancel_requested）。
    ///
    /// 不直接在这两处各读一次全局量：收尾那一帧可能已经跨到下一轮握手，而
    /// CANCEL_REQUESTED 是会被复位的一次性闸门。见过就一直记着。
    cancel_seen: bool,
    /// 第 3 屏出现的时刻（成功 2 秒后自动关窗）
    done_at: Option<std::time::Instant>,
    /// 这一轮见过的「已发字节」最大值。
    ///
    /// 只能用它来报「发出去了多少」，不能直接读 PROGRESS：取消 / 收尾 / 重新开始
    /// 都会 reset 全局进度（session.rs 那几处 reset 和 engine 的会话收尾），
    /// reset 之后 session_bytes 立刻变 0 —— 于是传了 1.94 GB 再点取消，
    /// 小窗收尾时会写「只发出 0 B / 2.00 GB」（真机 2026-11-21 撞见）。
    /// 峰值只涨不落，取消也拿得到真实数字。
    peak_sent: u64,
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
            cancelled: false,
            cancel_seen: false,
            done_at: None,
            peak_sent: 0,
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

    /// 用户自己取消的那一轮：进第 3 屏，但**不**算失败 —— 不弹红字、不挂窗，
    /// 和「已发送」一样 2 秒后自己收掉（半截文件和「已取消」由传输小窗去说）。
    fn finish_cancelled(&mut self, text: String) {
        self.cancelled = true;
        self.finish(false, text);
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

/// 发送方向（电脑 -> 手机）的取消被点了一下。返回 false = 忽略这次点击，
/// 界面据此不弹确认框 —— 和接收方向完全对称。
fn cancel_send_request() -> bool {
    if !hfx_receiver::control::sending() {
        log::warn(i18n::tr(
            "按钮：取消发送被忽略（当前没有发送在进行）",
            "cancel sending ignored: nothing is being sent",
        ));
        return false;
    }
    if hfx_receiver::control::cancel_requested() {
        log::warn(i18n::tr(
            "按钮：取消发送被忽略（这次取消已经受理过）",
            "cancel sending ignored: this cancel was already accepted",
        ));
        return false;
    }
    log::warn(i18n::tr(
        "按钮：取消发送（弹确认框；传输先不停，等用户选完再停）",
        "button: cancel sending (dialog shown; transfer keeps running until the user chooses)",
    ));
    true
}

/// 把「保留 / 不保留」落到当前那个方向上。
///
/// 半截文件在哪一头决定了谁来处置：接收方向在电脑本地（finish_with 删），
/// 发送方向在手机上（收尾时发 CANCEL_DISCARD 让它删）。
fn apply_cancel_choice(choice: hfx_receiver::control::CancelChoice) -> bool {
    if hfx_receiver::control::sending() {
        hfx_receiver::control::cancel_send(choice)
    } else {
        hfx_receiver::control::cancel_receive(choice)
    }
}

/// 保留：收尾时留着半截文件和 .hfxpart（下次同一个文件接着传）
fn cancel_keep(pending: &Arc<Mutex<UiPending>>) {
    if !apply_cancel_choice(hfx_receiver::control::CancelChoice::Keep) {
        // 幂等：这次取消已经受理过（收尾 / 重建还没做完）时不再做任何事，
        // 上一次的结果原样复用，界面不重复提示、控制通道不插第二帧。
        log::warn(i18n::tr(
            "按钮：取消接收 · 保留进度被忽略（这次取消已经受理过）",
            "cancel · keep ignored: this cancel was already accepted",
        ));
        return;
    }
    log::warn(i18n::tr(
        "按钮：取消 · 保留进度（半截文件和 .hfxpart 留着，可续传）",
        "button: cancel · keep the partial file (resumable)",
    ));
    // 这句话由 Timer 写进 State（理由见 UiPending：回调里拿 State 锁会自死锁）
    if let Ok(mut q) = pending.lock() {
        q.cancel_note = Some(
            i18n::tr(
                "已取消：保留已收到的部分，下次同一个文件可续传",
                "cancelled: what already arrived is kept and can be resumed",
            )
            .to_string(),
        );
    }
}

/// 不保留：接收方向由 Receiver::finish_with(false) 本地删，
/// 发送方向由 CANCEL_DISCARD 让手机把它写了一半的文件和续传记录删掉
fn cancel_discard(pending: &Arc<Mutex<UiPending>>) {
    if !apply_cancel_choice(hfx_receiver::control::CancelChoice::Discard) {
        log::warn(i18n::tr(
            "按钮：取消 · 不保留被忽略（这次取消已经受理过）",
            "cancel · discard ignored: this cancel was already accepted",
        ));
        return;
    }
    // 删除是"谁那一头的事"：接收方向电脑自己删（收尾的 finish_with），
    // 发送方向要等手机收到 CANCEL_DISCARD 之后自己删 —— 提示词得对得上。
    let sending = hfx_receiver::control::sending();
    log::warn(i18n::tr(
        if sending {
            "按钮：取消 · 不保留（已通知手机删掉没传完的文件和续传记录）"
        } else {
            "按钮：取消 · 不保留（收尾删掉没传完的文件和续传记录）"
        },
        if sending {
            "button: cancel · discard (the phone was told to remove the incomplete files)"
        } else {
            "button: cancel · discard the incomplete files"
        },
    ));
    if let Ok(mut q) = pending.lock() {
        q.cancel_note = Some(
            if sending {
                i18n::tr(
                    "已取消：已通知手机删除未传完的文件和续传记录",
                    "cancelled: the phone was told to remove the incomplete files",
                )
            } else {
                i18n::tr(
                    "已取消：已删除未传完的文件和续传记录",
                    "cancelled: the incomplete files and their resume journals were removed",
                )
            }
            .to_string(),
        );
    }
}

/// 确认框里选了「继续接收」：把刚才为了弹框加的暂停放掉，传输照常
fn cancel_dismiss() {
    // 弹框时没有暂停，所以这里也不用放暂停 —— 传照常（以前这里负责把"为了弹框而加的暂停"放掉）
    log::info(i18n::tr(
        "取消确认框：选了「继续」（这一轮照常跑）",
        "cancel dialog: keep going",
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

/// 传输窗口里「一个操作块」收起时的高度（逻辑像素），和 app.slint 的 XferOperation 对得上。
///
/// 2026-11-19「对齐资源管理器」那一轮整窗只有一块、收起是 156，逐项是：
/// 14(上留白) + 20(主文案) + 6 + 30(百分比行) + 13(百分比行到条，原 12)
/// + 15(条) + 15(条到分隔线，原 0 —— 原来线贴着条子画) + 1(分隔线)
/// + 10(线到「详细信息」行，原 9) + 22(「详细信息」行，原 20) + 10(下留白)。
///
/// 现在一个窗口能堆 N 块，156 这个数就拆成了「块自己 114 + 尾部开关行 42」：
/// 单块时 114 + 42 还是 156，一个像素都没变；多块时每块各自 +114。
const TW_OP_COLLAPSED: f32 = TW_TOP_PART + TW_BAR_BLOCK + TW_DIVIDER; // = 114.0
/// 收起态里「分段进度条」那一整块：13(百分比行到条) + 15(条) + 15(条到分隔线) = 43。
/// 展开态这一块**整块不画**（app.slint 里用 if !root.details 包起来，不是 visible:false
/// —— 那还占着高度），省下的 43px 换成详情区。
const TW_BAR_BLOCK: f32 = 43.0;
/// 顶端那段和进度条无关的部分：14(上留白) + 20(主文案) + 6 + 30(百分比行) = 70。
/// 收起 / 展开两种状态都一模一样。
const TW_TOP_PART: f32 = 70.0;
/// 块末尾那条分隔线 1px。它在 app.slint 里是 XferOperation 的最后一行，兼职
/// 「块与块之间的分隔线」，所以**每块**都算 1px —— 单块时它正好是原来那条
/// 「进度区 ↔ 详细信息行」的线（展开态也一样在「名称：」上面那条）。
const TW_DIVIDER: f32 = 1.0;
/// 展开态尾部（所有块共用）：10(行区下留白) + 1(「简略信息」上面那条分隔线) + 10
/// + 22(开关行) + 10(下留白) = 53。
const TW_DETAIL_TAIL: f32 = 10.0 + 1.0 + 10.0 + 22.0 + 10.0;
/// 展开态：分段条不画之后，百分比行到分隔线仍然留的那一口气（app.slint 里 if root.details 的 13px）。
/// 不这么留，那条分隔线会直接贴到 20px 大字的下沿。
const TW_DETAIL_GAP: f32 = 13.0;
/// 展开态详情区的固定高度（和行数无关的部分）：
///   10(分隔线到「名称：」) + 18*3(名称/剩余时间/剩余项目，原生这三行行距 18)
///   + 8(三行到速度图表条) + 80(速度图表条) + 8(图表条到第一条通道速率)
const TW_DETAIL_BODY: f32 = 10.0 + 18.0 * 3.0 + 8.0 + TW_CHART_H + 8.0;
/// 展开态一块的高度（不含明细行）：70(上半) + 13(分段条不画了留的呼吸) + 1(分隔线)
/// + 160(详情固定部分，含 80 的图表条) = 244。明细行 22*rows 在
/// transfer_window_height() 里按**非等待块**汇总后另算（等待块永远不展开）。
const TW_OP_EXPANDED: f32 = TW_TOP_PART + TW_DETAIL_GAP + TW_DIVIDER + TW_DETAIL_BODY; // = 244.0
/// 展开态那条「速度图表条」的高度，80px，和 app.slint 里 chart 的 height 一模一样。
/// 参考图 native-details.png 逐行数：y=92 外框 / 93 内侧 / 94..141 上层 48 / 142 黑基线 /
/// 143..169 下层实心 27 / 170 内侧 / 171 外框 = 80 行（y=172 起是纯白）。
/// 原先这里是 15px 的「大聚合条」，用户拍板要真 1:1 的图表条，换成 80。
const TW_CHART_H: f32 = 80.0;
/// 收起态尾部（所有块共用一条开关行）：10(分隔线到行) + 22(「详细信息」行) + 10(下留白) = 42。
/// 展开态它挪到最底下、中间还多插一条分隔线，见 TW_DETAIL_TAIL。
const TW_TOGGLE_TAIL: f32 = 10.0 + 22.0 + 10.0;
/// 明细一行的高度（app.slint 里 for 循环的 height: 22px）
const TW_ROW_H: f32 = 22.0;
/// 等待块（手机队列里还没开始传的那一条）的高度。
///
/// 和精简块**同值** 114：等待块只画精简态（一行「文件名（大小）」+ 小号灰字「等待中」+
/// 空的进度条），窗口的「详细信息」展开它也不跟着展开 —— 所以它没有 244 那一档，
/// 也不会占任何明细行。app.slint 的 raw-content-h 里同样是 waiting-ops * 114。
const TW_WAITING_OP: f32 = 114.0;
/// 传输窗口高度封顶要扣掉的上下留白：封顶值 = 屏幕可用高度 - 这个数。
///
/// 160 是「标题栏 + 边框 + 上下各留一口气」的量级：窗口是贴工作区右下角的，
/// 封顶之后再靠 keep_on_screen 收一下尾，上下都还看得见窗口边缘。
/// app.slint 的 cap-h 里是同一个数（两处必须一致）。
const TW_HEIGHT_CAP_SLACK: f32 = 160.0;
/// 拿不到屏幕高度时的封顶值（退化值）。720 能装下 1366x768 笔记本的工作区（768-48=720），
/// 又不至于把一块 244 高的展开块裁掉。
const TW_HEIGHT_CAP_FALLBACK: f32 = 720.0;
/// 「接收失败：<原因>」那一行红字的高度（app.slint 里 height: 20px）。
/// 每个失败块各加一次（多块时可能只失败其中一块）。
const TW_FAIL_LINE_H: f32 = 20.0;

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

/// 流光那条亮带的几何参数（照抄 Windows 原生 WPF 默认模板，参见 app.slint 里 band 的注释）。
///
/// 以前这四样是 tick 闭包里的局部 const —— 流光跑在一个独立的 16ms Timer 上，整窗只有一块，
/// 亮带位置一个数就够了。现在一个窗口能堆 N 块，每块的亮带位置由自己那份填充比例算出来，
/// 所以搬到模块级给 OpTrack 用（数值一个都没动，BAR_PX 是「按已填充比例换算成像素」用的
/// 换算基准，改了亮带速度和位置就变）。
const FLOW_BAND_PX: f32 = 100.0; // 高光带宽（WPF 就是 100px）
const FLOW_SPEED_PX_S: f32 = 200.0; // 200 px/s（WPF 注释里的原值）
const FLOW_PAUSE_S: f32 = 1.0; // 扫完停 1 秒
const FLOW_BAR_PX: f32 = 428.0; // 条的实际像素宽：窗口 476 - 左右各 24 的内边距

/// 传输窗口里「一个操作块」的滚动状态（一条传输任务一份）。
///
/// 为什么要有这一份：速度、分段速率、曲线分桶、量程、流光相位在「窗外只有一块」的时候
/// 都是 tick 闭包里的局部变量，天然只有一份。现在一个窗口能堆 N 块，这些东西必须各算各的
/// —— 共用一份的话两块会画同一条曲线、共用同一个量程，读数互相污染（一块在跑、一块停着，
/// 量程会被跑得快的那个顶死，停着那块也照样长出新曲线）。
struct OpTrack {
    /// 这条任务读哪一份进度。目前一条任务就是「一台设备的一次传输」，所以这里就是设备串号
    /// （进度按串号分开记，见 receiver::progress_for）。
    serial: String,
    /// 速度（B/s）：0.2 秒窗口 + EMA 平滑（和主窗口那张卡同一套算法）
    speed: f64,
    last_bytes: u64,
    last_tick: std::time::Instant,
    /// 每截速率：拿「这一秒」和「上一秒」各截的已收字节做差（PROGRESS 没有速率数据源）
    seg_rate_bytes: Vec<u64>,
    seg_rate_prev: Vec<u64>,
    seg_rates: Vec<f32>,
    seg_rate_at: Option<std::time::Instant>,
    /// 展开态曲线的进度分桶（分桶的理由见 CURVE_BINS 的说明）
    curve_bins: Vec<f32>,
    curve_bin_cnt: Vec<u16>,
    curve_cur_progress: f32,
    curve_last_at: Option<std::time::Instant>,
    curve_last_progress: f32,
    curve_max_speed: f32,
    curve_peak_win: Vec<f32>,
    curve_win: Vec<(std::time::Instant, u64)>,
    curve_path_progress: f32,
    curve_path_at: Option<std::time::Instant>,
    /// 上次拼出来的 Path commands：这帧不用重建时原样写回去（字符串就不用动）
    curve_cmds: String,
    /// 黑线的显示位置：每帧朝目标值追 22%，不在 5Hz 的速率刷新上跳格子
    guide_y_disp: Option<f32>,
    /// 流光相位（秒，0..一个周期）。以前是一个独立的 16ms Timer + 一个 Cell，
    /// 现在跟着 tick 走（同一个节拍，省掉一条定时器）
    flow_phase: f32,
    /// 明细行 / 分段条的 model：内容不变就沿用旧对象（换一次 = Slint 重建全部行）
    rows_model: slint::ModelRc<XferRow>,
    rows_prev: Option<Vec<XferRow>>,
    segs_model: slint::ModelRc<XferSeg>,
    segs_prev: Option<Vec<XferSeg>>,
    /// 上次写进模型的那份数据：和这帧一样就不写（Slint 收到一样的值也会标脏、重绘整窗）
    sent: Option<XferOp>,
    /// 这一块的明细行数（0 = 块里画「这一轮还没有分通道数据」那一行，仍占 22px）
    rows_count: usize,
    /// 这一块**当前这一单**传的是哪个文件（PROGRESS 里的路径）。
    ///
    /// 队列模式下同一个窗口连着跑好几单，OpTrack 是按设备复用的：不记路径的话，
    /// 第 2 单的曲线会接着第 1 单的样子往右画、速率从上一单的末值开始衰减、
    /// 分段条也还是上一单的读数（真机 2026-11-22 队列测试里第 2 单开头就是一条
    /// 半截的旧曲线）。路径一变就说明换单了，历史清零。
    run_path: String,
}

/// tick 每帧给所有块准备的公共上下文。
///
/// 这几样都是**进程级**的东西（control.rs 里的 static AtomicBool、全局的失败快照），
/// 不分设备也不分任务 —— 每块拿到的都是同一个值，不需要按块各读一次。
struct OpCtx<'a> {
    paused: bool,
    failed: bool,
    failure_text: &'a str,
    /// 窗口里有几块：只有一块时文案和改之前一模一样，多块时第 1 行前面带上设备名
    op_count: usize,
    /// 「详细信息」展开着没有：展开态分段条整条不画，流光也不用走
    details: bool,
    /// 窗口现在可见吗：不可见就别推进流光相位（改了看不见，却让整窗重绘）
    visible: bool,
}

impl OpTrack {
    fn new(serial: &str) -> OpTrack {
        OpTrack {
            serial: serial.to_string(),
            speed: 0.0,
            last_bytes: 0,
            last_tick: std::time::Instant::now(),
            seg_rate_bytes: Vec::new(),
            seg_rate_prev: Vec::new(),
            seg_rates: Vec::new(),
            seg_rate_at: None,
            curve_bins: vec![0.0; CURVE_BINS],
            curve_bin_cnt: vec![0; CURVE_BINS],
            curve_cur_progress: 0.0,
            curve_last_at: None,
            curve_last_progress: 0.0,
            curve_max_speed: 0.0,
            curve_peak_win: Vec::new(),
            curve_win: Vec::new(),
            curve_path_progress: -1.0,
            curve_path_at: None,
            curve_cmds: String::new(),
            guide_y_disp: None,
            flow_phase: 0.0,
            rows_model: slint::ModelRc::new(slint::VecModel::from(Vec::<XferRow>::new())),
            rows_prev: None,
            segs_model: slint::ModelRc::new(slint::VecModel::from(Vec::<XferSeg>::new())),
            segs_prev: None,
            sent: None,
            rows_count: 0,
            run_path: String::new(),
        }
    }

    /// 换了一单（PROGRESS 里的路径变了）：把上一单留在身上的历史清零。
    ///
    /// 归零的是**一条任务内部的累计量**（曲线分桶、每截速率、平滑过的速率、
    /// 明细行缓存）；不动的只有 Slint 那几个 model 对象（换对象 = 整块重建，见
    /// 「只建一次」那条注释）和 sent（下一帧的 op 本来就全变了）。
    fn begin_run(&mut self, path: &str) {
        self.run_path = path.to_string();
        self.speed = 0.0;
        self.last_bytes = 0;
        self.last_tick = std::time::Instant::now();
        self.seg_rate_bytes.clear();
        self.seg_rate_prev.clear();
        self.seg_rates.clear();
        self.seg_rate_at = None;
        self.curve_bins = vec![0.0; CURVE_BINS];
        self.curve_bin_cnt = vec![0; CURVE_BINS];
        self.curve_cur_progress = 0.0;
        self.curve_last_at = None;
        self.curve_last_progress = 0.0;
        self.curve_max_speed = 0.0;
        self.curve_peak_win.clear();
        self.curve_win.clear();
        self.curve_path_progress = -1.0;
        self.curve_path_at = None;
        self.curve_cmds.clear();
        self.guide_y_disp = None;
        self.rows_prev = None;
        self.segs_prev = None;
        self.rows_count = 0;
    }

    /// 算出这一帧这个块该显示什么（顺手把 model / Path 换掉）。
    ///
    /// 这里整段是原来「窗外只有一块」时那套逻辑的搬运：除了把 tw.set_xxx 换成写进 XferOp、
    /// 把闭包里的局部变量换成 self 的字段，算法和数值一个都没动 —— 单块时的界面数字
    /// 必须和改之前完全一样。
    fn frame(&mut self, ctx: &OpCtx, device_name: &str) -> XferOp {
        // 读哪一份进度：接收方向是按设备分开记的（receiver::progress_for，收块时
        // 「本设备」和「全局」两份都写，见 receiver.rs 的进度上报）。
        // 发送方向（电脑 -> 手机：手机来取文件 / 用户推送）目前只有全局那份在更新 ——
        // session.rs 的发送循环只写 PROGRESS，要改成按设备写属于会话层，这一轮不动它。
        // 所以推文件时退回全局那份：单块时显示的数字和改之前完全一样；两台设备同时推
        // 会看到同一组数字（那个方向本来也只有一个计数）。
        let dev_prog;
        let prog: &hfx_receiver::receiver::ProgressState = if hfx_receiver::control::sending() {
            &hfx_receiver::receiver::PROGRESS
        } else {
            dev_prog = hfx_receiver::receiver::progress_for(&self.serial);
            &dev_prog
        };
        let (path, done, total_bytes, _session_bytes, _files) = prog.snapshot();
        // 换文件 = 换了一单（队列模式下同一个窗口连着跑好几单）：先把上一单留在
        // 这里的曲线 / 速率 / 分段历史清掉，再往下算这一单的数字。
        // 路径还是空的那几帧不算换单 —— 那一轮还没解析出文件呢。
        if !path.is_empty() && path != self.run_path {
            self.begin_run(&path);
        }

        // 速率只按「这次真收到的字节」算：续传基线（磁盘上本来就有的一块）不算进去，
        // 否则续传一开始会跳出一个几 GB/s 的假峰值。
        let received = prog.received_bytes();
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(self.last_tick).as_secs_f64();
        // 0.2 秒一个窗口 = 每秒更新 5 次。以前是 0.4 秒（2.5 次/秒），
        // 黑线和「速度:」那行字看着就像一秒跳两下的电子钟。
        if elapsed >= 0.2 {
            let delta = received.saturating_sub(self.last_bytes);
            let instant = delta as f64 / elapsed;
            // 暂停时**冻结**速率，不跟着衰减：暂停期间一个字节都不会来，
            // 照着算 delta=0 会让速率每 0.4 秒掉 40%，几秒后就从 9.6 MB/s 掉成
            // 一串无意义的数字。原生暂停是把图表整块冻住，数字停在最后那个值上。
            if !ctx.paused {
                // 0.55 的惯性 + 0.45 的新值：窗口短了噪声会变大，靠这个压住。
                self.speed = if self.speed <= 0.0 {
                    instant
                } else {
                    self.speed * 0.55 + instant * 0.45
                };
            }
            self.last_bytes = received;
            self.last_tick = now;
        }
        let speed = self.speed;

        // 传完了没有：真机第一次跑出来的是「已完成 100%」+ 0 B/s 一直挂着不消失 ——
        // 因为 PROGRESS 不清零，所以「结束」得单独认：字节满了就算完；另外真机第二次
        // 撞见过 done 停在 599.0/600.0 MB（块计数尾巴对不上），再认一个信号：
        // control 那边 receiving()/sending() 都落下去，就是这一轮结束了。
        // 「这一轮是不是我在跑」必须按设备算，不能用进程级的两个标志直接当答案：
        //
        //   · 接收方向：receiving() 是**进程级**的（谁在发都置位）。两台手机都连着、
        //     只让小米发一份文件，红米那个窗口也跟着收到 true，于是它永远「没传完」，
        //     窗口当场弹出来显示「正在传输 0 B / 0%」并一直挂着 —— 真机上就是这么
        //     多出一个空窗口的。所以这里加一道「我这份进度里到底有没有东西」：
        //     路径 / 总字节 / 已收字节，一个都没有就说明这一轮跟我无关。
        //   · 发送方向：进度全局只有一份，靠 control 记下的目标串号认人。
        let involved = if hfx_receiver::control::sending() {
            hfx_receiver::control::send_target_is(&self.serial)
        } else {
            true
        };
        let has_own_data = !path.is_empty() || total_bytes > 0 || received > 0;
        let in_flight = involved
            && has_own_data
            && (hfx_receiver::control::receiving() || hfx_receiver::control::sending());
        // 数据已经全到了就算「传完」，哪怕这一刻还在等对端结论帧（receiving 仍是 true）：
        // 否则暂停 / 取消落在「最后一块之后、收尾之前」这个窗口里时，界面会一直停在
        // 「100% · 已暂停」，用户以为没传完（真机 2026-11-22 报的就是这个）。
        // 字节没满的那些块仍然按 in_flight 判断，正常收尾不受影响。
        let bytes_full = total_bytes > 0 && done >= total_bytes;
        let finished = bytes_full || !in_flight;

        let short = path
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(&path)
            .to_string();
        let size_text = human(total_bytes);

        // ---- 第 1 行 ----
        // 单块时就是改之前那句「文件名（大小）」；多块时前面加设备名 —— 否则两块内容
        // 长得一样，用户分不出哪一块是哪台手机 / 哪条任务。
        let action_now = if ctx.failed {
            i18n::tr("接收失败", "receiving failed").to_string()
        } else if ctx.op_count > 1 {
            format!("{}：{}（{}）", device_name, short, size_text)
        } else {
            format!("{}（{}）", short, size_text)
        };

        // 百分比往下取整：向上四舍五入会在「1.99 / 2.00 GB」就跳成 100%，
        // 而这时其实还在传。没传完最多显示 99%，满字节了才给 100%。
        let pct = if total_bytes > 0 {
            ((done.min(total_bytes) as f64 / total_bytes as f64) * 100.0).floor() as i32
        } else {
            0
        };
        // 失败的时候按块算：真机那次字节数明明满了、却缺 2 块，报「未完成 100%」是
        // 自相矛盾的，按块算出来是 99%。用 set_failure 那一刻拍下来的百分比。
        let shown_pct = if ctx.failed {
            hfx_receiver::receiver::failure_percent().unwrap_or(99)
        } else if finished {
            100
        } else {
            pct.min(99)
        };

        // 暂停时速率位置显示「已暂停」（和手机端弹窗一致）
        let speed_text = if ctx.paused {
            "已暂停".to_string()
        } else {
            human_speed(speed)
        };
        let detail_now = format!("{} / {} · {}", human(done), human(total_bytes), speed_text);

        // ---- 展开「详细信息」里那三行「标签：值」的值（标签在 Slint 那边写死）----
        // 剩余时间：算得出来给「x 分 y 秒」，算不出来给「正在计算...」（原生的写法就是这个）。
        // 刚起步（速率还没稳定）、暂停、已经收满都算「算不出来」。
        let remain_bytes = total_bytes.saturating_sub(done);
        let eta = if !ctx.paused && !finished && speed > 0.5 && remain_bytes > 0 {
            human_eta((remain_bytes as f64 / speed) as u64)
        } else {
            i18n::tr("正在计算...", "calculating...").to_string()
        };
        // 剩余项目：「项目数 (剩余大小)」。这一轮一次只有一个文件在收（PROGRESS 只记
        // 「当前文件」），所以项目数是 0 / 1，括号里是**这个文件**还剩多少字节。
        let remain_now = format!(
            "{} ({})",
            if remain_bytes > 0 { 1 } else { 0 },
            human(remain_bytes)
        );

        // IDM 式分块：按「哪条通道负责哪一段」画。段边界 = 相邻块通道归属的变化处。
        let mut raw: Vec<hfx_receiver::receiver::XferSeg> = prog.segments();
        let seg_total: f32 = raw.iter().map(|s| s.span).sum::<f32>().max(1.0);
        for s in raw.iter_mut() {
            s.span = (s.span / seg_total).clamp(0.0, 1.0);
            s.filled = s.filled.clamp(0.0, 1.0);
        }
        // 已填充比例（按块算）：进度条画到哪、流光在哪儿扫都看它
        let fill_frac: f32 = raw.iter().map(|s| s.span * s.filled).sum::<f32>().clamp(0.0, 1.0);

        // ---- 「详细信息」里的明细行：一截一行 ----
        let mut rows: Vec<XferRow> = Vec::with_capacity(raw.len());
        // 行名先不带序号、行的排序键先攒着：手机端那张表是**按通道号**排的
        // （1 USB_ADB、2 USB_ADB#2、3 wlan1 …），电脑这边段是按文件位置来的，
        // 直接照段序排行两边就对不上。排完再统一编号。
        let mut labels: Vec<String> = Vec::with_capacity(raw.len());
        let mut row_order: Vec<(u8, f32)> = Vec::with_capacity(raw.len());
        self.seg_rate_bytes.clear();
        // 暂停中：速率列统一写 0 KB/s（这一秒本来就没在传，挂个旧速率是骗人）
        for (i, s) in raw.iter().enumerate() {
            let seg_total_b = (total_bytes as f64 * s.span as f64) as u64;
            let seg_done = (seg_total_b as f64 * s.filled as f64) as u64;
            self.seg_rate_bytes.push(seg_done);
            let rate = self.seg_rates.get(i).copied().unwrap_or(0.0);
            // 「各通道速率」这一列的说法（照原生的意思来）：
            //   这一截收满 → 「已完成」；暂停中 / 一秒没动静 → 「0 KB/s」；
            //   其余按真实速率（human_speed 自带 KB/s、MB/s 的量级切换）。
            let rate_text = if s.filled >= 0.999 {
                i18n::tr("已完成", "done").to_string()
            } else if ctx.paused || rate <= 0.05 {
                "0 KB/s".to_string()
            } else {
                human_speed(rate as f64)
            };
            // 段名 = 「谁在负责」，和手机端那条明细（「1 USB_ADB」）同一个读法（序号后加）。
            let label = if s.owner == u8::MAX {
                "未分配".to_string()
            } else if s.owner == hfx_receiver::receiver::OWNER_RESUMED {
                "续传".to_string()
            } else {
                match prog.channel_name(s.owner) {
                    Some(n) => n,
                    None => {
                        if std::env::var("HFX_DEBUG_NAME").is_ok() {
                            log::info(&format!(
                                "通道名缺失 owner={} 已登记={}",
                                s.owner,
                                prog.channel_count()
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
                amount: format!("{} / {}", human(seg_done), human(seg_total_b)).into(),
                rate: rate_text.into(),
                percent: (s.filled * 100.0).round() as i32,
            });
        }
        // 按通道号重排（和手机端那张表同序），再统一编号；「未分配 / 多通道合并」的段排最后。
        {
            let mut order: Vec<usize> = (0..rows.len()).collect();
            order.sort_by(|&a, &b| {
                let (oa, xa) = row_order[a];
                let (ob, xb) = row_order[b];
                let ka = if oa == u8::MAX { (1u8, u8::MAX) } else { (0u8, oa) };
                let kb = if ob == u8::MAX { (1u8, u8::MAX) } else { (0u8, ob) };
                ka.cmp(&kb)
                    .then(xa.partial_cmp(&xb).unwrap_or(std::cmp::Ordering::Equal))
            });
            let mut sorted: Vec<XferRow> = Vec::with_capacity(rows.len());
            for (new_i, &old_i) in order.iter().enumerate() {
                let mut r = rows[old_i].clone();
                r.name = format!("{}. {}", new_i + 1, labels[old_i]).into();
                sorted.push(r);
            }
            rows = sorted;
        }
        // 每截速率的取样：约一秒一次。逐帧做差会把块到达的抖动放大成满屏跳动的数字。
        {
            let now_at = std::time::Instant::now();
            let due = match self.seg_rate_at {
                Some(t) => now_at.duration_since(t).as_secs_f32() >= 0.9,
                None => true,
            };
            if due {
                if let Some(t) = self.seg_rate_at {
                    let dt = now_at.duration_since(t).as_secs_f32().max(0.2);
                    let new_rates: Vec<f32> = self
                        .seg_rate_bytes
                        .iter()
                        .enumerate()
                        .map(|(i, &b)| {
                            let prev = self.seg_rate_prev.get(i).copied().unwrap_or(b);
                            if b >= prev {
                                (b - prev) as f32 / dt
                            } else {
                                0.0
                            }
                        })
                        .collect();
                    self.seg_rates = new_rates;
                }
                self.seg_rate_prev = self.seg_rate_bytes.clone();
                self.seg_rate_at = Some(now_at);
            }
        }
        // 进度条上的切分线：段是「相邻块换了通道」留下的痕迹，位置就是它占整条的比例。
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
        // 每拍新建一个 VecModel 塞给 Slint，等于告诉它「整个列表全变了」：它会把这 10 行
        // 全部拆掉重建（每行里还有 10 来个 Text/Rectangle），光这一步就吃掉整帧预算的大头。
        // 所以先和自己的上一帧比，内容一样就沿用旧 model，Slint 那边什么都不会动。
        if self.segs_prev.as_ref().map(|p| p.as_slice()) != Some(segs.as_slice()) {
            self.segs_prev = Some(segs.clone());
            self.segs_model = slint::ModelRc::new(slint::VecModel::from(segs));
        }
        if self.rows_prev.as_ref().map(|p| p.as_slice()) != Some(rows.as_slice()) {
            self.rows_prev = Some(rows.clone());
            self.rows_model = slint::ModelRc::new(slint::VecModel::from(rows));
        }
        self.rows_count = self.rows_prev.as_ref().map(|r| r.len()).unwrap_or(0);

        // ---- 展开态图表条：速度曲线采样 + 拼 Path + 「速度:」那行 ----
        // 横轴是**进度**：把 0..1 切成 CURVE_BINS 格，每格记那一刻的速率。写进格子的值
        // 就不再改 —— 曲线只向右长，画过的部分一个像素都不动。暂停 / 失败时不写。
        //
        // 回退检测要**狠一点**：进度是从 PROGRESS 里几千个块的计数算出来的，
        // 个别块先完成后重置，会让 fill_frac 瞬间回弹几个百分点。真正的新一轮必然是
        // 「回到接近 0」，所以要求回退超过 25% 且新进度 < 10%。
        if self.curve_last_progress - fill_frac > 0.25 && fill_frac < 0.10 {
            self.curve_bins.iter_mut().for_each(|b| *b = 0.0);
            self.curve_bin_cnt.iter_mut().for_each(|c| *c = 0);
            self.curve_cur_progress = 0.0;
            self.curve_last_at = None;
            self.curve_max_speed = 0.0;
            self.curve_peak_win.clear();
            self.curve_win.clear();
        }
        self.curve_last_progress = fill_frac;
        // ---- 曲线专用速率：300ms 滑窗 ----
        // 不能拿上面那个 speed 画曲线：那个值（0.2 秒窗口 + EMA）被压得几乎不动，
        // 画出来就是一条平线。曲线单独按 40ms 采样间隔算一次瞬时值，两套互不干扰。
        // 也不用「相邻两次采样之差」：PROGRESS 的字节数按块累加，一块收完才跳一次，
        // 40ms 窗口里会读到「0、0、42761232、21MB」这种突发序列，尖峰能把量程顶飞。
        let curve_now = std::time::Instant::now();
        self.curve_win.push((curve_now, received));
        while self
            .curve_win
            .first()
            .map(|(t, _)| curve_now.duration_since(*t).as_secs_f32() > 2.0)
            .unwrap_or(false)
        {
            self.curve_win.remove(0);
        }
        // 取「至少 300ms 前」的那个点当锚：窗口还不够长时用最老的点，
        // 这样刚起步时也有一个（偏小的）速率可用，而不是一路 0。
        let anchor = self
            .curve_win
            .iter()
            .rev()
            .find(|(t, _)| curve_now.duration_since(*t).as_millis() >= 300)
            .copied()
            .unwrap_or_else(|| *self.curve_win.first().unwrap_or(&(curve_now, received)));
        let curve_elapsed = curve_now.duration_since(anchor.0).as_secs_f64();
        let curve_instant = if curve_elapsed > 0.05 {
            (received.saturating_sub(anchor.1) as f64 / curve_elapsed) as f32
        } else {
            0.0
        };
        // ---- 量程：最近 5 秒的 90 分位 × 1.1 ----
        // 用**分位数**而不是最大值：偶发的突发（实测 66.8 MB/s，正常 21）会把「最大值」
        // 顶到天上，量程跟着涨 3 倍，整条曲线被压成贴底。
        self.curve_peak_win.push(curve_instant);
        if self.curve_peak_win.len() > 125 {
            self.curve_peak_win.remove(0);
        }
        let peak_now = {
            let mut v: Vec<f32> = self.curve_peak_win.iter().copied().collect();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let idx = ((v.len() as f32) * 0.9) as usize;
            v.get(idx.min(v.len().saturating_sub(1))).copied().unwrap_or(0.0)
        };
        let want_max = chart_max_speed(peak_now);
        if want_max > self.curve_max_speed {
            // 涨：立刻跟上（不然曲线会冲出量程被削平）
            self.curve_max_speed = want_max;
        } else if self.curve_max_speed > want_max * 1.15 {
            // 跌：只在水花明显偏大时收敛，每拍 0.2%
            self.curve_max_speed = (self.curve_max_speed
                - (self.curve_max_speed - want_max) * 0.002)
                .max(want_max);
        }
        let curve_due = self
            .curve_last_at
            .map(|t| curve_now.duration_since(t).as_millis() >= CURVE_SAMPLE_MS)
            .unwrap_or(true);
        if curve_due && !ctx.paused && !ctx.failed {
            self.curve_last_at = Some(curve_now);
            // 从上次写到的位置一直填到当前进度：中间的格子沿用这一拍的速率。
            // （不填的话进度一快，格子之间会留下 0 值，曲线变成一排向下的尖刺。）
            let upto = if finished || fill_frac >= 0.999 {
                1.0f32
            } else {
                fill_frac.clamp(0.0, 1.0)
            };
            let v = curve_instant.max(0.0);
            let from = self.curve_cur_progress;
            // 首帧（from==0）如果速率还没算出来，**先别填**：传输窗口是在进度非零时
            // 才出现的，首帧要从 0 一口气补几百格，可那一拍的速率还是 0，于是整段被 0
            // 填死，曲线左边贴底。速率一上来（>1KB/s）再补，那一次才是有意义的波形。
            let ready_to_fill = from > 0.0 || v >= 1024.0;
            if std::env::var("HFX_DEBUG_CURVE").is_ok() {
                log::info(&format!(
                    "采样 fill={:.4} v={:.0} 量程={:.0} 峰值={:.0} from={:.4} upto={:.4}",
                    fill_frac, v, self.curve_max_speed, peak_now, from, upto
                ));
            }
            if upto > from && ready_to_fill {
                // 首次填桶必须从 0 开始：那一刻 fill_frac 可能已经 15%，
                // 直接按 floor(0.15*400)=58 当起点时 bins[0..57] 就永远是 0。
                let i0 = if from <= 0.0 {
                    0usize
                } else {
                    (from * CURVE_BINS as f32).floor() as usize
                };
                let i1 = ((upto * CURVE_BINS as f32).ceil() as usize).min(CURVE_BINS);
                for i in i0..i1 {
                    if let Some(b) = self.curve_bins.get_mut(i) {
                        // 加权平均：同一个桶被多次采样时取均值（覆盖会让相邻桶跳成锯齿）
                        let c = self.curve_bin_cnt.get(i).copied().unwrap_or(0) as f32;
                        *b = (*b * c + v) / (c + 1.0);
                        if let Some(cnt) = self.curve_bin_cnt.get_mut(i) {
                            *cnt = cnt.saturating_add(1);
                        }
                    }
                }
                self.curve_cur_progress = upto;
            }
        }
        // ---- 曲线 Path 降频重建 ----
        // Path 每次换 commands 字符串，Slint 都要把 200 个点重新三角化 + 重绘，
        // 这是整帧里最贵的一步。曲线右边界每涨 0.2%（600MB 文件约 1.2MB）才需要重画一次，
        // 且最少间隔 150ms —— 视觉上完全看不出延迟，帧率却能提上去。
        let path_due = self
            .curve_path_at
            .map(|t| t.elapsed().as_millis() >= 150)
            .unwrap_or(true)
            && (self.curve_cur_progress - self.curve_path_progress).abs() >= 0.002;
        if path_due {
            self.curve_path_at = Some(std::time::Instant::now());
            self.curve_path_progress = self.curve_cur_progress;
            self.curve_cmds =
                build_curve_path(&self.curve_bins, self.curve_cur_progress, self.curve_max_speed);
        }
        // 黑线：当前速度在刻度上的位置。暂停时钉在速度 0（贴底），
        // 和「速度: 已暂停」那行字一致。
        let guide_y = chart_guide_y(
            if ctx.paused { 0.0 } else { speed as f32 },
            self.curve_max_speed,
        );
        // 黑线：目标值是 0.1px 精度，但**每帧插值**逼近，所以线是连续滑动的。
        let disp = match self.guide_y_disp {
            // 第一帧 / 新一轮：直接落位，别从屏外滑进来
            None => guide_y,
            Some(prev) => prev + (guide_y - prev) * 0.22,
        };
        self.guide_y_disp = Some(disp);
        // 量化到整数像素再写属性：黑线是 1px 高的横线，亚像素位置没有意义（原生就是整数
        // 像素）。而「速度: xx MB/秒」那行字的 y 绑在 guide-y 上，属性一变它就重排一次，
        // 中文重排很贵 —— 60Hz 下每帧都重排，实测展开态吃 74% 单核。
        let quantized = disp.round();
        // 参考图右上那行是「速度: 72.9 MB/秒」——单位沿用 human_speed 的量级切换
        // （MB → KB → B），只把 "/s" 写成 "/秒"。暂停时写「已暂停」，别摆一个旧速率
        // 和上面那行「已暂停」大字自相矛盾。
        let chart_speed = if ctx.paused {
            i18n::tr("已暂停", "paused").to_string()
        } else {
            human_speed(speed).replace("/s", "/秒")
        };
        let speed_line = format!("{}: {}", i18n::tr("速度", "Speed"), chart_speed);

        // ---- 流光位置 ----
        // 公式照抄 Windows 原生（WPF 默认模板的 PART_GlowRect），一句注释见 app.slint 里 band：
        // 带宽 100px，匀速 200px/s 从填充区左边外面扫到右边外面，扫完停 1 秒再从头来。
        // 以前这段跑在一个独立的 16ms Timer 上：整窗只有一块，亮带位置一个数就够了。
        // 现在每块的填充宽度不同、亮带位置也就各不同，并进 tick 一块算（同一个节拍）。
        // 只在「真在传 + 收起态 + 窗口可见」时推进：展开态分段条整条隐藏、窗口藏起来时
        // 反正看不见，改了只会让 Slint 白重绘整窗。
        let flow_on = !finished && !ctx.paused && speed > 0.5;
        let mut flow = 0.0f32;
        if flow_on {
            let fill_px = (FLOW_BAR_PX * fill_frac.clamp(0.0, 1.0)).max(1.0);
            // WPF：从 -带宽 扫到 填充宽 + 带宽 → 距离 = 填充宽 + 2×带宽
            let travel_px = fill_px + 2.0 * FLOW_BAND_PX;
            let sweep_s = travel_px / FLOW_SPEED_PX_S;
            let cycle_s = sweep_s + FLOW_PAUSE_S;
            if ctx.visible && !ctx.details {
                self.flow_phase += 0.016;
                if self.flow_phase >= cycle_s {
                    self.flow_phase -= cycle_s;
                }
            }
            // 扫描阶段线性走完 travel，剩下的时间（1 秒）停在最右边（已经在填充区外）
            let t = (self.flow_phase / sweep_s).clamp(0.0, 1.0);
            let x_px = -FLOW_BAND_PX + travel_px * t; // 亮带左边缘
            flow = (x_px + FLOW_BAND_PX / 2.0) / FLOW_BAR_PX;
        }
        // fill 是进度条长度：量化到 0.1% 再比，避免 60Hz 下为了 0.02px 重绘整窗
        let fill_q = (fill_frac * 1000.0).round() / 1000.0;

        let op = XferOp {
            action_text: action_now.into(),
            percent: shown_pct,
            detail_text: detail_now.into(),
            file_name: short.into(),
            eta_text: eta.into(),
            remain_text: remain_now.into(),
            speed_text: speed_line.into(),
            paused: ctx.paused,
            done: finished,
            // 正常块永远不是「等待中」：等待块由 waiting_op() 单独造（见窗口那一层）
            waiting: false,
            failed: ctx.failed,
            fail_text: ctx.failure_text.into(),
            fill: fill_q,
            flow,
            flow_on,
            segs: self.segs_model.clone(),
            curve_path: self.curve_cmds.clone().into(),
            guide_y: quantized,
            rows: self.rows_model.clone(),
        };
        op
    }
}

/// 造一块「等待中」：手机队列里排着、还没开始传的那一条（QUEUE_STATE 的 state == 0）。
///
/// 字段和正常块用同一套（结构一样，只是等待块只画精简态），waiting 位是唯一的分叉 ——
/// app.slint 那边据此把百分比那行换成小号灰字「等待中」、进度条留空、暂停 / 取消两颗
/// 按钮藏掉，并且**永远不展开详情**（所以它的高度固定 114，和 details 无关）。
/// 明细行和通道段都给空：排队中的文件电脑端既没开始收、也没分到任何通道 ——
/// 反正等待块一行明细都不画。
fn waiting_op(device_name: &str, op_count: usize, item: &hfx_receiver::queue::QueueItem) -> XferOp {
    // 手机还在「准备中」（还没解析出文件）时名字是空串、字节数是 0：这时候别在界面上
    // 留一个空括号，直接说「准备中…」—— 它也确实是排队中的一种。
    let short = if item.name.is_empty() {
        i18n::tr("准备中…", "preparing…").to_string()
    } else {
        item.name
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(&item.name)
            .to_string()
    };
    let size_text = human(item.size);
    // 大小还没解析出来就别写「（0 B）」：那看着像这个文件是空的
    let head = if item.size > 0 {
        format!("{}（{}）", short, size_text)
    } else {
        short.clone()
    };
    // 第 1 行和正常块同一套写法：多块时前面带设备名（否则两块内容一样，分不出是谁的），
    // 只有一块时就是「文件名（大小）」
    let action_text = if op_count > 1 {
        format!("{}：{}", device_name, head)
    } else {
        head
    };
    XferOp {
        action_text: action_text.into(),
        // 这个数在 waiting 时不会画出来（那一行换成「等待中」了），给 0 只是占位
        percent: 0,
        // 第 2 行右边那行灰字留空：排队中没有任何速率 / 进度可报
        detail_text: "".into(),
        file_name: short.into(),
        eta_text: i18n::tr("正在计算...", "calculating...").into(),
        // 剩余项目：这个文件整个还没开始，1 项、剩的就是它自己的大小
        remain_text: format!("1 ({})", size_text).into(),
        speed_text: "".into(),
        paused: false,
        done: false,
        waiting: true,
        failed: false,
        fail_text: "".into(),
        // 进度条留空：没有段、没有填充、流光不走
        fill: 0.0,
        flow: 0.0,
        flow_on: false,
        segs: slint::ModelRc::new(slint::VecModel::from(Vec::<XferSeg>::new())),
        curve_path: "".into(),
        guide_y: 75.0,
        rows: slint::ModelRc::new(slint::VecModel::from(Vec::<XferRow>::new())),
    }
}

/// 传输窗口这一轮该多高（**未封顶**的完整内容高度）。
///
/// 逐项和 app.slint 的 raw-content-h 一个字一个字对得上：
///   高度 = 非等待块数 × (收起 114 / 展开 244)
///          + 明细行 22×detail_rows（**只在展开态**，且**只算非等待块**）
///          + 等待块数 × 114（details 开不开都一样）
///          + 失败块 20×failed_ops + 尾部（收起 42 / 展开 53）
///
/// 单块时就是改之前那两个数：
///   收起 114 + 42 = 156 ✓
///   展开 244 + 22*rows + 53 = 297 + 22*rows ✓（原来那条分隔线算在头部，这里算在块里）
///   「接收失败：<原因>」那行红字每块 +20 ✓
///
/// detail_rows 由调用方汇总**非等待块**的明细行，每块至少按 1 行算 —— rows 为 0 时
/// app.slint 画的是「这一轮还没有分通道数据」那一行，同样 22px。
/// 一个块都没有（窗口本来就该收起的那些帧）时非等待块数按 1 算，和 app.slint 里
/// Math.max(1, ops.length - waiting-ops) 那个兜底一致。
///
/// 手机队列里「等待中」的块（waiting_ops）单独一项、固定 114：它们只画精简态
/// （见 app.slint 里 XferOperation 的 slim），窗口展开也不跟，所以不计明细行。
/// 弹「取消」确认框时不用再撑高（它是独立窗口）。
fn transfer_window_height(
    details: bool,
    op_count: usize,
    waiting_ops: usize,
    detail_rows: usize,
    failed_ops: usize,
) -> f32 {
    let per_op = if details { TW_OP_EXPANDED } else { TW_OP_COLLAPSED };
    let tail = if details { TW_DETAIL_TAIL } else { TW_TOGGLE_TAIL };
    let normal = op_count.saturating_sub(waiting_ops).max(1) as f32;
    // 明细行只在展开态算：收起态块里根本不画那些行（app.slint 里 rows 的 for 循环
    // 包在 if root.details 里），算进去就是窗口底下多一块白。
    let rows_h = if details {
        detail_rows as f32 * TW_ROW_H
    } else {
        0.0
    };
    normal * per_op
        + rows_h
        + waiting_ops as f32 * TW_WAITING_OP
        + failed_ops as f32 * TW_FAIL_LINE_H
        + tail
}

/// 传输窗口高度的封顶值：屏幕可用高度 - 160，拿不到屏幕高度就退化成 720。
///
/// 和 app.slint 的 cap-h 是同一个算式（那边：root.screen-h > 0 ? screen-h-160 : 720）。
/// screen_h 传 0 = 没拿到（Rust 那边量工作区失败时的约定）。
fn transfer_window_cap(screen_h: i32) -> f32 {
    if screen_h > 0 {
        screen_h as f32 - TW_HEIGHT_CAP_SLACK
    } else {
        TW_HEIGHT_CAP_FALLBACK
    }
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
    // 每台设备一个独立传输窗口（键 = 设备串号）：只读 PROGRESS，不参与主界面布局。
    // 传输一开始自己弹出来，结束自己收起 —— 主窗口最小化到托盘时也能看到进度。
    //
    // **Slint 对象不能跨线程**：窗口只在这个 UI 线程上建 / 驱动，所以整套东西塞在
    // Rc<RefCell<..>> 里，由那个 16ms Timer（同一个 UI 线程）负责建、刷、收。
    // 它不 Send，也就不可能被别的线程碰到。
    let xfer_wins: Rc<RefCell<XferWins>> = Rc::new(RefCell::new(XferWins::default()));
    // 界面回调要交给主循环去做的脏活（记窗口坐标 / 写取消提示）。
    // 回调里**不拿 State 锁**的原因见 UiPending 的注释（关窗卡死就是这么来的）。
    let ui_pending: Arc<Mutex<UiPending>> = Arc::new(Mutex::new(UiPending::default()));
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
        let wins_start = Rc::clone(&xfer_wins);
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
            // 上一轮可能还挂着的「取消确认」状态位，别带进这一次（所有窗口一起清）
            clear_cancel_ask(&wins_start);
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
        let wins_stop = Rc::clone(&xfer_wins);
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
            clear_cancel_ask(&wins_stop);
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
            // 同传输窗口那颗：空闲时不改进程级标志，免得把「暂停」带进下一单
            if !hfx_receiver::control::receiving() && !hfx_receiver::control::sending() {
                log::warn(i18n::tr(
                    "按键无效：没有正在传输的任务（暂停）",
                    "ignored: nothing is transferring (pause)",
                ));
                return;
            }
            let paused = !hfx_receiver::control::paused();
            hfx_receiver::control::pause(paused);
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_send_paused(paused);
            }
        });
    }
    // 主窗口上那个 `cancel-send` 回调没有任何地方 invoke（发送方向的取消都在
    // 传输窗口 / 推送小窗上），所以这里不再注册——留着只会让人以为还有第三条路。
    //
    // 传输窗口上那套回调（× 的两个方向 / 暂停 / 收起 / 标题栏 ✕ / 详细信息）现在跟着
    // 窗口走：每台设备建窗口时由 wire_dev_xfer_win 挂一遍（见那个函数的说明）。
    // 以前只有一个窗口，所以只在启动时挂一次；现在窗口数是运行中才定下来的。
    // ---- 接收方向的「取消」（不只是暂停 / 继续）----
    // 主界面和独立传输窗口注册的是同一套函数（见上面的 cancel_recv_request / cancel_keep /
    // cancel_discard / cancel_dismiss），语义只有一份 —— 弹窗上的 ✕ 和主界面上的「取消」
    // 完全等价：先暂停（手机端跟着停发），再问「保留进度 / 不保留，删掉 / 继续接收」。
    ui.on_cancel_recv(cancel_recv_request);
    {
        let q = Arc::clone(&ui_pending);
        ui.on_cancel_keep(move || cancel_keep(&q));
    }
    {
        let q = Arc::clone(&ui_pending);
        ui.on_cancel_discard(move || cancel_discard(&q));
    }
    ui.on_cancel_dismiss(cancel_dismiss);
    // 独立窗口上的三个按钮：先收窗口、把传输窗口的状态位清零（所有窗口一起清：取消是
    // 进程级的一件事，用户的回答只有一个），再走同一套语义
    {
        let q = Arc::clone(&ui_pending);
        let wins = Rc::clone(&xfer_wins);
        let aw = ask_win.as_weak();
        ask_win.on_keep(move || {
            clear_cancel_ask(&wins);
            if let Some(a) = aw.upgrade() {
                let _ = a.hide();
            }
            cancel_keep(&q);
        });
    }
    {
        let q = Arc::clone(&ui_pending);
        let wins = Rc::clone(&xfer_wins);
        let aw = ask_win.as_weak();
        ask_win.on_discard(move || {
            clear_cancel_ask(&wins);
            if let Some(a) = aw.upgrade() {
                let _ = a.hide();
            }
            cancel_discard(&q);
        });
    }
    {
        // 确认框的标题栏 ✕ = 「继续接收」（不改主意），和框里那个按钮一个语义
        let wins = Rc::clone(&xfer_wins);
        let aw = ask_win.as_weak();
        ask_win.window().on_close_requested(move || {
            clear_cancel_ask(&wins);
            if let Some(a) = aw.upgrade() {
                let _ = a.hide();
            }
            cancel_dismiss();
            slint::CloseRequestResponse::KeepWindowShown
        });
    }
    {
        let wins = Rc::clone(&xfer_wins);
        let aw = ask_win.as_weak();
        ask_win.on_dismiss(move || {
            clear_cancel_ask(&wins);
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
        //
        // 结果是「已取消」，不是「失败」：以前这里写 finish(true, ...)，于是用户
        // 点了取消，小窗却变成红字「发送失败 / 已取消发送」（真机截图报的）。
        let round = Arc::clone(&push_round);
        push_win.on_cancel_send(move || {
            log::info("推送小窗：取消发送");
            hfx_receiver::control::cancel();
            if let Ok(mut r) = round.lock() {
                if r.step == PUSH_SENDING {
                    let sent = r.peak_sent;
                    let text = if r.bytes > 0 {
                        format!("已发出 {} / {}", human(sent), human(r.bytes))
                    } else {
                        String::new()
                    };
                    r.finish_cancelled(text);
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
            // 立即生效，并且**连着的手机也跟着变**（308 起两端共用一个值）：
            // set_configured_mb 会提升代次，会话线程发现后发一帧 SPEED_LIMIT，
            // 手机那边改自己的 SpeedLimiter 并把它的设置页也刷成这个数。
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
        // 每台设备的传输窗口：窗口 / 列表 model / 块状态 / 落位状态 / 收起标志都在里面，
        // 建、刷、收全在这一条线程（这个 Timer）上做。
        let xfer_wins = Rc::clone(&xfer_wins);
        let ui_pending = Arc::clone(&ui_pending);
        // 取消确认框那个独立窗口
        let ask_weak = ask_win.as_weak();
        // tick 耗时统计（HFX_TICK_STAT=1 时每秒往日志写一行）：
        // 用来确认「卡」到底是 tick 太重，还是渲染跟不上。
        // env 查询放到循环外：std::env::var 每拍都做一次是白烧（虽然不贵，但没必要）
        let tick_stat_on = std::env::var("HFX_TICK_STAT").is_ok();
        let mut tick_stat_at = std::time::Instant::now();
        let mut tick_stat_sum = 0u128;
        let mut tick_stat_n = 0u32;
        let mut tick_stat_max = 0u128;
        let mut last_bytes = 0u64;
        let mut last_tick = std::time::Instant::now();
        let mut speed = 0f64;

        let mut last_want_h = 0f32;
        let mut boot_hidden = false;
        // 上一次刷进输入框的限速值，用来判断「需要不要再刷一次」。
        // 手机推来新值时（含握手定下来的共用值）就把它写进配置和输入框。
        let mut limit_shown = hfx_receiver::ratelimit::configured_mb();

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
            // ---- 取消确认框：独立窗口，跟着「哪个传输窗口的 cancel-ask」弹/收 ----
            // 传完 / 已经取消 / 用户选了任意一项 → 状态位被清掉 → 这里把窗口收起来。
            if let Some(aw) = ask_weak.upgrade() {
                // 是哪台设备的块弹了「保留 / 不保留」：确认框就摆到那个窗口正中
                // （多窗口时按创建顺序取第一个想弹的 —— 取消是进程级的一件事，
                // 同一时刻只会有一个窗口挂着这个状态位）
                let anchor: Option<(i32, i32, u32, u32)> = {
                    let mut found = None;
                    if let Ok(g) = xfer_wins.try_borrow() {
                        for key in &g.order {
                            if let Some(d) = g.map.get(key) {
                                if d.win.get_cancel_ask() {
                                    let pos = d.win.window().position();
                                    let size = d.win.window().size();
                                    found = Some((pos.x, pos.y, size.width, size.height));
                                    break;
                                }
                            }
                        }
                    }
                    found
                };
                let want = anchor.is_some();
                let shown = aw.window().is_visible();
                if want && !shown {
                    // 标题和正文按方向走（见 app.slint 的 CancelAskWindow.sending）：
                    // 发送方向的半截文件在手机上，「不保留」是让手机删。
                    aw.set_sending(hfx_receiver::control::sending());
                    let _ = aw.show();
                    // 弹在传输窗口正中（先 show 再量尺寸：没显示时尺寸还不可信）
                    if let Some((tw_x, tw_y, tw_w, tw_h)) = anchor {
                        let aw_size = aw.window().size();
                        let x = tw_x + (tw_w as i32 - aw_size.width as i32) / 2;
                        let y = tw_y + (tw_h as i32 - aw_size.height as i32) / 2;
                        aw.window()
                            .set_position(slint::PhysicalPosition::new(x.max(0), y.max(0)));
                    }
                    // 传输窗口自己是 HWND_TOPMOST（§6.5），这个确认框也必须跟着置顶 + 抢前台，
                    // 否则用户看着它在前面、鼠标点下去命中的是后面那个置顶窗口 ——
                    // 真机 2026-11-19：点「保留进度」毫无反应，传输一路跑到 100%。
                    #[cfg(windows)]
                    if let Some(hwnd) = tray_win::find_window_titled(ASK_TITLE)
                        .or_else(|| tray_win::find_window_titled(ASK_TITLE_SEND))
                    {
                        tray_win::bring_to_front(hwnd);
                    }
                } else if !want && shown {
                    let _ = aw.hide();
                }
            }
            let mut s = st.lock().unwrap();

            // ---- 界面回调攒下来的脏活：这里持锁顺手做掉 ----
            //
            // 反过来的顺序（回调自己拿锁）在关窗那一下会自死锁：Timer 正持着这把锁调
            // Slint 的 show/set_size，Windows 消息泵把「关窗」回调重入进来，回调再
            // lock 一次 = 同一线程重入 std::Mutex = 永远等下去（主窗口「未响应」）。
            if let Ok(mut q) = ui_pending.lock() {
                for (pos, placed) in q.pos_saves.drain(..) {
                    save_transfer_pos_at(pos, &mut s, placed);
                }
                if let Some(note) = q.cancel_note.take() {
                    s.cancel_note = Some(note);
                }
            }

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
                            // 峰值：取消会 reset 全局进度，收尾那句「发出去了多少」得靠它
                            r.peak_sent = r.peak_sent.max(session_all);
                            let sending_now = hfx_receiver::control::sending();
                            if sending_now {
                                r.saw_sending = true;
                            }
                            // 用户点过「取消发送」：这一轮的结果是「已取消」，不是失败。
                            // 见过就记着 —— 收尾比 cancel_requested 的复位早不了多少，
                            // 但整整一轮的 150ms tick 里总能撞上。
                            if hfx_receiver::control::cancel_requested() {
                                r.cancel_seen = true;
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
                                // 同样看峰值：传满之后点取消，session_all 会变 0，
                                // 拿它判就会把「已经传完」误判成「只发了一部分」。
                                r.bytes == 0
                                    || r.peak_sent >= r.bytes
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
                                // 取消优先：用户自己停的不叫「失败」，也不能把
                                // 「只发出 215.0 MB / 2.00 GB」当成错误原因摆在红字里。
                                if r.cancel_seen {
                                    let sent_total = r.peak_sent;
                                    log::warn(&format!(
                                        "推送小窗：已取消（已发出 {} / {}）",
                                        human(sent_total),
                                        human(r.bytes)
                                    ));
                                    let text = if r.bytes > 0 {
                                        format!(
                                            "已发出 {} / {}",
                                            human(sent_total),
                                            human(r.bytes)
                                        )
                                    } else {
                                        String::new()
                                    };
                                    r.finish_cancelled(text);
                                    step_changed = true;
                                } else if bytes_done {
                                    log::info(&format!(
                                        "推送小窗：已发送 {} · {} 个文件",
                                        human(r.bytes),
                                        r.files
                                    ));
                                    r.finish(false, String::new());
                                } else {
                                    // 用峰值而不是当前的 session_all：收尾时它已经被 reset 成 0
                                    let sent_total = r.peak_sent;
                                    log::warn(&format!(
                                        "推送小窗：只发出一部分 {} / {}",
                                        human(sent_total),
                                        human(r.bytes)
                                    ));
                                    let done_text =
                                        format!("只发出 {} / {}", human(sent_total), human(r.bytes));
                                    r.finish(true, done_text);
                                }
                            }
                            // ---- 一直没开始的超时说法 ----
                            if !r.saw_sending && !r.failed && !r.cancelled {
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
                            // 成功：窗口直接收起来（完成由传输小窗说）。
                            // 取消：留着说一句「已取消发送」，2 秒后自己收 —— 用户刚点了
                            // 取消，界面总得应一声；以前这里因为「没发完」被判成失败，
                            // 弹的是红字「发送失败 / 只发出 215.0 MB」（真机截图报的）。
                            if !r.failed && !r.cancelled {
                                if pw.window().is_visible() {
                                    let _ = pw.hide();
                                }
                            }
                            if let Some(t) = r.done_at {
                                if !r.failed && t.elapsed() >= Duration::from_secs(2) {
                                    log::info("推送小窗：这一轮收尾，退出小窗状态");
                                    r.reset();
                                }
                            }
                            // 失败 / 取消：窗口必须在 —— 失败挂着等用户看，
                            // 取消显示 2 秒再收。
                            if (r.failed || r.cancelled) && !pw.window().is_visible() {
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
                            pw.set_cancelled(r.cancelled);
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

            // ---- 限速：手机那边改了要反映到本机 ----------------
            //
            // 308 起两端共用一个值，谁改都推给对方。手机推来的值由会话线程写进
            // ratelimit（adopt_configured_mb），这里发现和界面上显示的不一样就更新
            // 配置 + 输入框，并落盘 —— 否则用户下次开程序，框里又是旧数。
            // 配置文件无论如何都要跟着写 —— 那是真正生效的值，和光标在哪无关。
            //
            // 注意：这里**不能**再 st.lock() 一次 —— tick 开头（1756 行）已经拿到那把锁
            // 并一直持有到最后，同线程重复 lock 就是自己把自己锁死（界面直接定住，
            // 配置也一直存不下去）。所以直接改手里这份 s。
            let limit_now = hfx_receiver::ratelimit::configured_mb();
            if limit_now != limit_shown {
                limit_shown = limit_now;
                if s.config.speed_limit_mb != limit_now as u32 {
                    s.config.speed_limit_mb = limit_now as u32;
                    let path = s.config_path.clone();
                    let config = s.config.clone();
                    if let Err(e) = config.save(&path) {
                        log::warn(&format!("保存同步来的限速值失败: {e}"));
                    }
                }
                // 共用值就是真值：**每次都写进框**，不看用户有没有在打字。
                //
                // 之前这里加了个「用户手上有没提交的字就跳过刷新」的闸门，本意是别抢
                // 他的光标。但那个闸门两头坏事：手机刚改完、电脑这边框里还留着半截旧
                // 字，界面就一直显示旧值（用户说的「没有及时刷新出来」）；等他一点别处
                // 失焦，那行旧字又会被当成「用户改的」推回手机，把手机刚设的值顶回去。
                // 共用值只有一份，对端改了就以对端为准，框跟着变。
                // 顺手把「手里有未提交输入」的标志清掉，失焦时就不会再提交一遍。
                ui.set_speed_limit_text(limit_now.to_string().into());
                ui.set_limit_typed(false);
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
            // 取消之后有一段真空：电脑端在重建传输通道（实测十几秒），这期间 PROGRESS
            // 已经 reset、total = 0，可手机队列里明明还排着后面的任务。按老条件
            // active 当场落 false，窗口 3 秒后被自动收起、下一单再弹出来 ——
            // 用户看到的就是「点了取消，窗口消失一下又出现」。
            // 队列里还有没跑完的任务（等待中 / 正在跑）就算这一轮没结束。
            let queue_pending = s.devices.keys().any(|k| hfx_receiver::queue::has_pending(k));
            let active = s.running() && any_connected && (total > 0 || queue_pending);
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
            // 两个方向都要算：发送方向 RECEIVING 一直是 false，只按它判的话
            // 确认框刚弹出来就会被下面那段「传输已经不在收」立刻收掉 ——
            // 用户看到的就是「按了 ×，什么都没弹」。
            let busy_now =
                hfx_receiver::control::receiving() || hfx_receiver::control::sending();
            let idle_now = !busy_now || !active;
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
            if !busy_now {
                // 「这个位」现在每个窗口一份：哪一个挂着就清哪一个（用户的回答只有一个，
                // 所以一律全清 —— 见 clear_cancel_ask）
                let mut any_ask = false;
                if let Ok(g) = xfer_wins.try_borrow() {
                    for key in &g.order {
                        if let Some(d) = g.map.get(key) {
                            if d.win.get_cancel_ask() {
                                any_ask = true;
                                break;
                            }
                        }
                    }
                }
                if any_ask {
                    clear_cancel_ask(&xfer_wins);
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

                // ---- 独立传输窗口：一台设备一个窗口 ----
                // 一条任务 = 一台设备的一次传输（进度按串号分开记，见 receiver::progress_for；
                // 一台设备同时只有一条进行中的传输 —— PROGRESS 只记「当前文件」）。
                // 所以「按设备分组」现在就是「每台设备一块」：每个窗口只喂自己那几块，
                // 窗口内的多块堆叠能力（tracks / model / 竖向排列）原样保留 ——
                // 将来同一台设备并行几条任务时，这里的分组不用改。
                //
                // 顺序用串号排序（和主界面设备列表同序）：块的位置不能因为 HashMap 的
                // 遍历顺序每次都不一样而上下乱跳。
                let mut task_keys: Vec<String> = s
                    .devices
                    .iter()
                    .filter(|(serial, st)| {
                        matches!(st, EngineState::Connected(_)) && !serial.is_empty()
                    })
                    .map(|(serial, _)| serial.clone())
                    .collect();
                task_keys.sort();

                // 还没窗口的设备：先把窗口建出来（Slint 对象只能在 UI 线程建，所以只在
                // 这个 Timer 里建）。
                for key in &task_keys {
                    let exists = {
                        let g = xfer_wins.borrow();
                        g.map.contains_key(key)
                    };
                    if exists {
                        continue;
                    }
                    // 标签默认用设备显示名，重名依次加 " 2"、" 3"：标题后缀必须唯一，
                    // 找 HWND 是按后缀匹配的（两台同名手机会互相抢到对方的窗口）。
                    let base = device_display_name(&s, key);
                    let taken: Vec<String> = {
                        let g = xfer_wins.borrow();
                        g.map.values().map(|d| d.label.clone()).collect()
                    };
                    let label = unique_label(&taken, &base);
                    if let Some(d) = make_dev_xfer_win(&xfer_wins, &ui_pending, key, &label) {
                        xfer_wins.borrow_mut().insert(key.clone(), d);
                    }
                }

                // 这一帧没有任务的设备（断开了 / 停止接收了）：窗口收起来并从表里移除
                //（下一轮重新建、重新摆位 —— 单实例改成每设备一个之后，不需要常驻的空窗口了）。
                let dead: Vec<String> = {
                    let g = xfer_wins.borrow();
                    g.map
                        .keys()
                        .filter(|k| !task_keys.contains(k))
                        .cloned()
                        .collect()
                };
                for key in dead {
                    drop_dev_xfer_win(&xfer_wins, &mut *s, &key);
                }

                // 每个窗口各喂各的块 + 该弹就弹 / 该收就收。
                // 槽位（摆位时往上错开的档位）就是创建顺序里的下标。
                let keys: Vec<String> = {
                    let g = xfer_wins.borrow();
                    g.order.clone()
                };
                // 只有「恰好一个窗口」时才沿用 config 里记的窗口坐标：多窗口是从右下角
                // 自动往上排的（见 tray_win::nudge_to_bottom_right_up）
                let single = keys.len() == 1;
                let saved = if single { s.config.xfer_win_pos } else { None };
                for (slot, key) in keys.iter().enumerate() {
                    // 错开量要在取可变借用**之前**算：stack_up_for 要读一遍所有比它低的窗口，
                    // 那个借用不能和下面的 borrow_mut 撞上。
                    #[cfg(windows)]
                    let up_now = stack_up_for(&xfer_wins.borrow(), &keys, slot);
                    #[cfg(not(windows))]
                    let up_now = 0;
                    let mut wins_g = xfer_wins.borrow_mut();
                    let Some(d) = wins_g.map.get_mut(key) else { continue };

                    // 槽位 / 窗口总数变了（第二台设备冒出来、走掉一台）：按新槽位重排一遍 ——
                    // 否则新窗口会叠在旧窗口上、走掉一台后底下空出一个洞，而且多窗口时不该
                    // 再沿用 config 里那一份单独坐标（见 transfer_window_origin 的说明）。
                    // 两个数都没变就一个像素都不动 —— 用户拖过的位置得保住。
                    if d.slot_seen != slot || d.stack_seen != keys.len() {
                        d.slot_seen = slot;
                        d.stack_seen = keys.len();
                        d.re_anchor();
                        // 已经开着的窗口不会再走 show_at 那一套，这里补一帧落位
                        if d.shown {
                            d.settle(saved, slot, up_now);
                        }
                    }
                    // 让这个窗口的 tracks 和「这台设备的任务」一一对应：还在的原样留着
                    //（曲线 / 量程 / 速率不能因为别的设备下线就归零），没了的下车，
                    // 新来的追加在末尾。
                    let mut own: Vec<String> = task_keys
                        .iter()
                        .filter(|k| *k == &d.serial)
                        .cloned()
                        .collect();
                    // 取消之后这一单就作废了：卡片要撤掉。
                    //
                    // 以前不撤：块是「一台设备一块」常驻的（这样连传几单不会闪），取消之后
                    // 它要么停在半路、要么亮成「已完成 100%」，而真机上它要等到**下一单开始**
                    // 才被换掉 —— 用户实测这段有 20 秒，中间盯着一个再也不动的进度条，
                    // 当然以为卡死了。
                    //
                    // 失败不在此列：那块红字要留在屏幕上给用户看原因。
                    //
                    // 只在**后面还排着等待中的任务**时才撤：那种情形下整窗的高度由后面的
                    // 等待块撑住，撤掉中间这块是「刷新」，用户马上看到队列在往前走。
                    // 若队列里没有等待块（单文件分享后取消，或最后一单被取消），撤掉之后
                    // 窗口里一块不剩，高度当场从 156 塌成一条只挂着「详细信息」的 42px 窄条
                    // —— 那才是真的「窗口消失一下又出来」。这种情形留着这块：标题已经改成
                    // 「已取消」，块里停住的就是取消那一刻的进度，3 秒后随窗口一起收起来。
                    //
                    // 判据必须用 waiting_for 而不是 has_pending：被取消的这一单在手机的
                    // QUEUE_STATE 里还会留着 state==1（正在跑）直到它自己收尾，按
                    // has_pending 算就永远是「后面还有」，卡片当场被撤、窗口照样塌成 42px
                    //（2026-11-24 真机复现：ops=1 → ops=0，高度 156 → 42）。
                    let more_queued = !hfx_receiver::queue::waiting_for(&d.serial).is_empty();
                    if hfx_receiver::control::cancelled() && !failed && more_queued {
                        own.clear();
                    }
                    let mut old_tracks = std::mem::take(&mut d.tracks);
                    let mut next_tracks: Vec<OpTrack> = Vec::with_capacity(own.len());
                    for k in &own {
                        match old_tracks.iter().position(|t| &t.serial == k) {
                            Some(i) => next_tracks.push(old_tracks.remove(i)),
                            None => next_tracks.push(OpTrack::new(k)),
                        }
                    }
                    d.tracks = next_tracks;

                    // ---- 手机排队中的任务（QUEUE_STATE，309）----
                    //
                    // 手机一次选了几十个文件时，同一刻只有一个在传，其余排着队；
                    // 这些排队的以前电脑端完全看不见（用户以为选中的文件丢了）。
                    // 现在它们接在「正在跑的那块」后面，画成「等待中」。
                    //
                    // 只画 state == 0：state == 1 的那一条就是本窗口正在跑的这个块
                    //（再画一遍就成了同一份文件占两块），2 / 3 已经收尾 / 失败，
                    // 失败的红字走 RESULT / failure 那一套，这里再画一块会自相矛盾。
                    // 顺序就是手机报来的顺序 —— 那是用户眼里的排队次序。
                    let waiting = hfx_receiver::queue::waiting_for(&d.serial);
                    // 块总数 = 正在跑的 + 等待中的：第 1 行的「设备名：」前缀按它判断，
                    // 只有一块时保持和以前一模一样的写法。
                    let op_count = d.tracks.len() + waiting.len();
                    let track_count = d.tracks.len();
                    let fail_now = failure_now.clone().unwrap_or_default();
                    // 暂停 / 失败是**进程级**的（control.rs 的 static 标志 + 全局失败快照），
                    // 展开 / 可见性是**这个窗口**自己的。块数只算这个窗口里的。
                    let ctx = OpCtx {
                        paused: hfx_receiver::control::paused(),
                        failed,
                        failure_text: &fail_now,
                        op_count,
                        details: d.details,
                        visible: d.win.window().is_visible(),
                    };
                    let mut ops_out: Vec<XferOp> = Vec::with_capacity(op_count);
                    // 明细行只算**非等待块**：等待块永远只画精简态，一行明细都没有
                    let mut detail_rows = 0usize;
                    let mut failed_ops = 0usize;
                    let mut all_finished = true;
                    for track in d.tracks.iter_mut() {
                        // 名字用手机自己报的（「Xiaomi Pad 7 Pro」）；还没解析出来就先顶
                        // 串号前 8 位，免得设备刚连上来那一下界面上是个空名字。
                        let name = device_display_name(&s, &track.serial);
                        let op = track.frame(&ctx, &name);
                        if op.failed {
                            failed_ops += 1;
                        }
                        // 每块至少按 1 行算：rows 为 0 时块里画的是「这一轮还没有分通道数据」
                        // 那一行，同样 22px（高度公式见 transfer_window_height）。
                        detail_rows += track.rows_count.max(1);
                        all_finished &= op.done;
                        ops_out.push(op);
                    }
                    // 等待块接在后面。它们只画精简态（app.slint 里 XferOperation 的 slim），
                    // 高度固定 114、明细行一行都不占 —— 高度公式里是单独的一项
                    // （waiting_ops × TW_WAITING_OP），所以**不能**把它们的行数加进 detail_rows。
                    //
                    // 手机队列里 state==1 的那一条（正在传）**不画**：它对应的是本窗口已有的
                    // 那块进度，再画一遍就成了同一个文件占两行。这里只用它的名字校准块标题 ——
                    // 用户实测「多选后电脑端一直显示第一个文件」，就是因为块的标题来自本端
                    // PROGRESS 的路径，而"手机现在在传哪一个"只有队列知道。
                    let queue_running = hfx_receiver::queue::running_for(&d.serial);
                    if let (Some(run), Some(op)) = (queue_running.as_ref(), ops_out.first_mut()) {
                        // 只有「还在传、没失败」的块才改：已经收尾的那块名字是实的，不该被覆盖
                        if !op.failed && !op.done {
                            let short = run
                                .name
                                .rsplit(['/', '\\'])
                                .next()
                                .unwrap_or(&run.name)
                                .to_string();
                            if !short.is_empty() && op.file_name.as_str() != short {
                                op.file_name = short.as_str().into();
                                op.action_text = if op_count > 1 {
                                    format!(
                                        "{}：{}（{}）",
                                        device_display_name(&s, &d.serial),
                                        short,
                                        human(run.size)
                                    )
                                    .into()
                                } else {
                                    format!("{}（{}）", short, human(run.size)).into()
                                };
                            }
                        }
                    }
                    // 取消之后卡片上的数字要冻在断开那一刻（原因见 frozen_op 的说明）。
                    // 没取消就每帧把这张卡记下来 —— 它永远是「最近一张正常卡片」。
                    let cancelled_now = hfx_receiver::control::cancelled();
                    if cancelled_now {
                        if !failed && !d.tracks.is_empty() {
                            if let Some(mut frozen) = d.frozen_op.clone() {
                                frozen.done = true;
                                frozen.flow_on = false;
                                frozen.paused = false;
                                // 速率那一栏换成「已取消」：冻住的 5.0 MB/s 看着像还在传，
                                // 和暂停时把速率位写「已暂停」同一个道理。
                                // 明细是「已收 / 总量 · 速率」一整条，按最后那个分隔符切开再换尾。
                                if let Some((head, _)) = frozen.detail_text.rsplit_once(" · ") {
                                    frozen.detail_text =
                                        format!("{} · {}", head, i18n::tr("已取消", "cancelled"))
                                            .into();
                                }
                                if !ops_out.is_empty() {
                                    ops_out[0] = frozen;
                                    all_finished = ops_out.iter().all(|o| o.waiting || o.done);
                                }
                            }
                        }
                    } else if let Some(first) = ops_out.first() {
                        d.frozen_op = Some(first.clone());
                    }
                    let waiting_ops = waiting.len();
                    if !waiting.is_empty() {
                        let device_name = device_display_name(&s, &d.serial);
                        for item in &waiting {
                            ops_out.push(waiting_op(&device_name, op_count, item));
                        }
                        // 队列里还有排着的块：这一轮就不算「全部收尾」，别亮「已完成」
                        all_finished = false;
                    }
                    // 写进模型：块数变了就整表换（Slint 正好该重建那几块）；否则只把内容变了
                    // 的行写回去 —— 整表换会把每一块的元素全拆了重建（一块几十个元素），
                    // 没变的块一个都不该动。
                    //
                    // 模型里第 i 行 = tracks[i]（i < track_count），之后才是等待块 ——
                    // 两段的下标基准不同，所以比较时分开走：tracks 那份各自记在
                    // OpTrack.sent 里，等待块那份记在 DevXferWin.waiting_sent 里。
                    if d.model.row_count() != ops_out.len() {
                        d.model.set_vec(ops_out.clone());
                        for (i, op) in ops_out[..track_count].iter().enumerate() {
                            d.tracks[i].sent = Some(op.clone());
                        }
                        d.waiting_sent = ops_out[track_count..].to_vec();
                    } else {
                        for (i, op) in ops_out[..track_count].iter().enumerate() {
                            if d.tracks[i].sent.as_ref() != Some(op) {
                                d.model.set_row_data(i, op.clone());
                                d.tracks[i].sent = Some(op.clone());
                            }
                        }
                        for (j, op) in ops_out[track_count..].iter().enumerate() {
                            let row = track_count + j;
                            if d.waiting_sent.get(j) != Some(op) {
                                d.model.set_row_data(row, op.clone());
                            }
                        }
                        if d.waiting_sent.as_slice() != &ops_out[track_count..] {
                            d.waiting_sent = ops_out[track_count..].to_vec();
                        }
                    }
                    // 这里**不能**再 set_ops 一次。
                    //
                    // 窗口建出来的时候已经 set_ops 过一次（见 create_dev_xfer_win），
                    // 之后改的是同一个 VecModel，Slint 会自己跟着刷新。
                    // 每帧再包一个新的 ModelRc 塞回去，等于每 16ms 告诉 Slint
                    // 「整个列表换了」，它就把每一块连同里头上百个元素全拆了重建 ——
                    // 表现就是暂停 / 取消按钮一直闪、按下去没反应（鼠标按下那一刻
                    // 元素正好被销毁重建，clicked 根本不会派发）。
                    // 真机 2026-11-22 复现，改回「只建一次」。

                    // 标题栏那两个位：多块时标题只说「N 个正在运行的操作」，单块时靠它们
                    // 保持原来那三种说法（正在传输 / 已完成 / 接收失败）。
                    //
                    // 「空闲」计时器：这一窗口的所有块都收尾了就从这一刻起算，
                    // 只要还有一块在跑 / 队列里还有排着的就清零。它同时管三件事 ——
                    // 「已完成」要不要亮（TW_DONE_QUIET_MS）、窗口几时收（3 秒）、
                    // 以及没在传的时候窗口还要不要留（TW_IDLE_KEEP_SECS）。
                    if all_finished {
                        d.idle_since.get_or_insert(now);
                    } else {
                        d.idle_since = None;
                    }
                    // 队列两单之间那零点几秒不算「传完」：标题别在「已完成 / 正在传输」之间闪
                    let quiet_ms = d.idle_since.map(|t| t.elapsed().as_millis()).unwrap_or(0);
                    let done_now = all_finished && quiet_ms >= TW_DONE_QUIET_MS;
                    if d.win.get_failed() != failed {
                        d.win.set_failed(failed);
                    }
                    if d.win.get_done() != done_now {
                        d.win.set_done(done_now);
                    }
                    // 取消之后块已经撤掉（上面 own.clear()），这里一块不剩 = all_finished。
                    // 只看 done 的话标题会亮「已完成」—— 用户点的是「中断」，
                    // 看到「已完成」会以为文件传完了（真机实测亮了 2 秒才收窗口）。
                    let ended_by_cancel = all_finished && hfx_receiver::control::cancelled();
                    if d.win.get_ended_by_cancel() != ended_by_cancel {
                        d.win.set_ended_by_cancel(ended_by_cancel);
                    }
                    // 高度公式要的三个汇总值：Slint 没有「对数组求和」的表达式，
                    // 逐块累加只能在这边算（和 rows / segs 同一个来路）。
                    // total_rows 只含非等待块，等待块自己的个数另走 waiting_ops。
                    if d.win.get_total_rows() != detail_rows as i32 {
                        d.win.set_total_rows(detail_rows as i32);
                    }
                    if d.win.get_waiting_ops() != waiting_ops as i32 {
                        d.win.set_waiting_ops(waiting_ops as i32);
                    }
                    if d.win.get_failed_ops() != failed_ops as i32 {
                        d.win.set_failed_ops(failed_ops as i32);
                    }
                    // 失败原因挂在每块自己身上（红色那行），这里再往主窗口的状态卡下面放一句
                    if let Some(reason) = failure_now.as_deref() {
                        let note = format!("{}：{}", i18n::tr("这次传输失败了", "the transfer failed"), reason);
                        if ui.get_notice_text() != note {
                            ui.set_notice_text(note.into());
                        }
                    }
                    // × 的含义按方向分（接收 = 弹保留/不保留，发送 = 直接中断），
                    // 方向由引擎的 control 标志给出，不靠界面自己猜。多块时这条是共用的。
                    let sending_now = hfx_receiver::control::sending();
                    if d.win.get_sending_now() != sending_now {
                        d.win.set_sending_now(sending_now);
                    }
                    let cancelling_now = hfx_receiver::control::cancel_requested();
                    if d.win.get_cancelling() != cancelling_now {
                        d.win.set_cancelling(cancelling_now);
                    }
                    // 流光的位置 / 亮不亮都在每块自己的 op.flow / op.flow-on 里，见 OpTrack::frame

                    // 精简：界面上不再显示「合计速率 · 剩余时间」（用户选的），这里照旧算一份
                    // 交给属性（日志和其它小窗复用），只是布局里不再占那一行高度。
                    // 仍是**进程级**那组数字（PROGRESS 的总量），和改之前一模一样。
                    let summary = if speed > 0.5 && total_bytes > done {
                        let remain = ((total_bytes - done) as f64 / speed) as u64;
                        format!("合计 {} · 剩余约 {}", human_speed(speed), human_eta(remain))
                    } else {
                        format!("合计 {} · 剩余时间未知", human_speed(speed))
                    };
                    if d.win.get_summary_text() != summary.as_str() {
                        d.win.set_summary_text(summary.into());
                    }
                    // 高度同步不放在这里：暂停等用户选「保留/不保留」的时候 active 已经是 false，
                    // 这一支根本不跑，对话框就会被 151 高的窗口裁掉（真机第一次点 ✕ 就是这个下场）。
                    // 下面 active 分支之外有一段专门干这个。
                    if failed {
                        // 失败了：停在窗口上等用户看见。3 秒自动收是对「传完了」才有的礼遇，
                        // 对失败用就是骗人（用户回头只看到文件在那儿，以为传好了）。
                        // 窗口要是已经收起来过（上一轮刚自动收完 / 用户点过收起），重新弹出来。
                        if !d.shown && !d.dismissed {
                            d.shown = true;
                            d.show_at(saved, slot, up_now);
                        }
                        if d.shown {
                            d.settle(saved, slot, up_now);
                        }
                        if d.details {
                            d.details = false;
                            d.win.set_details(false);
                        }
                    } else if all_finished {
                        // 所有块都收尾了才算「传完」：一个窗口里有块还在跑，窗口就留着。
                        // 传完了就把明细收起来：窗口回到收起高度，
                        // 3 秒的「已完成」不用顶着 400px 的空白
                        if d.details {
                            d.details = false;
                            d.win.set_details(false);
                        }
                        // 空档满 3 秒就收窗口。数的是 idle_since（和「已完成」同一个起点）：
                        // 队列模式下两单之间那零点几秒够不着这个数，窗口一动不动；
                        // 真传完了用户在干别的，3 秒后照旧自己收起来。
                        if quiet_ms >= 3000 {
                            if d.shown {
                                log::debug(&format!("窗口收起：{} 空闲 {} ms（已取消={}）", d.serial, quiet_ms, hfx_receiver::control::cancelled()));
                                d.shown = false;
                                let placed = d.placed_at();
                                // 坐标只有「恰好一个窗口」时才记（多窗口是自动往上排的）
                                if single {
                                    save_transfer_pos(&d.win, &mut *s, placed);
                                }
                                let _ = d.win.hide();
                            }
                            // 这一轮到此为止：用户中途点过的「收起」只对那一轮有效。
                            // 窗口现在**不销毁、留着给下一单**（见 idle_since 的说明），
                            // 所以必须在这里把 dismissed 清掉 —— 否则用户收起过一次之后，
                            // 后面每一单都不再弹了（以前靠「每轮重建窗口」掩盖了这一条）。
                            d.dismissed = false;
                        }
                    } else {
                        // 又一轮新传输（文件换了 / 续传接上了）：允许重新弹
                        // 新一轮传输才重新弹（用户在这个窗口上点过「收起」就别再烦他）
                        if !d.shown && !d.dismissed {
                            d.shown = true;
                            // 第一次出现：贴到工作区右下角（多窗口时往上错开；只有单窗口
                            // 才按用户拖过的位置），窗口建出来后再补几帧落位 + 置顶
                            d.show_at(saved, slot, up_now);
                        }
                        if d.shown {
                            d.settle(saved, slot, up_now);
                        }
                    }
                }
            } else if failed {
                // 失败但已经不在传输中（收尾那一帧就走到了这里）：保证有个窗口还在，
                // 别让用户错过「接收失败」这一眼。
                //
                // 这时已经拿不到「正在传的任务」了（引擎收尾时设备表已经清空），所以按失败
                // 那一刻的快照拼一块出来：和正常帧同一个 model、同一个组件，布局 / 高度
                // 都不用走特殊路径。
                //
                // 多窗口时**只让一个窗口亮**（创建顺序第一个；一个都没有就照上次那台设备的
                // 身份重建一个）：失败是进程级的一件事，每台都亮一排同样的红字只会让用户
                // 以为所有设备都传坏了。
                let key = {
                    let g = xfer_wins.borrow();
                    g.order
                        .first()
                        .cloned()
                        .or_else(|| g.last.clone())
                        .unwrap_or_default()
                };
                let exists = {
                    let g = xfer_wins.borrow();
                    g.map.contains_key(&key)
                };
                if !exists {
                    // 后缀不能是空串：find_transfer_window_of("") 会匹配本进程任何一个
                    // 可见窗口（连主界面都会中），所以没身份也得给个兜底标签。
                    let base = if key.is_empty() {
                        i18n::tr("传输", "transfer").to_string()
                    } else {
                        device_display_name(&s, &key)
                    };
                    let taken: Vec<String> = {
                        let g = xfer_wins.borrow();
                        g.map.values().map(|d| d.label.clone()).collect()
                    };
                    let label = unique_label(&taken, &base);
                    if let Some(d) = make_dev_xfer_win(&xfer_wins, &ui_pending, &key, &label) {
                        xfer_wins.borrow_mut().insert(key.clone(), d);
                    }
                }
                // 其余窗口这一帧没有块了：收起来并移除（和 active 分支同一套规矩）
                let others: Vec<String> = {
                    let g = xfer_wins.borrow();
                    g.order.iter().filter(|k| **k != key).cloned().collect()
                };
                for k in others {
                    drop_dev_xfer_win(&xfer_wins, &mut *s, &k);
                }
                let mut wins_g = xfer_wins.borrow_mut();
                let single = wins_g.map.len() == 1;
                let slot = wins_g.slot_of(&key);
                let saved = if single { s.config.xfer_win_pos } else { None };
                // 错开量按「下面那些窗口有多高」算（这一支里 wins_g 已经借出去了，
                // 直接把它的不可变重借用传进去，别再 borrow 一次）。
                #[cfg(windows)]
                let up_now = {
                    let order = wins_g.order.clone();
                    stack_up_for(&wins_g, &order, slot)
                };
                #[cfg(not(windows))]
                let up_now = 0;
                if let Some(d) = wins_g.map.get_mut(&key) {
                    if !d.win.window().is_visible() && !d.dismissed {
                        let pct = hfx_receiver::receiver::failure_percent().unwrap_or(99);
                        let fail_text = failure_now.clone().unwrap_or_default();
                        d.model.set_vec(vec![XferOp {
                            action_text: i18n::tr("接收失败", "receiving failed").into(),
                            percent: pct,
                            detail_text: "".into(),
                            file_name: "".into(),
                            eta_text: i18n::tr("正在计算...", "calculating...").into(),
                            remain_text: "0 (0 B)".into(),
                            speed_text: "".into(),
                            paused: false,
                            done: true,
                            waiting: false,
                            failed: true,
                            fail_text: fail_text.into(),
                            fill: 0.0,
                            flow: 0.0,
                            flow_on: false,
                            segs: slint::ModelRc::new(slint::VecModel::from(Vec::<XferSeg>::new())),
                            curve_path: "".into(),
                            guide_y: 75.0,
                            rows: slint::ModelRc::new(slint::VecModel::from(Vec::<XferRow>::new())),
                        }]);
                        let model = d.model.clone();
                        d.win.set_ops(slint::ModelRc::new(model));
                        // 这一帧没有任务了：块状态留着没意义（下一轮重新建 OpTrack）
                        d.tracks.clear();
                        // 高度公式那三个汇总值：一块（不是等待块）、一行都没有、
                        // 一块失败（那行红字 +20）
                        d.win.set_total_rows(1);
                        d.win.set_waiting_ops(0);
                        d.win.set_failed_ops(1);
                        d.win.set_done(true);
                        d.win.set_failed(true);
                        d.shown = true;
                        d.show_at(saved, slot, up_now);
                    }
                }
            } else {
                // 传输结束 / 停止接收 / 设备全断开。
                //
                // **不能一律立刻销毁**（2026-11-22 真机：「一个任务传完，窗口消失一下又打开」）：
                // 手机多选时是一个文件一个任务，两单之间电脑端有一段真空（进度 reset、
                // total = 0 → active 落成 false），这一支就会把窗口销毁，下一单再建一个 ——
                // 屏幕上就是闪一下。
                //
                // 现在的规矩：
                //   * 引擎停了（用户按了「停止接收」）/ 这台设备已经不在 Connected 里了
                //     → 立刻销毁（这才是「这一轮结束了」的真正含义）；
                //   * 还连着、只是暂时没在传 → 留着等下一单；空档超过 TW_IDLE_KEEP_SECS
                //     才销毁（一次普通传输结束后窗口早就藏起来了，这里只是多留个对象）。
                let keys: Vec<String> = {
                    let g = xfer_wins.borrow();
                    g.order.clone()
                };
                let running_now = s.running();
                for key in keys {
                    let still_connected = running_now
                        && matches!(s.devices.get(&key), Some(EngineState::Connected(_)));
                    if still_connected {
                        let now = std::time::Instant::now();
                        // (窗口还留不留, 这个空档是不是刚开始的, 这个空档已经空了多久)
                        let (keep, fresh, quiet_ms) = {
                            let mut g = xfer_wins.borrow_mut();
                            match g.map.get_mut(&key) {
                                Some(d) => {
                                    let fresh = d.idle_since.is_none();
                                    let since = *d.idle_since.get_or_insert(now);
                                    (
                                        since.elapsed().as_secs() < TW_IDLE_KEEP_SECS,
                                        fresh,
                                        since.elapsed().as_millis(),
                                    )
                                }
                                None => (false, false, 0),
                            }
                        };
                        if keep {
                            // 一次空档只记一行（这一支每 16ms 跑一次，逐帧记会把日志刷爆）。
                            // 这行也是「窗口闪一下」那件事的证据：改之前这里没有任何判断，
                            // 任何一帧 active=false 都会把窗口整个销毁、下一帧再建一个。
                            if fresh {
                                log::info(&format!("{}：空档开始，窗口先留着等下一单", key));
                            }
                            // 空档满 3 秒 = 这一轮真的结束了（队列两单之间只有零点几秒，
                            // 够不到这个数）。收窗口 + 把「用户点过收起」复位这件事，
                            // active 分支里也做了一遍，但那一支**根本不会再跑**：
                            // 一单传完 total 就归零，active 立刻落成 false，从那一刻起
                            // 每帧都走这一支。所以必须在这儿补上，否则 ——
                            // 用户中途点过「收起」，下一单就永远不弹了
                            //（窗口现在不销毁、留着给下一单，见 idle_since 的说明）。
                            if quiet_ms >= 3000 {
                                let single = xfer_wins.borrow().map.len() == 1;
                                let mut hide_now = false;
                                if let Some(d) = xfer_wins.borrow_mut().map.get_mut(&key) {
                                    d.dismissed = false;
                                    if d.shown {
                                        d.shown = false;
                                        hide_now = true;
                                        let placed = d.placed_at();
                                        if single {
                                            save_transfer_pos(&d.win, &mut *s, placed);
                                        }
                                    }
                                }
                                if hide_now {
                                    if let Some(d) = xfer_wins.borrow().map.get(&key) {
                                        let _ = d.win.hide();
                                    }
                                }
                            }
                            continue;
                        }
                    }
                    drop_dev_xfer_win(&xfer_wins, &mut *s, &key);
                }
            }

            // 传输窗口的高度：明细展开 / 「接收失败」那行会改它，**每帧都要跟**。
            // （取消确认框改成独立窗口了，不再影响这个高度。）
            // 每个窗口各算各的：块数 / 明细行数 / 失败块都只算自己窗口里的那些块。
            let all_keys: Vec<String> = {
                let g = xfer_wins.borrow();
                g.order.clone()
            };
            // 这一帧有窗口改过高度 → 循环结束后把所有窗口重新对一次线
            let mut layout_dirty = false;
            for (_hslot, win_key) in all_keys.iter().enumerate() {
                let win_key = win_key.clone();
                let mut wins_g = xfer_wins.borrow_mut();
                let Some(d) = wins_g.map.get_mut(&win_key) else { continue };
                if !d.win.window().is_visible() {
                    continue;
                }
                // 框还开着、传输自己先跑完了：框里的三个选项已经没有意义，
                // 直接收掉（不替用户选 —— 文件已经在盘上了，选「不保留」才会删，那是骗人）
                if d.win.get_cancel_ask() && hfx_receiver::receiver::last_failure().is_none() {
                    let pct = hfx_receiver::receiver::PROGRESS.snapshot();
                    let (_, done_now, total_now, _, _) = &pct;
                    if *total_now > 0 && *done_now >= *total_now {
                        d.win.set_cancel_ask(false);
                    }
                }
                // 每帧按当前状态重算目标高度，和上一次比：变了才 set_size。
                // 以前只在「明细展开 / 失败」变化时才动，靠 active 分支里记的
                // last_row_count 兜底；块数也会变（设备增减），干脆每帧算一遍再比，
                // 反正只是几次乘法。
                let details_now = d.details;
                // 「谁把窗口改小了」的对照日志：Slint 自己会按 preferred-height 重排，
                // 光看 set_size 那一处并不知道窗口后来被别人改成什么样。
                {
                    let scale_now = d.win.window().scale_factor();
                    let actual_h = d.win.window().size().height as f32 / scale_now;
                    if (actual_h - d.last_actual_h).abs() > 1.0 {
                        log::debug(&format!(
                            "窗口实际高度 {} → {}（ops={} waiting={} rows={}）",
                            d.last_actual_h,
                            actual_h,
                            d.model.row_count(),
                            d.win.get_waiting_ops(),
                            d.win.get_total_rows()
                        ));
                        d.last_actual_h = actual_h;
                    }
                }
                // 先算完整内容高度，再按屏幕可用高度封顶 —— 内容更高的时候窗口不长高，
                // 改成窗口里能滚（app.slint 里把整列包进了 Flickable）。
                // 封顶值和 app.slint 的 cap-h 是同一个算式（screen-h 那条属性也从这里喂）。
                let want_h = transfer_window_height(
                    details_now,
                    d.model.row_count(),
                    d.win.get_waiting_ops().max(0) as usize,
                    d.win.get_total_rows().max(0) as usize,
                    d.win.get_failed_ops().max(0) as usize,
                )
                .min(transfer_window_cap(d.win.get_screen_h()));
                if (want_h - d.last_h).abs() > 0.5 {
                    log::debug(&format!(
                        "窗口高度 {} → {}（ops={} waiting={} rows={} failed={} details={}）",
                        d.last_h, want_h, d.model.row_count(),
                        d.win.get_waiting_ops(), d.win.get_total_rows(),
                        d.win.get_failed_ops(), details_now
                    ));
                    d.last_h = want_h;
                    d.win
                        .window()
                        .set_size(slint::LogicalSize::new(460.0, want_h));
                    // 长高之后要重新摆一次，但**不能在这一格里做**：底下的窗口长高时，
                    // 上面那台的底边也得跟着抬，而它这一帧的高度根本没变、压根不进这个 if
                    // （真机截图：红米长到 195，把上面的小米压得只剩 86px 标题栏）。
                    // 所以这里只记个标记，循环结束后统一重排 —— 见下面「摆位重排」。
                    layout_dirty = true;
                }
            }

            // ---- 摆位重排：任何一个窗口的高度变过，就把**所有**窗口重新对一次线 ----
            //
            // 为什么必须全体重排：窗口是从下往上叠的，下面那台长高之后，上面那台的底边
            // 也得跟着抬，否则就被压住（真机截图：红米长到 195，把上面的小米压得只剩
            // 86px 标题栏，进度条和暂停/取消按钮全被盖着，看着像只传了一个）。
            // 而高度没变的那台根本不会进上面那个 if，所以只能在这里统一做。
            //
            // 对线用 nudge_to_bottom_right_up（把可见下边对到「工作区底边 - margin - 错开量」），
            // **上下都会走**。只用 keep_on_screen 那种「越界才推回来」不行：窗口长矮了
            // 永远不回位，一次一次往上爬，最后贴着屏幕顶。
            #[cfg(windows)]
            if layout_dirty {
                let keys_now: Vec<String> = {
                    let g = xfer_wins.borrow();
                    g.order.clone()
                };
                // 错开量先一次算齐：算的时候要读所有窗口的高度，不能一边拿可变借用一边读
                let ups: Vec<i32> = {
                    let g = xfer_wins.borrow();
                    (0..keys_now.len())
                        .map(|i| stack_up_for(&g, &keys_now, i))
                        .collect()
                };
                for (i, k) in keys_now.iter().enumerate() {
                    let mut g = xfer_wins.borrow_mut();
                    let Some(d) = g.map.get_mut(k) else { continue };
                    if !d.win.window().is_visible() {
                        continue;
                    }
                    let Some(hwnd) = tray_win::find_transfer_window_of(&d.suffix) else {
                        continue;
                    };
                    let before = tray_win::window_position(hwnd);
                    // 用户自己拖走的窗口不按槽位对线：那条线他只是路过，不是他要的位置。
                    // 判据沿用 save_transfer_pos 那一套（当前位置 ≠ 我们摆的位置 = 他拖过）。
                    let dragged = matches!((d.tw.placed_at, before), (Some(a), Some(b)) if a != b);
                    if dragged {
                        // 拖过的只保证别被屏幕切掉
                        tray_win::keep_on_screen_up(hwnd, TW_MARGIN, 0);
                    } else {
                        tray_win::nudge_to_bottom_right_up(hwnd, TW_MARGIN, ups[i]);
                    }
                    let after = tray_win::window_position(hwnd);
                    if before != after {
                        log::debug(&format!(
                            "重排 {}：槽位 {} 错开 {} px（拖过={}），{:?} → {:?}",
                            d.suffix, i, ups[i], dragged, before, after
                        ));
                        // 我们摆的位置跟着更新，否则下一帧会把它当成「用户拖过」
                        d.tw.placed_at = after;
                    }
                }
            }
            #[cfg(not(windows))]
            let _ = layout_dirty;

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
/// 本机架构对应的接收器附件名。
///
/// x64 沿用老名字 —— 1.5.0 及更早的更新器只认这个字符串，改名会让那些用户
/// 收到「更新信息不完整」。ARM64 单独一个名字，免得两边互相覆盖。
const EXE_ASSET: &str = if cfg!(target_arch = "aarch64") {
    "HybridFileXfer-Receiver-arm64.exe"
} else {
    "HybridFileXfer-Receiver.exe"
};
/// 兜底：ARM64 版万一没传上去，ARM 机器也还能拿 x64 版（Windows 会自己用
/// 仿真跑起来，慢一点但能用）。x64 机器这边是空串，等于没有兜底。
const EXE_ASSET_FALLBACK: &str = if cfg!(target_arch = "aarch64") {
    "HybridFileXfer-Receiver.exe"
} else {
    ""
};

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
        let mut exe_fallback = String::new();
        if let Some(assets) = release["assets"].as_array() {
            for asset in assets {
                let name = asset["name"].as_str().unwrap_or_default();
                let url = asset["browser_download_url"].as_str().unwrap_or_default();
                if name == VERSION_ASSET {
                    version_url = url.to_string();
                } else if name == EXE_ASSET {
                    exe_url = url.to_string();
                } else if !EXE_ASSET_FALLBACK.is_empty() && name == EXE_ASSET_FALLBACK {
                    exe_fallback = url.to_string();
                }
            }
        }
        // 本机架构那一份优先；没有才退回 x64 版。
        if exe_url.is_empty() {
            exe_url = exe_fallback;
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