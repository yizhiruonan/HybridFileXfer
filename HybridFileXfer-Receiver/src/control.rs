//! 发送侧（电脑 → 手机）的暂停 / 中断开关。
//!
//! 接收器是单进程单实例，用全局原子量最省事：界面改，传输线程读。

use std::sync::atomic::{AtomicBool, Ordering};

/// 暂停发送：send_channel 每领一块前检查，暂停时原地等
pub static PAUSE_SEND: AtomicBool = AtomicBool::new(false);
/// 中断本次发送：send_channel 抛错退出，手机端会看到断线
pub static CANCEL_SEND: AtomicBool = AtomicBool::new(false);
/// 当前是否有一条「手机来拉文件」的发送在进行（界面据此显示按钮）
pub static SENDING: AtomicBool = AtomicBool::new(false);
/// 当前是否正在接收手机推过来的文件（界面据此显示暂停按钮）
pub static RECEIVING: AtomicBool = AtomicBool::new(false);
/// 手机端最近一次上报的暂停状态：只有它变了才说明是手机点的暂停，
/// 这时才把电脑本地的暂停开关同步过去（否则电脑自己点的暂停会被手机的状态覆盖）
pub static PHONE_PAUSED: AtomicBool = AtomicBool::new(false);

pub fn pause(on: bool) {
    PAUSE_SEND.store(on, Ordering::Relaxed);
}

pub fn paused() -> bool {
    PAUSE_SEND.load(Ordering::Relaxed)
}

pub fn cancel() {
    PAUSE_SEND.store(false, Ordering::Relaxed);
    CANCEL_SEND.store(true, Ordering::Relaxed);
}

pub fn cancelled() -> bool {
    CANCEL_SEND.load(Ordering::Relaxed)
}

pub fn reset() {
    PAUSE_SEND.store(false, Ordering::Relaxed);
    CANCEL_SEND.store(false, Ordering::Relaxed);
    PHONE_PAUSED.store(false, Ordering::Relaxed);
}

pub fn sending() -> bool {
    SENDING.load(Ordering::Relaxed)
}

pub fn receiving() -> bool {
    RECEIVING.load(Ordering::Relaxed)
}

pub fn phone_paused() -> bool {
    PHONE_PAUSED.load(Ordering::Relaxed)
}

pub fn set_phone_paused(on: bool) {
    PHONE_PAUSED.store(on, Ordering::Relaxed);
}

/// 接收开始时重置开关并置位；离开作用域时清掉 RECEIVING
pub struct ReceivingGuard;

impl ReceivingGuard {
    pub fn begin() -> Self {
        reset();
        RECEIVING.store(true, Ordering::Relaxed);
        ReceivingGuard
    }
}

impl Drop for ReceivingGuard {
    fn drop(&mut self) {
        RECEIVING.store(false, Ordering::Relaxed);
        reset();
    }
}

/// 发送开始时重置开关并置位；离开作用域时清掉 SENDING
pub struct SendingGuard;

impl SendingGuard {
    pub fn begin() -> Self {
        reset();
        SENDING.store(true, Ordering::Relaxed);
        SendingGuard
    }
}

impl Drop for SendingGuard {
    fn drop(&mut self) {
        SENDING.store(false, Ordering::Relaxed);
        reset();
    }
}
