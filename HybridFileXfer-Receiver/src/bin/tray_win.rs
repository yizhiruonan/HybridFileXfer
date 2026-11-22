//! 极简 Windows 系统托盘（Shell_NotifyIcon），不引入额外的 UI 框架。

#![cfg(windows)]

use std::sync::Mutex;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTONULL,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, EnumWindows, FindWindowW, GetCursorPos, GetMessageW, GetWindowRect,
    GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible, LoadIconW,
    PostQuitMessage, SetForegroundWindow, SetWindowPos, ShowWindow, TrackPopupMenu,
    TranslateMessage, HWND_TOPMOST, MSG, MF_STRING, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOZORDER, SW_HIDE, SW_SHOW, TPM_BOTTOMALIGN, TPM_RIGHTBUTTON, WM_APP, WM_COMMAND,
    WM_DESTROY, WM_LBUTTONDBLCLK, WM_RBUTTONUP, WNDCLASSW,
};

// windows-sys 0.59 没开 Win32_Graphics_Dwm 特性，自己声明（和上面的 RegisterClassW 一样）
#[link(name = "dwmapi")]
extern "system" {
    fn DwmGetWindowAttribute(
        hwnd: HWND,
        dw_attribute: u32,
        pv_attribute: *mut std::ffi::c_void,
        cb_attribute: u32,
    ) -> i32;
}

// windows-sys 0.59 没有导出 RegisterClassW，自己声明
#[link(name = "user32")]
extern "system" {
    fn RegisterClassW(lp_wnd_class: *const WNDCLASSW) -> u16;
}

const WM_COPYDATA: u32 = 0x004A;

#[repr(C)]
struct CopyDataStruct {
    dw_data: usize,
    cb_data: u32,
    lp_data: *mut std::ffi::c_void,
}

const WM_TRAY: u32 = WM_APP + 1;
const CMD_SHOW: usize = 1001;
const CMD_QUIT: usize = 1002;
const TRAY_ID: u32 = 1;

static ON_SHOW: Mutex<Option<Box<dyn Fn() + Send>>> = Mutex::new(None);
static ON_QUIT: Mutex<Option<Box<dyn Fn() + Send>>> = Mutex::new(None);

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 找到运行中实例的托盘窗口（--send 靠它投递文件）
pub fn tray_window_handle() -> Option<HWND> {
    let class = wide("HfxTrayWindow");
    let title = wide("HybridFileXfer Tray");
    let hwnd = unsafe { FindWindowW(class.as_ptr(), title.as_ptr()) };
    if hwnd.is_null() { None } else { Some(hwnd) }
}

/// 把文件列表投递给已经运行的实例，成功返回 true
pub fn deliver_paths_with_target(paths: &[std::path::PathBuf], target: Option<&str>) -> bool {
    deliver(paths, target)
}

pub fn deliver_paths(paths: &[std::path::PathBuf]) -> bool {
    deliver(paths, None)
}

fn deliver(paths: &[std::path::PathBuf], target: Option<&str>) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::SendMessageW;
    let Some(hwnd) = tray_window_handle() else {
        return false;
    };
    let mut lines: Vec<String> = Vec::with_capacity(paths.len() + 1);
    if let Some(target) = target {
        lines.push(format!("@to={target}"));
    }
    lines.extend(paths.iter().map(|path| path.display().to_string()));
    let text = lines.join("\r\n");
    let mut buffer: Vec<u16> = text.encode_utf16().collect();
    let cds = CopyDataStruct {
        dw_data: 0,
        cb_data: (buffer.len() * 2) as u32,
        lp_data: buffer.as_mut_ptr() as *mut std::ffi::c_void,
    };
    unsafe {
        SendMessageW(
            hwnd,
            WM_COPYDATA,
            0,
            &cds as *const CopyDataStruct as isize,
        ) != 0
    }
}

// ---------------------------------------------------------------------------
// 独立传输窗口：摆位置 / 置顶
//
// 之前这个窗口一出来就在系统默认位置 (182,182)，谁在前面谁盖住它。现在：
//   * 第一次出现贴「鼠标所在那块屏」的工作区右下角（距边 24px，不压任务栏）；
//   * 用户拖过就记住坐标（config.ini 的 xfer_win_x / xfer_win_y），下次照办；
//   * 记住的坐标落到屏幕外（拔了副屏、改了分辨率）就回落右下角；
//   * 一直置顶（HWND_TOPMOST）。
//
// hwnd 不能靠标题找：「正在传输 / 已完成」会来回切，切换那一帧正好找不到。
// 所以按「本进程 + 可见 + 有标题 + 标题不是主窗口」来认。
// ---------------------------------------------------------------------------

/// 句柄在调用方只当不透明整数传，省得 gui_slint.rs 也去依赖 windows-sys 的类型
pub type HwndVal = isize;

/// 工作区（不含任务栏），物理像素：(left, top, right, bottom)
pub type WorkArea = (i32, i32, i32, i32);

fn monitor_work_area(point: POINT, nearest: bool) -> Option<WorkArea> {
    let flags = if nearest {
        MONITOR_DEFAULTTONEAREST
    } else {
        MONITOR_DEFAULTTONULL
    };
    unsafe {
        let monitor = MonitorFromPoint(point, flags);
        if monitor.is_null() {
            return None;
        }
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(monitor, &mut info) == 0 {
            return None;
        }
        Some((
            info.rcWork.left,
            info.rcWork.top,
            info.rcWork.right,
            info.rcWork.bottom,
        ))
    }
}

/// 鼠标所在那块屏的「可用高度」（工作区，物理像素，已扣掉任务栏）。
///
/// 给传输窗口做高度封顶用：块一多，窗口就跟着长，真机上直接长出屏幕顶。
/// 为什么按鼠标所在的那块屏算：窗口第一次出现就是贴鼠标所在屏的右下角
/// （见 transfer_window_origin），而且这里调用时窗口的 hwnd 还没建出来、
/// 量不到它自己在哪块屏上。
///
/// 只读几何，不碰任何摆位 / 吸附逻辑。
pub fn cursor_work_area_height() -> Option<i32> {
    let mut cursor = POINT { x: 0, y: 0 };
    if unsafe { GetCursorPos(&mut cursor) } == 0 {
        return None;
    }
    let (_, top, _, bottom) = monitor_work_area(cursor, true)?;
    let h = bottom - top;
    if h > 0 {
        Some(h)
    } else {
        None
    }
}

/// 窗口该出现在哪（物理像素，左上角）
///
/// saved = 用户上次拖到的位置。它必须整块还在某块屏幕上（中心点落在屏幕内就行，
/// 允许露一半在边外），否则当成「副屏被拔了」，回落到鼠标所在屏的右下角。
pub fn transfer_window_origin(win_w: i32, win_h: i32, saved: Option<(i32, i32)>) -> (i32, i32) {
    // 距工作区边缘的距离：资源管理器那个弹窗也是这个量级，不贴死
    const MARGIN: i32 = 24;
    if let Some((x, y)) = saved {
        let center = POINT {
            x: x + win_w / 2,
            y: y + win_h / 2,
        };
        if monitor_work_area(center, false).is_some() {
            return (x, y);
        }
    }
    let mut cursor = POINT { x: 0, y: 0 };
    unsafe {
        GetCursorPos(&mut cursor);
    }
    let (left, top, right, bottom) =
        monitor_work_area(cursor, true).unwrap_or((0, 0, 1920, 1080));
    let x = (right - win_w - MARGIN).max(left);
    let y = (bottom - win_h - MARGIN).max(top);
    (x, y)
}

// 下面这一组（FindWindow / find_window_proc / find_transfer_window）是「本进程第一个带标题的
// 窗口」那套找法，一台设备一个传输窗口之后**不能再用**：EnumWindows 永远先命中最早建出来的
// 那个，第二台设备的窗口永远摆不上位置。留着是为了排查时对照，标记 dead_code 免得报警。
#[allow(dead_code)]
struct FindWindow {
    pid: u32,
    main_title: String,
    /// 另外几个「不是传输窗口」的标题（推送小窗、取消确认框都排在传输窗口前面被枚举到）
    skip: Vec<String>,
    found: HWND,
}

#[allow(dead_code)]
unsafe extern "system" fn find_window_proc(hwnd: HWND, lparam: LPARAM) -> i32 {
    let state = &mut *(lparam as *mut FindWindow);
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, &mut pid);
    if pid != state.pid {
        return 1;
    }
    if IsWindowVisible(hwnd) == 0 {
        return 1;
    }
    let len = GetWindowTextLengthW(hwnd);
    if len <= 0 {
        return 1;
    }
    let mut buffer = vec![0u16; len as usize + 1];
    let n = GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32);
    if n <= 0 {
        return 1;
    }
    let title = String::from_utf16_lossy(&buffer[..n as usize]);
    // 主窗口、托盘窗口（无标题）、推送小窗都不是我们要的
    if title == state.main_title || state.skip.iter().any(|t| *t == title) {
        return 1;
    }
    state.found = hwnd;
    0 // 找到了就停
}

/// 本进程里那个独立的传输窗口；还没建出来（窗口刚 show）时返回 None
///
/// `skip_titles` 是「别当成传输窗口」的标题。进程里现在不止三个窗口了
/// （主界面 / 传输窗口 / 取消确认框 / 推送小窗），谁先被 EnumWindows 枚举到不一定，
/// 不把推送小窗排掉，传输窗口的落位 / 置顶就会作用到小窗身上。
#[allow(dead_code)]
pub fn find_transfer_window(main_title: &str, skip_titles: &[&str]) -> Option<HwndVal> {
    let mut state = FindWindow {
        pid: std::process::id(),
        main_title: main_title.to_string(),
        skip: skip_titles.iter().map(|t| t.to_string()).collect(),
        found: std::ptr::null_mut(),
    };
    unsafe {
        EnumWindows(Some(find_window_proc), &mut state as *mut FindWindow as LPARAM);
    }
    if state.found.is_null() {
        None
    } else {
        Some(state.found as isize)
    }
}

/// 枚举用的状态：按「标题后缀」认窗口。
///
/// 每台设备一个传输窗口之后，按标题找就不能再用「本进程第一个带标题的窗口」那套了 ——
/// EnumWindows 永远先命中最早建出来的那个，第二台设备的窗口永远摆不上位置。
/// 于是让每个传输窗口的标题带上设备名（"正在传输 · 小米 15"），这里按后缀认。
struct FindWindowFor {
    pid: u32,
    suffix: String,
    found: HWND,
}

unsafe extern "system" fn find_window_for_proc(hwnd: HWND, lparam: LPARAM) -> i32 {
    let state = &mut *(lparam as *mut FindWindowFor);
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, &mut pid);
    if pid != state.pid {
        return 1;
    }
    if IsWindowVisible(hwnd) == 0 {
        return 1;
    }
    let len = GetWindowTextLengthW(hwnd);
    if len <= 0 {
        return 1;
    }
    let mut buffer = vec![0u16; len as usize + 1];
    let n = GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32);
    if n <= 0 {
        return 1;
    }
    let title = String::from_utf16_lossy(&buffer[..n as usize]);
    if !title.ends_with(&state.suffix) {
        return 1;
    }
    state.found = hwnd;
    0
}

/// 某一台设备的传输窗口（标题以 `suffix` 结尾，例如 " · 小米 15"）。
///
/// 标题里的状态前缀是会变的（正在传输 / 已完成 / 接收失败），所以只认后缀，
/// 调用方在窗口刚 show 出来那几帧反复调它，哪个状态第一次命中就是它。
pub fn find_transfer_window_of(suffix: &str) -> Option<HwndVal> {
    let mut state = FindWindowFor {
        pid: std::process::id(),
        suffix: suffix.to_string(),
        found: std::ptr::null_mut(),
    };
    unsafe {
        EnumWindows(Some(find_window_for_proc), &mut state as *mut FindWindowFor as LPARAM);
    }
    if state.found.is_null() {
        None
    } else {
        Some(state.found as isize)
    }
}

/// 按标题找一个窗口（本进程自己建的，标题是唯一的）。
///
/// 和 `find_transfer_window` 的区别：那个是「除了主界面之外的第一个窗口」，
/// 现在进程里有主界面 / 传输窗口 / 取消确认框三个，谁先被枚举到不一定，
/// 所以要能按标题精确找。
pub fn find_window_titled(title: &str) -> Option<HwndVal> {
    let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    let h = unsafe { FindWindowW(std::ptr::null(), wide.as_ptr()) };
    if h.is_null() {
        None
    } else {
        Some(h as isize)
    }
}

/// 置顶 + 抢到前台（要给用户点的窗口才用这个）。
///
/// 为什么必须抢前台：传输窗口自己是 HWND_TOPMOST 的（§6.5），取消确认框如果不跟着置顶，
/// 用户看着框在前、鼠标点下去命中的却是后面那个置顶的传输窗口 ——
/// 真机 2026-11-19 撞见：点「保留进度」没反应，传输照跑到 100%。
pub fn bring_to_front(hwnd: HwndVal) {
    unsafe {
        SetWindowPos(
            hwnd as HWND,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE,
        );
        SetForegroundWindow(hwnd as HWND);
    }
}

/// 置顶（不改位置、不改大小、不抢焦点）
pub fn set_topmost(hwnd: HwndVal) {
    unsafe {
        SetWindowPos(
            hwnd as HWND,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

/// 窗口「看得见的那块」在屏幕上的位置（物理像素）：(left, top, right, bottom)。
///
/// GetWindowRect 会带上 DWM 那圈看不见的缩放边框（100% 缩放下左右下各约 8px），
/// 按它算「贴右下角」会往任务栏里压进去一截，所以改用 DWMWA_EXTENDED_FRAME_BOUNDS。
pub fn window_visible_rect(hwnd: HwndVal) -> Option<WorkArea> {
    const DWMWA_EXTENDED_FRAME_BOUNDS: u32 = 9;
    unsafe {
        let mut rect: windows_sys::Win32::Foundation::RECT = std::mem::zeroed();
        let hr = DwmGetWindowAttribute(
            hwnd as HWND,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut rect as *mut _ as *mut std::ffi::c_void,
            std::mem::size_of::<windows_sys::Win32::Foundation::RECT>() as u32,
        );
        if hr < 0 {
            return None;
        }
        Some((rect.left, rect.top, rect.right, rect.bottom))
    }
}

/// 只在「窗口已经探出工作区」时才把它推回来，其余情况一个像素都不动。
///
/// 为什么不用 nudge_to_bottom_right 收这个尾：那个是「吸到右下角」，
/// 用户自己把传输窗口拖到左上角之后，一展开明细又会被弹回右下角 —— 那是抢方向盘。
/// 这里只处理「长得太高/太靠边被屏幕切掉」这一种情况（真机：明细展开后窗口往下长到
/// 屏幕外，后 6 行明细根本看不见）。
pub fn keep_on_screen(hwnd: HwndVal, margin: i32) -> bool {
    let Some((left, top, right, bottom)) = window_visible_rect(hwnd) else {
        return false;
    };
    let center = POINT {
        x: (left + right) / 2,
        y: (top + bottom) / 2,
    };
    let Some((work_left, work_top, work_right, work_bottom)) = monitor_work_area(center, true) else {
        return false;
    };
    let mut dx = 0;
    let mut dy = 0;
    // 先收右边/下边（越界的方向优先），再把左边/上边拉回工作区内
    if right > work_right - margin {
        dx = (work_right - margin) - right;
    }
    if bottom > work_bottom - margin {
        dy = (work_bottom - margin) - bottom;
    }
    if left + dx < work_left {
        dx = work_left - left;
    }
    if top + dy < work_top {
        dy = work_top - top;
    }
    if dx == 0 && dy == 0 {
        return true;
    }
    unsafe {
        let mut outer: windows_sys::Win32::Foundation::RECT = std::mem::zeroed();
        if GetWindowRect(hwnd as HWND, &mut outer) == 0 {
            return false;
        }
        SetWindowPos(
            hwnd as HWND,
            std::ptr::null_mut(),
            outer.left + dx,
            outer.top + dy,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
    true
}

/// 把窗口挪到「可见右下角距工作区右下 margin 像素」的位置。
///
/// 用的是「量一下现在还差多少、再挪多少」的写法，不去猜标题栏和边框有多高：
/// 多显示器时按窗口自己所在那块屏的工作区算（不是主屏），所以副屏也摆得对。
/// 返回 true = 已经量到并处理过。
/// 同 `nudge_to_bottom_right`，但往上再错开 `up` 像素。
///
/// 多设备时会有好几个传输窗口，都贴右下角就会叠在一起、只看得见最上面那个。
/// 第 i 个窗口 `up = i * (窗口高 + 间距)`，就从下往上排开，像资源管理器那样一列。
pub fn nudge_to_bottom_right_up(hwnd: HwndVal, margin: i32, up: i32) -> bool {
    let Some((left, top, right, bottom)) = window_visible_rect(hwnd) else {
        return false;
    };
    let center = POINT {
        x: (left + right) / 2,
        y: (top + bottom) / 2,
    };
    let Some((_, _, work_right, work_bottom)) = monitor_work_area(center, true) else {
        return false;
    };
    let dx = (work_right - margin) - right;
    let dy = (work_bottom - margin - up) - bottom;
    if dx == 0 && dy == 0 {
        return true;
    }
    unsafe {
        let mut outer: windows_sys::Win32::Foundation::RECT = std::mem::zeroed();
        if GetWindowRect(hwnd as HWND, &mut outer) == 0 {
            return false;
        }
        SetWindowPos(
            hwnd as HWND,
            std::ptr::null_mut(),
            outer.left + dx,
            outer.top + dy,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
    true
}

pub fn nudge_to_bottom_right(hwnd: HwndVal, margin: i32) -> bool {
    let Some((left, top, right, bottom)) = window_visible_rect(hwnd) else {
        return false;
    };
    let center = POINT {
        x: (left + right) / 2,
        y: (top + bottom) / 2,
    };
    let Some((_, _, work_right, work_bottom)) = monitor_work_area(center, true) else {
        return false;
    };
    let dx = (work_right - margin) - right;
    let dy = (work_bottom - margin) - bottom;
    if dx == 0 && dy == 0 {
        return true;
    }
    // 当前的外框左上角（SetWindowPos 要的是绝对坐标，不是位移）
    unsafe {
        let mut outer: windows_sys::Win32::Foundation::RECT = std::mem::zeroed();
        if GetWindowRect(hwnd as HWND, &mut outer) == 0 {
            return false;
        }
        SetWindowPos(
            hwnd as HWND,
            std::ptr::null_mut(),
            outer.left + dx,
            outer.top + dy,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
    true
}

/// 窗口当前位置（外框左上角，物理像素）
pub fn window_position(hwnd: HwndVal) -> Option<(i32, i32)> {
    unsafe {
        let mut outer: windows_sys::Win32::Foundation::RECT = std::mem::zeroed();
        if GetWindowRect(hwnd as HWND, &mut outer) == 0 {
            return None;
        }
        Some((outer.left, outer.top))
    }
}

fn window_handle(title: &str) -> HWND {
    let title = wide(title);
    unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) }
}

pub fn hide_main_window(title: &str) {
    let hwnd = window_handle(title);
    if !hwnd.is_null() {
        unsafe {
            ShowWindow(hwnd, SW_HIDE);
        }
    }
}

pub fn show_main_window(title: &str) {
    let hwnd = window_handle(title);
    if !hwnd.is_null() {
        unsafe {
            ShowWindow(hwnd, SW_SHOW);
            SetForegroundWindow(hwnd);
        }
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY => {
            let event = lparam as u32;
            if event == WM_LBUTTONDBLCLK {
                if let Ok(guard) = ON_SHOW.lock() {
                    if let Some(callback) = guard.as_ref() {
                        callback();
                    }
                }
            } else if event == WM_RBUTTONUP {
                let menu = CreatePopupMenu();
                let show = wide("显示主界面");
                let quit = wide("退出");
                AppendMenuW(menu, MF_STRING, CMD_SHOW, show.as_ptr());
                AppendMenuW(menu, MF_STRING, CMD_QUIT, quit.as_ptr());
                let mut point = POINT { x: 0, y: 0 };
                GetCursorPos(&mut point);
                SetForegroundWindow(hwnd);
                let _ = TrackPopupMenu(
                    menu,
                    TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
                    point.x,
                    point.y,
                    0,
                    hwnd,
                    std::ptr::null(),
                );
                DestroyMenu(menu);
            }
            0
        }
        WM_COMMAND => {
            let command = wparam & 0xFFFF;
            if command == CMD_SHOW {
                if let Ok(guard) = ON_SHOW.lock() {
                    if let Some(callback) = guard.as_ref() {
                        callback();
                    }
                }
            } else if command == CMD_QUIT {
                if let Ok(guard) = ON_QUIT.lock() {
                    if let Some(callback) = guard.as_ref() {
                        callback();
                    }
                }
            }
            0
        }
        WM_COPYDATA => {
            let cds = &*(lparam as *const CopyDataStruct);
            if !cds.lp_data.is_null() && cds.cb_data > 0 {
                let len = cds.cb_data as usize / 2;
                let slice = std::slice::from_raw_parts(cds.lp_data as *const u16, len);
                let text = String::from_utf16_lossy(slice);
                let mut target: Option<String> = None;
                let mut paths: Vec<std::path::PathBuf> = Vec::new();
                for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
                    if let Some(rest) = line.strip_prefix("@to=") {
                        target = Some(rest.to_string());
                    } else {
                        paths.push(std::path::PathBuf::from(line));
                    }
                }
                if !paths.is_empty() {
                    hfx_receiver::log::info(&format!(
                        "{}: {}{}",
                        hfx_receiver::i18n::tr("收到待发送文件", "pending files"),
                        paths.len(),
                        target
                            .as_ref()
                            .map(|t| format!(" -> {}", t))
                            .unwrap_or_default()
                    ));
                    hfx_receiver::pending::push_with_target(paths, target);
                }
            }
            1
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 在后台线程创建托盘图标。`show` 在双击图标或选择“显示主界面”时调用。
pub fn init(show: impl Fn() + Send + 'static, quit: impl Fn() + Send + 'static) {
    if let Ok(mut guard) = ON_SHOW.lock() {
        *guard = Some(Box::new(show));
    }
    if let Ok(mut guard) = ON_QUIT.lock() {
        *guard = Some(Box::new(quit));
    }

    std::thread::spawn(|| unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let class_name = wide("HfxTrayWindow");
        let window_name = wide("HybridFileXfer Tray");

        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_name.as_ptr(),
        };
        RegisterClassW(&wc);

        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            window_name.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null_mut(),
        );
        if hwnd.is_null() {
            return;
        }

        let mut data: NOTIFYICONDATAW = std::mem::zeroed();
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = hwnd;
        data.uID = TRAY_ID;
        data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        data.uCallbackMessage = WM_TRAY;
        // 资源 ID 1（build.rs 里用 winres 嵌入的图标）
        data.hIcon = LoadIconW(instance, 1 as *const u16);
        // NOTIFYICONDATAW 是 packed 结构体，不能对它内部字段取引用，整体赋值
        let mut tip = [0u16; 128];
        for (slot, ch) in tip
            .iter_mut()
            .zip("多轨快传 · 等待手机连接".encode_utf16())
        {
            *slot = ch;
        }
        data.szTip = tip;
        Shell_NotifyIconW(NIM_ADD, &data);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        Shell_NotifyIconW(NIM_DELETE, &data);
        DestroyWindow(hwnd);
    });
}
