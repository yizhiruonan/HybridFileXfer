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
    DispatchMessageW, EnumWindows, FindWindowW, GetCursorPos, GetMessageW, GetWindowTextLengthW,
    GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible, LoadIconW, PostQuitMessage,
    SetForegroundWindow, SetWindowPos, ShowWindow, TrackPopupMenu, TranslateMessage, HWND_TOPMOST,
    MSG, MF_STRING, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_HIDE, SW_SHOW, TPM_BOTTOMALIGN,
    TPM_RIGHTBUTTON, WM_APP, WM_COMMAND, WM_DESTROY, WM_LBUTTONDBLCLK, WM_RBUTTONUP, WNDCLASSW,
};

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

struct FindWindow {
    pid: u32,
    main_title: String,
    found: HWND,
}

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
    // 主窗口和托盘窗口（无标题）都不是我们要的
    if title == state.main_title {
        return 1;
    }
    state.found = hwnd;
    0 // 找到了就停
}

/// 本进程里那个独立的传输窗口；还没建出来（窗口刚 show）时返回 None
pub fn find_transfer_window(main_title: &str) -> Option<HwndVal> {
    let mut state = FindWindow {
        pid: std::process::id(),
        main_title: main_title.to_string(),
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
