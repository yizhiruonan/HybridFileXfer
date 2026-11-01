//! 极简 Windows 系统托盘（Shell_NotifyIcon），不引入额外的 UI 框架。

#![cfg(windows)]

use std::sync::Mutex;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, FindWindowW, GetCursorPos, GetMessageW, LoadIconW, PostQuitMessage,
    SetForegroundWindow, ShowWindow, TrackPopupMenu, TranslateMessage, MSG,
    MF_STRING, SW_HIDE, SW_SHOW, TPM_BOTTOMALIGN, TPM_RIGHTBUTTON, WM_APP, WM_COMMAND, WM_DESTROY,
    WM_LBUTTONDBLCLK, WM_RBUTTONUP, WNDCLASSW,
};

// windows-sys 0.59 没有导出 RegisterClassW，自己声明
#[link(name = "user32")]
extern "system" {
    fn RegisterClassW(lp_wnd_class: *const WNDCLASSW) -> u16;
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
