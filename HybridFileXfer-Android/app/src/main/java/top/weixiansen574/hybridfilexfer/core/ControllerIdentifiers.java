package top.weixiansen574.hybridfilexfer.core;

public class ControllerIdentifiers {
    public static final short SHUTDOWN = 0;
    public static final short LIST_FILES = 1;
    public static final short DELETE_FILE = 2;
    public static final short MKDIR = 3;
    public static final short REQUEST_RECEIVE = 10;
    public static final short REQUEST_SEND = 11;
    /**
     * 心跳。手机端在空闲时每隔几秒发送一次，用于发现连接已经断开（例如拔掉数据线）。
     * 客户端遇到不认识的控制指令会直接忽略，因此旧版客户端也能正常工作。
     */
    public static final short PING = 20;
    /**
     * 轮询：电脑有没有要推给手机的文件（本项目新增）。
     * 旧电脑端不认识这条指令会报错，所以只在本机配套版本上使用。
     */
    public static final short CHECK_PENDING = 21;
    /**
     * 暂停状态同步（本项目新增）：手机发 [bool 自己是否暂停]，电脑回 [bool 电脑是否暂停]。
     * 任何一端暂停/继续，另一端下一次同步（约 250ms）就跟上，两边的界面一起显示"已暂停"。
     */
    public static final short PAUSE_STATE = 22;
    /**
     * 302：一次传输的结论帧 {@code [bool ok][UTF 错误信息?]}（成功时不带错误信息）。
     * 两个方向共用：接收方写"落盘结论"，发送方写"读文件结论"。
     * 以前这里是裸 bool 加裸 UTF，收尾期间谁都不能碰控制通道，暂停同步和心跳全被憋住。
     */
    public static final short RESULT = 23;
    /** 302：PAUSE_STATE 的回报帧 {@code [bool 电脑是否暂停]}（以前是裸 bool）。 */
    public static final short PAUSE_REPLY = 24;
    /**
     * 303：手机端主动取消本次传输（用户点了「取消」），无载荷。
     *
     * 以前取消就是直接断 socket，电脑端把 ConnectionReset 当成传输失败报错；
     * 暂停状态下更糟——电脑的传输线程停在暂停等待里不读 socket，根本发现不了
     * 对端已经走了，整条会话卡死在「已暂停」。现在先发这一帧再关，电脑端
     * 就地置取消标志，各通道干净退出、界面上也不报错。
     */
    public static final short CANCEL = 25;
    /**
     * 304：取消后重建传输通道。手机发完 CANCEL 只关传输通道、保留控制通道，
     * 电脑收尾完再连一批新的传输通道，会话不断。
     *
     * <p>两个方向都带 opcode 分帧，不能用裸 bool —— 手机端随时可能在飞 PAUSE_STATE，
     * 裸 bool 会和它的帧头撞车，帧流一错位整条会话就报废。
     *
     * <pre>
     * 电脑 → 手机：{i16 REBUILD_REQ}{UTF 通道名}
     * 手机 → 电脑：{i16 REBUILD_ACK}{bool 是否接受}
     * </pre>
     */
    public static final short REBUILD_REQ = 26;
    public static final short REBUILD_ACK = 27;
    /**
     * 305：断点续传查询（发送方 → 接收方）。
     *
     * <pre>
     * {i32 count}{ (UTF 目标路径, i64 总大小, i64 源修改时间) * count }
     * </pre>
     */
    public static final short RESUME_QUERY = 28;
    /**
     * 305：断点续传应答（接收方 → 发送方）。
     *
     * <pre>
     * {i32 count}{ (bool 可续传, i32 位图字节数, byte[位图]) * count }
     * </pre>
     * 位图第 i 位 = 第 i 块（1MiB）已经落盘。
     */
    public static final short RESUME_REPLY = 29;
    /**
     * 306：手机（接收端）上报"这些文件我已经收到哪些块"，电脑按它只补缺的块。
     *
     * <pre>
     * {i32 count}{ (UTF 目标路径, i64 总大小, i64 源修改时间, i32 位图字节数, byte[位图]) * count }
     * </pre>
     */
    public static final short RESUME_STATE = 30;
    /**
     * 取消传输时用户选了「不保留进度」：让电脑端把这次写到一半的文件和
     * {@code <文件名>.hfxpart} 续传记录一起删掉，下次同一个文件从头传。
     *
     * <p>和 {@link #CANCEL} 一样不带载荷，语义只多一条：电脑端收到它就当取消处理，
     * 收尾时按"丢弃"收尾（{@code Receiver::finish_with(false)}，见 src/session.rs）。
     * 没有为它升协议版本号 —— 旧版本电脑端会把它当成"不认识的控制指令"忽略掉，
     * 取消本身仍然会因为传输通道被关掉而被发现，只是半截文件会被保留
     * （退化成"保留进度"），不会出错；和当初 PING / CHECK_PENDING 一样是纯增量指令。
     */
    public static final short CANCEL_DISCARD = 31;

    //就让魔法打败魔法吧！
    /**
     * 禁止转载应用到酷安
     * 不许未经授权将本项目爬取到国内平台，点名GitCode，除非你的平台容得下下面的数字😁
     */
    private static final int 酷安飞机杯 = 8964;
    /**
     * 你号没了！
     * 当然，如果你看到了有人在酷安分享本软件，请拿此内容进行举报攻击，让他狠狠的享受以下内容！
     * 你的账号【___】，存在发布涉政信息的违规行为，已禁止访问
     */
    private static final String 威武小姐姐 = "学“习”通😋";

}