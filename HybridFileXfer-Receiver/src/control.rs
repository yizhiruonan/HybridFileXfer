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
/// 手机上取消时选了「不保留进度」（CANCEL_DISCARD）：收尾时把这次写到一半的文件
/// 和 `.hfxpart` 续传记录一起删掉，下次同一个文件从头传。
///
/// 用全局原子量而不是给 handle_receive 加参数：取消这一帧可能在任何时候到
/// （传输中、收尾算校验时、甚至传输线程都退出了才到），收到的那一处只管置位，
/// 由收尾的 finish_with 一次性消费。每次接收开始（ReceivingGuard::begin）都会清掉，
/// 不会把上一次的选择带到下一次。
pub static DISCARD_PARTIAL: AtomicBool = AtomicBool::new(false);
/// 「取消接收」这次请求是否**已经受理过**（幂等闸门）。
///
/// 用户可能连点两下「取消 → 保留」：第一下置好取消标志后，接收线程还要退出、
/// 收尾写日志、重建传输通道，这中间 RECEIVING 仍然是 true，界面上那颗「取消」
/// 还看得见。第二下如果照旧置位，就会在「已取消、控制通道正在重建」的状态下
/// 再插一次选择，和手机端正在做的重建握手抢帧序 —— 手机端报
/// `unexpected control frame while waiting for RESULT`，电脑端这边却显示"可续传"。
///
/// 所以第二次点击一律忽略：状态机是 Idle → Requested（一次性），
/// 由 ReceivingGuard（每次接收开始）和 reset() 复位。
static CANCEL_REQUESTED: AtomicBool = AtomicBool::new(false);

/// 这一轮取消的 {@code CANCEL} 帧是否已经写进控制通道（保证只写一帧）。
///
/// 取消要通知手机端，而通知的时机有两处：等待循环里一发现取消（越早越好）、
/// 以及收尾后的 {@code rebuild_transfers}（兜底）。两处都写就成了两帧 CANCEL，
/// 而手机端此时正在做重建握手 —— 多出来的一帧会被它当成"未知指令"或者被
/// 下一个读循环吃走，帧序一乱整条会话就废。用 swap 抢一个名额：谁先谁写。
static CANCEL_FRAME_SENT: AtomicBool = AtomicBool::new(false);

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

/// 电脑端「当前文件」卡片上点「取消」时，用户在确认框里选的这一项。
///
/// 和手机端那套（CANCEL_DISCARD + Receiver::finish_with(keep_incomplete)）是同一份语义，
/// 只是选择权在电脑这边：选项直接写进 DISCARD_PARTIAL，收尾的 finish_with 照旧去读。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CancelChoice {
    /// 保留进度：半截文件 + .hfxpart 续传记录都留着，下次同一个文件接着传
    Keep,
    /// 不保留进度：删掉没传完的文件和续传记录，下次同一个文件从头传
    Discard,
}

impl CancelChoice {
    /// 传给 Receiver::finish_with 的 keep_incomplete 参数
    pub fn keep_incomplete(self) -> bool {
        matches!(self, CancelChoice::Keep)
    }
}

/// 电脑端点了「取消」并在确认框里做了选择：先记下选择，再置取消标志。
///
/// 顺序不能反：选择是收尾（handle_receive 里的 finish_with）才读的，
/// 先写选择再置取消，收尾时一定读得到；反过来会有一瞬间「已经是取消了、
/// 选择还是上一轮的」。
///
/// **幂等**：没有正在进行的接收、或者这次取消已经受理过（收尾 / 重建还没做完
/// 时用户又点了一下），直接返回 false —— 不置任何标志、不写任何帧。
/// 调用方（GUI）据此复用上一次的结果，而不是当成一次新的取消。
pub fn cancel_receive(choice: CancelChoice) -> bool {
    if !receiving() {
        return false;
    }
    if CANCEL_REQUESTED.swap(true, Ordering::Relaxed) {
        return false;
    }
    set_discard_partial(!choice.keep_incomplete());
    cancel();
    true
}

/// 发送方向（电脑 -> 手机：推送，或手机来电脑取文件）点了「取消」并做了选择。
///
/// 和 {@link #cancel_receive} 是同一套幂等闸门、同一套语义，区别只有两处：
///   * 判断的是"正在发送"而不是"正在接收"；
///   * 半截文件在手机那头，所以「不保留」不能只在本机删 —— 收尾时要把这个选择
///     变成一帧 {@code CANCEL_DISCARD} 发给手机，让它删掉自己写了一半的文件
///     （见 session.rs rebuild_transfers）。
pub fn cancel_send(choice: CancelChoice) -> bool {
    if !sending() {
        return false;
    }
    if CANCEL_REQUESTED.swap(true, Ordering::Relaxed) {
        return false;
    }
    set_discard_partial(!choice.keep_incomplete());
    cancel();
    true
}

/// 这次「取消接收」是否已经受理过（正在收尾 / 重建中）。
///
/// 界面用它把「取消」按钮灰掉 / 收起确认框：同一轮接收只允许一次取消。
pub fn cancel_requested() -> bool {
    CANCEL_REQUESTED.load(Ordering::Relaxed)
}

/// 抢"写 CANCEL 帧"的名额：返回 true 表示由调用方写，false 表示这一轮已经写过了。
///
/// 用它保证一轮取消在控制通道上**只有一帧** CANCEL（见 {@link CANCEL_FRAME_SENT}）。
pub fn claim_cancel_frame() -> bool {
    !CANCEL_FRAME_SENT.swap(true, Ordering::SeqCst)
}

pub fn cancelled() -> bool {
    CANCEL_SEND.load(Ordering::Relaxed)
}

pub fn reset() {
    PAUSE_SEND.store(false, Ordering::Relaxed);
    CANCEL_SEND.store(false, Ordering::Relaxed);
    PHONE_PAUSED.store(false, Ordering::Relaxed);
    DISCARD_PARTIAL.store(false, Ordering::Relaxed);
    // 下一轮接收可以再取消一次（幂等闸门复位），并且可以再写一帧 CANCEL
    CANCEL_REQUESTED.store(false, Ordering::Relaxed);
    CANCEL_FRAME_SENT.store(false, Ordering::Relaxed);
}

/// 手机端要求「不保留进度」（收尾时删掉半截文件和续传日志）
pub fn set_discard_partial(on: bool) {
    DISCARD_PARTIAL.store(on, Ordering::Relaxed);
}

/// 本次接收是不是要丢弃半截文件（由 finish_with 消费；ReceivingGuard 结束时清掉）
pub fn discard_partial() -> bool {
    DISCARD_PARTIAL.load(Ordering::Relaxed)
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
#[cfg(test)]
mod tests {
    use super::CancelChoice;

    /// 取消确认框的两个选项 -> Receiver::finish_with 的参数。
    /// 纯函数，不碰全局原子量（同一进程里别的用例也在跑，别去污染它们）
    #[test]
    fn cancel_choice_maps_to_keep_incomplete() {
        assert!(CancelChoice::Keep.keep_incomplete(), "保留进度要传 true");
        assert!(!CancelChoice::Discard.keep_incomplete(), "不保留要传 false");
    }
}
