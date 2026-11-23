//! 手机端「排队中」的任务快照（QUEUE_STATE，opcode 33）。
//!
//! 为什么要留一份登记表：手机上一次选了几十个文件，同一刻只有一个在传，其余的排着队。
//! 界面每 16ms 要画一帧，不可能每帧都去问手机（那是同步 RPC，会把控制通道占住），
//! 所以控制通道一读到 QUEUE_STATE 就把整张表覆盖进这里，界面只读这份。
//!
//! 按设备串号分开存：多设备时每台手机各有一张队列，一台的「等待中」不能串到另一台上。

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::sync::{LazyLock, Mutex};

use crate::proto;

/// 队列里一条任务的状态（线格式里那个 u8）。
pub mod state {
    /// 还在排队、没开始传 —— 界面上画成「等待中」的块
    pub const WAITING: u8 = 0;
    /// 正在传的那一条 —— 界面上就是已有的那个进度块，队列这边不再画第二遍
    pub const RUNNING: u8 = 1;
    /// 已经传完 —— 不显示（电脑端有自己的「已完成」收尾）
    pub const DONE: u8 = 2;
    /// 失败了 —— 也不显示：失败的红字走 RESULT / failure 那一套，两处各画一遍会自相矛盾
    pub const FAILED: u8 = 3;
}

/// 队列里的一条任务：线格式里的 「(UTF 文件名, i64 字节数, u8 状态)」。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueItem {
    /// 手机报来的文件名（可能带路径，界面只取最后一段）
    pub name: String,
    /// 字节数（手机报的大小；排队中的文件在电脑上还不存在）
    pub size: u64,
    /// 状态，取值见 {@link state}
    pub state: u8,
}

/// 串号 → 整张队列快照。
///
/// 用 HashMap 而不是「一台设备一份静态变量」：多设备（run_multi）时几条会话线程
/// 同时写各自的队列，键分开就不会互相踩。
static QUEUES: LazyLock<Mutex<HashMap<String, Vec<QueueItem>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 整表覆盖：手机每报一次就以这一次为准。
///
/// 空表 = 队列空了，直接把这条删掉而不是存一个空 Vec —— 这样「从来没收到过 QUEUE_STATE」
/// 和「收到一帧空表」在读取侧完全等价，界面不必再多判一次。
pub fn set_for(device: &str, items: Vec<QueueItem>) {
    let Ok(mut guard) = QUEUES.lock() else {
        return;
    };
    if items.is_empty() {
        guard.remove(device);
    } else {
        guard.insert(device.to_string(), items);
    }
}

/// 某台设备的整张队列。
///
/// 没有就返回空表、不返回 Option：界面上「没有队列」和「队列是空的」都表现为不画等待块，
/// 多一层 Option 只是让每个调用点多一次解包。
pub fn get_for(device: &str) -> Vec<QueueItem> {
    let Ok(guard) = QUEUES.lock() else {
        return Vec::new();
    };
    guard.get(device).cloned().unwrap_or_default()
}

/// 这台设备排队中（state == 0）的那些任务，顺序就是手机报来的顺序。
///
/// 界面只画这一种：state == 1 的那一条就是正在跑的那个进度块，2 / 3 已经收尾。
pub fn waiting_for(device: &str) -> Vec<QueueItem> {
    get_for(device)
        .into_iter()
        .filter(|item| item.state == state::WAITING)
        .collect()
}

/// 这台设备上手机正在传的那一条（state == 1）。
///
/// 为什么要读它：手机端「多选分享」是**一个文件一个任务**，队列里 state==1 的那条才是
/// 当前在传的。电脑端拿到的进度（PROGRESS）只知道自己收到的字节，不知道"手机认为现在是
/// 哪一个文件在跑"—— 两者对不上时（续传跳过、多任务轮转），界面上的文件名会停在上一个，
/// 用户看到的就是「一直在传第一个文件」。所以渲染那一块时以手机报的这条为准。
///
/// 暂停（PAUSED）在手机上也算 RUNNING（它还占着传输通道），语义一致。
pub fn running_for(device: &str) -> Option<QueueItem> {
    get_for(device)
        .into_iter()
        .find(|item| item.state == state::RUNNING)
}

/// 设备断开 / 会话结束时清掉（下次连上手机重新报，留着只会让界面挂着上一轮的等待块）。
pub fn clear(device: &str) {
    let Ok(mut guard) = QUEUES.lock() else {
        return;
    };
    guard.remove(device);
}

/// 写一帧 QUEUE_STATE 的**载荷**（opcode 由调用方写）。
///
/// 目前只有单测用得上（电脑端是接收方，正常不发这一帧），但和 read_items 配成一对，
/// 往返测试才能验证「编码」和「解码」认的是同一套线格式。
pub fn write_items<W: Write>(w: &mut W, items: &[QueueItem]) -> io::Result<()> {
    proto::write_i32(w, items.len() as i32)?;
    for item in items {
        proto::write_utf(w, &item.name)?;
        proto::write_i64(w, item.size as i64)?;
        proto::write_u8(w, item.state)?;
    }
    Ok(())
}

/// 读一帧 QUEUE_STATE 的载荷（opcode 已由调用方读掉）。
///
/// count 先夹到 0 再分配：负数（对端发来的垃圾 / 帧序错位）按空表处理，
/// 而不是让它变成一个天文数字去申请内存。
pub fn read_items<R: Read>(r: &mut R) -> io::Result<Vec<QueueItem>> {
    let count = proto::read_i32(r)?.max(0) as usize;
    let mut items = Vec::with_capacity(count.min(4096));
    for _ in 0..count {
        let name = proto::read_utf(r)?;
        let size = proto::read_i64(r)?.max(0) as u64;
        let state = proto::read_u8(r)?;
        items.push(QueueItem { name, size, state });
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 单测共用一把锁：这套用例改的是同一份全局登记表，
    /// 并行跑（cargo test 默认多线程）时一台设备的表会被另一条用例覆盖掉。
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn item(name: &str, size: u64, state: u8) -> QueueItem {
        QueueItem {
            name: name.to_string(),
            size,
            state,
        }
    }

    #[test]
    fn payload_round_trips() {
        let _guard = TEST_LOCK.lock();
        let items = vec![
            item("照片/IMG_0001.jpg", 12_345_678, state::WAITING),
            item("影片.mp4", 6_000_000_000, state::RUNNING),
            item("a", 0, state::FAILED),
        ];
        let mut buf: Vec<u8> = Vec::new();
        write_items(&mut buf, &items).unwrap();
        // 逐字节对一遍线格式：`{i32 count}` + `{i16 长度 + 修改版 UTF8}` + `{i64}` + `{u8}`
        assert_eq!(&buf[..4], &[0, 0, 0, 3]);
        // 第一个名字「照片/IMG_0001.jpg」：照片 6 字节 + /IMG_0001.jpg 13 字节 = 19（0x13）
        assert_eq!(&buf[4..6], &[0, 19]);
        assert_eq!(buf[4 + 2 + 19 + 8], state::WAITING);
        assert_eq!(buf[buf.len() - 1], state::FAILED);
        let back = read_items(&mut buf.as_slice()).unwrap();
        assert_eq!(back, items);
        // 空表也要能往返（手机清空队列时发过来的就是它）
        let mut empty: Vec<u8> = Vec::new();
        write_items(&mut empty, &[]).unwrap();
        assert_eq!(empty, vec![0, 0, 0, 0]);
        assert_eq!(read_items(&mut empty.as_slice()).unwrap(), Vec::new());
    }

    #[test]
    fn whole_table_replaces_the_previous_one() {
        let _guard = TEST_LOCK.lock();
        let device = "TEST-SERIAL-REPLACE";
        clear(device);
        set_for(
            device,
            vec![
                item("一.mp4", 10, state::WAITING),
                item("二.mp4", 20, state::WAITING),
            ],
        );
        assert_eq!(get_for(device).len(), 2);
        // 第二条开始传了：整表覆盖，不再是最初那两条
        set_for(device, vec![item("二.mp4", 20, state::RUNNING)]);
        let now = get_for(device);
        assert_eq!(now.len(), 1);
        assert_eq!(now[0].name, "二.mp4");
        assert_eq!(now[0].state, state::RUNNING);
        // 只剩「等待中」的那一份才给界面画
        assert!(waiting_for(device).is_empty());
        // 空表 = 队列空了：连键都不留
        set_for(device, Vec::new());
        assert!(get_for(device).is_empty());
        clear(device);
    }

    /// 「手机说现在在传的是哪一个」只有一个答案，而且必须是 state==1 那一条。
    ///
    /// 界面靠它校准块标题（用户报的「多选后电脑端一直显示第一个文件」）：
    /// 电脑端 PROGRESS 只知道自己收了多少字节，不知道手机把哪个文件算作当前任务。
    #[test]
    fn running_item_is_the_single_track_in_flight() {
        let _guard = TEST_LOCK.lock();
        let device = "TEST-SERIAL-RUNNING";
        clear(device);
        // 手机报的顺序：正在传的在最前（SendTasks.queueStateFor 的排序约定），
        // 但「谁在跑」是按 state 判断的，不是按下标 —— 顺序反了也得认对。
        set_for(
            device,
            vec![
                item("排队一.mp4", 100, state::WAITING),
                item("正在传.mp4", 200, state::RUNNING),
                item("已收尾.mp4", 300, state::DONE),
            ],
        );
        let run = running_for(device).expect("应该能找到正在传的那一条");
        assert_eq!(run.name, "正在传.mp4");
        assert_eq!(run.size, 200);
        // 等待块只有 state==0 的两条里的一条（DONE / RUNNING 都不画）
        let waiting = waiting_for(device);
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].name, "排队一.mp4");
        // 暂停时手机把它报成 RUNNING（它还占着通道）：这里照样认它是"当前这个"
        set_for(device, vec![item("正在传.mp4", 200, state::RUNNING)]);
        assert_eq!(running_for(device).unwrap().name, "正在传.mp4");
        // 全收尾了就没有"正在传"的那一条，界面那块回到本端 PROGRESS 的路径
        set_for(device, vec![item("已收尾.mp4", 300, state::DONE)]);
        assert!(running_for(device).is_none());
        assert!(waiting_for(device).is_empty());
        // 没收到过 QUEUE_STATE 的设备：两份都空，调用方不用再判 Option
        assert!(running_for("TEST-SERIAL-NEVER").is_none());
        clear(device);
    }

    #[test]
    fn devices_do_not_interfere() {
        let _guard = TEST_LOCK.lock();
        let a = "TEST-SERIAL-A";
        let b = "TEST-SERIAL-B";
        clear(a);
        clear(b);
        set_for(a, vec![item("a.mp4", 1, state::WAITING)]);
        set_for(
            b,
            vec![item("b.mp4", 2, state::RUNNING), item("b2.mp4", 3, state::WAITING)],
        );
        assert_eq!(get_for(a), vec![item("a.mp4", 1, state::WAITING)]);
        assert_eq!(get_for(b).len(), 2);
        assert_eq!(waiting_for(a).len(), 1);
        assert_eq!(waiting_for(b).len(), 1);
        assert_eq!(waiting_for(b)[0].name, "b2.mp4");
        // 清掉一台不能把另一台也带走
        clear(a);
        assert!(get_for(a).is_empty());
        assert_eq!(get_for(b).len(), 2);
        clear(b);
        assert!(get_for(b).is_empty());
    }
}
