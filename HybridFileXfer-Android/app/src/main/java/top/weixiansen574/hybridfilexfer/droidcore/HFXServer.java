package top.weixiansen574.hybridfilexfer.droidcore;

import android.os.RemoteException;

import java.io.EOFException;
import java.io.IOException;
import java.net.InetSocketAddress;
import java.nio.ByteBuffer;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.FutureTask;
import java.util.concurrent.LinkedBlockingDeque;

import top.weixiansen574.async.BackstageTask;
import top.weixiansen574.hybridfilexfer.NativeMemory;
import top.weixiansen574.hybridfilexfer.aidl.IIOService;
import top.weixiansen574.hybridfilexfer.core.ControllerIdentifiers;
import top.weixiansen574.hybridfilexfer.core.FileBlock;
import top.weixiansen574.hybridfilexfer.core.HFXService;
import top.weixiansen574.hybridfilexfer.core.ReadFileCall;
import top.weixiansen574.hybridfilexfer.core.ResumeJournal;
import top.weixiansen574.hybridfilexfer.core.ReceiveFileCall;
import top.weixiansen574.hybridfilexfer.core.SendFileCall;
import top.weixiansen574.hybridfilexfer.core.SpeedMonitorThread;
import top.weixiansen574.hybridfilexfer.core.TransferConnection;
import top.weixiansen574.hybridfilexfer.core.WriteFileCall;
import top.weixiansen574.hybridfilexfer.core.bean.Directory;
import top.weixiansen574.hybridfilexfer.core.bean.RemoteFile;
import top.weixiansen574.hybridfilexfer.core.bean.ServerNetInterface;
import top.weixiansen574.hybridfilexfer.droidcore.callback.StartServerCallback;
import top.weixiansen574.hybridfilexfer.core.callback.TransferFileCallback;
import top.weixiansen574.nio.DataByteChannel;

public class HFXServer extends HFXService {
    /** 所有会话共用的监听套接字：整个手机只绑定一次端口（B 方案：多台电脑连同一台手机）。 */
    private static ServerSocketChannel sharedSocket;
    /** 每个已连接的电脑一个实例，key 为自增连接号。 */
    private static final java.util.Map<Integer, HFXServer> INSTANCES = new java.util.concurrent.ConcurrentHashMap<>();
    private static final java.util.concurrent.atomic.AtomicInteger NEXT_ID = new java.util.concurrent.atomic.AtomicInteger(1);
    private static final String LABELS = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    /**
     * 等一条传输通道的超时。
     *
     * <p>电脑端是先把自己的连接建好、再在控制通道上回「这条连上了」，所以这条连接
     * 正常情况下已经在 accept 队列里，只是等 accept 线程分类（100ms）。12 秒纯粹是浪费：
     * 走蓝牙共享网络时（BNEP 建链慢、经常连上一半就掉），一旦某条通道迟迟到不了，
     * 手机就在这儿干等 12 秒 × 若干条，然后整次握手抛异常、电脑重连 —— 用户看到的就是
     * 「加进蓝牙局域网后要等一分钟才连上」（5 次重试 × 12 秒）。
     *
     * <p>现在只等 3 秒；等了还没有就把这条通道回绝掉，**其余通道照常握手成功**，
     * 迟到的连接由 ServerListener 的 spareTransfers 接住，用不着整盘重来。
     */
    private static final int TRANSFER_WAIT_MS = 3000;
    /**
     * 所有传输通道加起来的等待预算。
     *
     * <p>按条算的 3 秒在最坏情况下会累加（蓝牙 4 条都卡住就是 12 秒），
     * 而正常情况所有连接在 100ms 内就到齐了。整次握手最多为"等通道"花 6 秒，
     * 超过就把剩下的回绝掉，先连上再说话。
     */
    private static final int TRANSFER_WAIT_TOTAL_MS = 6000;

    protected final IIOService ioService;
    protected int remoteFileSystem;
    protected String remoteHomeDir;

    /** 传输通道探测锁：探测期间不允许开始新传输，避免把数据读走 */
    private final Object channelProbeLock = new Object();
    private volatile boolean transferring;
    /**
     * 本会话的控制通道分类器（accept 线程持有）。
     *
     * <p>304：取消后要重建传输通道，而 accept 只能在 accept 线程做，
     * 所以给它留个引用：重建前先在监听器上"挂个号"，让新连进来的静默连接别被当成
     * 上一轮的残留直接关掉（那是电脑刚为重连发起的传输通道）。
     */
    private volatile ServerListener listener;

    private final int id = NEXT_ID.getAndIncrement();
    private volatile String displayName = "";

    public HFXServer(IIOService ioService) {
        this.ioService = ioService;
    }

    public int getId() {
        return id;
    }

    /** 界面显示用的名字："电脑 A"，按连接顺序分配字母。 */
    public String getDisplayName() {
        return displayName.isEmpty() ? "电脑 " + id : displayName;
    }

    /** 控制通道是否还活着（心跳线程据此判断会话是否已失效） */
    public boolean isAlive() {
        DataByteChannel channel = ctChannel;
        return channel != null && channel.isOpen();
    }

    // ------------------------------------------------------------------
    // 共享监听套接字
    // ------------------------------------------------------------------

    /** 只在服务启动时绑定一次；后面的每台电脑都从这个套接字上 accept。 */
    public static synchronized ServerSocketChannel bindShared(int port) throws IOException {
        if (sharedSocket != null && sharedSocket.isOpen()) {
            return sharedSocket;
        }
        ServerSocketChannel channel = ServerSocketChannel.open();
        try {
            channel.bind(new InetSocketAddress(port), 16);
            // 非阻塞：accept 循环要能在没有新连接时定期检查服务是否还在运行
            channel.configureBlocking(false);
        } catch (IOException e) {
            try {
                channel.close();
            } catch (IOException ignored) {
            }
            throw e;
        }
        sharedSocket = channel;
        return channel;
    }

    public static synchronized void closeShared() {
        ServerSocketChannel channel = sharedSocket;
        sharedSocket = null;
        if (channel != null) {
            try {
                channel.close();
            } catch (IOException ignored) {
            }
        }
    }

    public static synchronized boolean isListening() {
        return sharedSocket != null && sharedSocket.isOpen();
    }

    // ------------------------------------------------------------------
    // 多实例注册表（替代原来的 static HFXServer.instance 单例）
    // ------------------------------------------------------------------

    public static synchronized void register(HFXServer server) {
        server.displayName = nextLabel();
        INSTANCES.put(server.id, server);
    }

    private static String nextLabel() {
        java.util.HashSet<Character> used = new java.util.HashSet<>();
        for (HFXServer server : INSTANCES.values()) {
            if (server.displayName.startsWith("电脑 ") && server.displayName.length() > 3) {
                used.add(server.displayName.charAt(3));
            }
        }
        for (int i = 0; i < LABELS.length(); i++) {
            char letter = LABELS.charAt(i);
            if (!used.contains(letter)) {
                return "电脑 " + letter;
            }
        }
        return "电脑 " + (INSTANCES.size() + 1);
    }

    public static void unregister(HFXServer server) {
        if (server != null) {
            INSTANCES.remove(server.id);
        }
    }

    /** 当前所有已握手的连接，按连接顺序排列。 */
    public static List<HFXServer> snapshot() {
        List<HFXServer> list = new ArrayList<>(INSTANCES.values());
        java.util.Collections.sort(list, (a, b) -> Integer.compare(a.id, b.id));
        return list;
    }

    /** 只关心"有没有连接"的地方用这个（单例时代的 getServer()）。 */
    public static HFXServer getAny() {
        List<HFXServer> list = snapshot();
        return list.isEmpty() ? null : list.get(0);
    }

    public static HFXServer getById(int id) {
        return INSTANCES.get(id);
    }

    public static int count() {
        return INSTANCES.size();
    }

    public static void disconnectAll() {
        for (HFXServer server : snapshot()) {
            try {
                server.disconnect();
            } catch (Throwable ignored) {
            }
        }
    }

    // ------------------------------------------------------------------
    // 一次握手
    // ------------------------------------------------------------------

    /**
     * 在共享监听套接字上跑完一次握手：版本 → 网卡列表 → 传输通道 → 缓冲区 → 对方文件系统。
     * 控制通道已经由 {@link ServerListener} 接受并完成协议分类（顺便读掉了 4 字节协议头）。
     * 失败只关闭本次连接，不会动共享的监听套接字，也不影响其它电脑。
     *
     * @return true 表示会话已建立（本实例已注册）
     */
    public boolean startServer(ServerListener listener, ServerListener.PendingControl control,
                               List<ServerNetInterface> interfaceList, int localBufferCount,
                               int remoteBufferCount, StartServerCallback callback) throws IOException {
        DataByteChannel ctChannel = new DataByteChannel(control.socket);
        // 控制通道上的读必须有个上限：帧一旦对不上（残留的 PAUSE_REPLY 被当成目录列表长度之类），
        // 以前会永久卡在 readUTF 里，而且握着 controlLock，会话线程连心跳都发不出去 ——
        // 电脑端 100 秒后判超时断线（实测一次正常传输收尾后界面刷新目录就触发）。
        ctChannel.setReadTimeout(30000);
        this.ctChannel = ctChannel;
        this.listener = listener;
        List<TransferConnection> connections = null;
        boolean success = false;
        try {
            //协议判断（协议头在分类阶段已经读掉）
            byte[] headerBytes = HFXServer.CLIENT_HEADER.getBytes(StandardCharsets.UTF_8);
            if (!Arrays.equals(headerBytes, control.header)) {
                ctChannel.writeBytes("protocol error\n");
                return false;
            }
            //版本判断
            int versionCode = ctChannel.readInt();
            if (versionCode != HFXServer.VERSION_CODE) {
                //返回当前服务端版本信息
                ctChannel.writeBoolean(false);//版本未正确匹配
                ctChannel.writeInt(HFXServer.VERSION_CODE);
                return false;
            }
            ctChannel.writeBoolean(true);//版本正确匹配
            // ---- 307：双端限速协商 ----
            //
            // 位置和电脑端（session.rs）严格对应：电脑在「版本匹配」之后先写它的上限、
            // 再读手机的上限；所以这里读完之后必须马上写自己的，两边才咬得住。
            //
            // 语义：0 = 「我这端不限速」，**不参与 min**。两端各自算 min(非零)，
            // 对**自己那条发送路径**生效：手机发文件用 SpeedLimiter，电脑推文件用它的
            // ratelimit —— 数据从谁那儿流出就由谁限速，上限是同一个协商值。
            int pcLimitMbps = ctChannel.readInt();
            // SpeedLimiter 的配置值只有服务进程从配置文件读出来之后才算数。
            // 读之前 configuredBytesPerSecond 是 0，照报就会把用户设的上限**静默抹掉**
            // （电脑那边看到的是「这台手机不限速」，于是它也不再限制自己）。
            // 实测里服务刚重启的那几秒握手确实报过「不限速」，就是这条路径。
            // 所以配置没就绪时改用持久化配置兜底（ServerService 带 Context 提前灌进来）。
            int phoneLimitMbps = top.weixiansen574.hybridfilexfer.core.SpeedLimiter.isConfiguredKnown()
                    ? (int) (top.weixiansen574.hybridfilexfer.core.SpeedLimiter
                            .getConfiguredBytesPerSecond() / (1024L * 1024L))
                    : configuredLimitFallbackMbps;
            ctChannel.writeInt(phoneLimitMbps);
            int negotiatedMbps = negotiateLimitMbps(phoneLimitMbps, pcLimitMbps);
            top.weixiansen574.hybridfilexfer.core.SpeedLimiter
                    .applyNegotiatedBytesPerSecond(negotiatedMbps * 1024L * 1024L);
            // HFXLog 只有 d()（且只在 DEBUG 包生效），没有 i()。
            // 文案和电脑端 session.rs 里那句对齐，两边日志一拼就能看出协商结果。
            top.weixiansen574.hybridfilexfer.core.HFXLog.d("限速协商：本机 " + limitText(phoneLimitMbps)
                    + "，电脑 " + limitText(pcLimitMbps) + " → 生效 " + limitText(negotiatedMbps));
            ctChannel.writeInt(interfaceList.size());//网卡IP数量
            for (ServerNetInterface netInterface : interfaceList) {
                byte[] address = netInterface.address.getAddress();
                ctChannel.writeUTF(netInterface.name);//网卡名称
                ctChannel.writeByte(address.length);//地址长度（IPv4：4与IPv6：16）
                ctChannel.write(address);//地址
                if (netInterface.clientBindAddress == null) {
                    ctChannel.writeByte(0);//地址长度（null:0）
                } else {
                    byte[] bAddress = netInterface.clientBindAddress.getAddress();
                    ctChannel.writeByte(bAddress.length);//地址长度（IPv4：4与IPv6：16）
                    ctChannel.write(bAddress);//地址
                }
            }
            //连接传输通道
            //允许部分通道失败：对方（电脑）可能连不上某张网卡（比如不在同一个WIFI），
            //此时只要还有一条通道可用就继续，用于"USB + WIFI 同时传输"这种自适应场景。
            // 写时复制：304 取消后重建通道时会在会话线程里往里加，
            // 而 getChannelDetails() / hasDeadChannel() 会在界面线程并发读它
            connections = new CopyOnWriteArrayList<>();
            // 握手期间也要开号：先到、但还不该取的那几条连接要能存住（原来只有"重建"才开号）
            listener.wantTransfers(interfaceList.size());
            long transferBudgetEnd = System.currentTimeMillis() + TRANSFER_WAIT_TOTAL_MS;
            for (int i = 0; i < interfaceList.size(); i++) {
                //对方连接通道是否成功
                boolean succeed = ctChannel.readBoolean();
                //对方所连接通道的名称（由控制器通道发送名称）
                String name = ctChannel.readUTF();
                long nameAt = System.currentTimeMillis();
                top.weixiansen574.hybridfilexfer.core.HFXLog.d("握手：" + name + (succeed ? " 已连" : " 不通"));
                if (succeed) {
                    //从共享套接字上取一条传输通道（别的电脑的控制通道会被暂存，不会认错）
                    long left = Math.min(TRANSFER_WAIT_MS, transferBudgetEnd - System.currentTimeMillis());
                    // 按网段认人：电脑并行连的，先到的未必就是这一条（否则界面上的名字和速率对不上）
                    SocketChannel socket = left <= 0 ? null
                            : listener.takeTransferFor(interfaceList.get(i).address, left);
                    if (socket != null) {
                        java.net.SocketAddress remote = null;
                        try {
                            remote = socket.getRemoteAddress();
                        } catch (Exception ignored) {
                        }
                        top.weixiansen574.hybridfilexfer.core.HFXLog.d("握手：" + name + " 传输通道 已到（远端 "
                                + remote + "，等了 " + (System.currentTimeMillis() - nameAt) + "ms）");
                    } else {
                        top.weixiansen574.hybridfilexfer.core.HFXLog.d("握手：" + name + " 传输通道 等超时，等了 "
                                + (System.currentTimeMillis() - nameAt) + "ms");
                    }
                    if (socket == null) {
                        // 只回绝这一条，别把整次握手带崩：走蓝牙时经常有一条通道卡住，
                        // 以前会让电脑重连，重连又卡在同一条上，一分钟都连不进来
                        ctChannel.writeBoolean(false);
                        callback.onAcceptFailed(name);
                        continue;
                    }
                    connections.add(new TransferConnection(name, new DataByteChannel(socket)));
                    ctChannel.writeBoolean(true);
                    callback.onAccepted(name);
                } else {
                    ctChannel.writeBoolean(false);
                    callback.onAcceptFailed(name);
                }
            }
            // 手头的通道收完了，剩下没主的连接（电脑多连了、或网段对不上）全部关掉
            listener.endWantTransfers();
            if (connections.isEmpty()) {
                //一条都没连上，本次握手放弃；监听还在，电脑会重连
                return false;
            }

            LinkedBlockingDeque<ByteBuffer> buffers = this.buffers;
            //告知对方创建的缓冲区块数量
            ctChannel.writeInt(remoteBufferCount);
            if (!ctChannel.readBoolean()) {
                callback.onPcOOM();
                return false;
            }
            //创建缓冲区块
            for (int i = 0; i < localBufferCount; i++) {
                ByteBuffer buffer = NativeMemory.allocateLargeBuffer(FileBlock.BLOCK_SIZE);
                if (buffer != null) {
                    buffers.add(buffer);
                } else {
                    ctChannel.writeBoolean(false);
                    callback.onMeOOM(i, localBufferCount);
                    return false;
                }
            }
            ctChannel.writeBoolean(true);
            //读取对方文件系统信息
            this.remoteFileSystem = ctChannel.readInt();
            //读取对方设定的主目录
            this.remoteHomeDir = ctChannel.readUTF();
            this.connections = connections;
            register(this);
            success = true;
            callback.onConnectSuccess();
            return true;
        } finally {
            if (!success) {
                //只清理本次连接的资源，共享监听套接字继续用
                unregister(this);
                releaseBuffers();
                closeConnections(connections);
                this.connections = null;
                try {
                    ctChannel.close();
                } catch (IOException ignored) {
                }
                this.ctChannel = null;
            }
        }
    }

    private static void closeConnections(List<TransferConnection> connections) {
        if (connections == null) {
            return;
        }
        for (TransferConnection connection : connections) {
            try {
                connection.close();
            } catch (IOException ignored) {
            }
        }
    }

    private void releaseBuffers() {
        for (ByteBuffer buffer : buffers) {
            NativeMemory.freeBuffer(buffer);
        }
        buffers.clear();
    }

    public void disconnect(BackstageTask.BaseEventHandler callback) {
        new DisconnectTask(callback, this).execute();
    }

    public int getRemoteFileSystem() {
        return remoteFileSystem;
    }

    /** 电脑的局域网 IP；adb 转发时是 127.0.0.1，会返回 null。 */
    public String getPeerIp() {
        try {
            if (ctChannel != null) {
                java.net.InetSocketAddress addr = ctChannel.getRemoteAddress();
                if (addr != null && addr.getAddress() != null && !addr.getAddress().isLoopbackAddress()) {
                    return addr.getAddress().getHostAddress();
                }
            }
            List<TransferConnection> snapshot = connections;
            if (snapshot != null) {
                for (TransferConnection connection : snapshot) {
                    if (connection == null || connection.channel == null || !connection.channel.isOpen()) {
                        continue;
                    }
                    java.net.InetSocketAddress addr = connection.channel.getRemoteAddress();
                    if (addr != null && addr.getAddress() != null && !addr.getAddress().isLoopbackAddress()) {
                        return addr.getAddress().getHostAddress();
                    }
                }
            }
        } catch (Exception ignored) {
        }
        return null;
    }

    /** 每个已连接通道的实时信息："名称\t对端地址" */
    public java.util.List<String> getChannelDetails() {
        java.util.ArrayList<String> details = new java.util.ArrayList<>();
        List<TransferConnection> snapshot = connections;
        try {
            if (snapshot != null) {
                for (TransferConnection connection : snapshot) {
                    if (connection == null || connection.channel == null || !connection.channel.isOpen()) {
                        continue;
                    }
                    java.net.InetSocketAddress remote = connection.channel.getRemoteAddress();
                    String address = remote == null ? "-" : remote.getAddress().getHostAddress() + ":" + remote.getPort();
                    details.add(connection.iName + "\t" + address);
                }
            }
        } catch (Exception ignored) {
        }
        return details;
    }

    /**
     * 问电脑有没有待推送的文件：回 数量 + (路径, 大小)*
     */
    public List<RemoteFile> checkPending() throws IOException {
        synchronized (controlLock) {
            // 上一轮传输留下的 PAUSE_REPLY 先吃掉，别把它当成"待推送文件数量"
            drainStaleControlFrames();
            ctChannel.writeShort(ControllerIdentifiers.CHECK_PENDING);
            skipStaleFramesBeforeReply();
            int count = ctChannel.readInt();
            List<RemoteFile> files = new ArrayList<>();
            for (int i = 0; i < count; i++) {
                String path = ctChannel.readUTF();
                long size = ctChannel.readLong();
                int cut = Math.max(path.lastIndexOf('/'), path.lastIndexOf('\\'));
                String name = cut >= 0 ? path.substring(cut + 1) : path;
                files.add(new RemoteFile(name, path, System.currentTimeMillis(), size, false));
            }
            return files;
        }
    }

    public String getRemoteHomeDir() {
        return remoteHomeDir;
    }

    /**
     * 305：断点续传查询。
     *
     * <p>把这次要发的文件（目标路径 + 大小 + 修改时间）发给电脑，电脑按它自己的
     * {@code <文件名>.hfxpart} 位图回答"已经收到哪些块"。返回 目标路径 → 位图。
     *
     * <p>必须在电脑端进入 handle_receive 之后立刻发（它就是先读这一帧的）。
     */
    private java.util.Map<String, java.util.BitSet> queryResume(List<RemoteFile> files,
                                                               Directory localDir, Directory remoteDir) throws IOException {
        java.util.List<String> targets = new java.util.ArrayList<>();
        java.util.List<RemoteFile> plain = new java.util.ArrayList<>();
        for (RemoteFile file : files) {
            if (file.isDirectory()) {
                continue;
            }
            plain.add(file);
            targets.add(localDir.generateTransferPath(file.getPath(), remoteDir));
        }
        java.util.Map<String, java.util.BitSet> result = new java.util.HashMap<>();
        synchronized (controlLock) {
            // 先把上一轮留下的 PAUSE_REPLY 吃掉，别把它当成位图的长度
            drainStaleControlFrames();
            // 电脑端读完 REQUEST_RECEIVE 就同步读这一帧，所以「一个文件都不查」（只发文件夹）
            // 也必须发出去，只是 count = 0
            ctChannel.writeShort(ControllerIdentifiers.RESUME_QUERY);
            ctChannel.writeInt(plain.size());
            for (int i = 0; i < plain.size(); i++) {
                ctChannel.writeUTF(targets.get(i));
                ctChannel.writeLong(plain.get(i).getSize());
                ctChannel.writeLong(plain.get(i).lastModified());
            }
            // 回复是带 opcode 的帧：{i16 RESUME_REPLY}{i32 count}{...}
            short opcode = ctChannel.readShort();
            if (opcode != ControllerIdentifiers.RESUME_REPLY) {
                throw new IOException("expected RESUME_REPLY, got " + opcode);
            }
            int count = ctChannel.readInt();
            for (int i = 0; i < count; i++) {
                boolean canResume = ctChannel.readBoolean();
                int length = ctChannel.readInt();
                int safe = Math.max(0, Math.min(length, 4 * 1024 * 1024));
                byte[] bits = new byte[safe];
                if (safe > 0) {
                    ctChannel.readFully(bits);
                }
                if (canResume && i < targets.size() && safe > 0) {
                    java.util.BitSet set = java.util.BitSet.valueOf(bits);
                    if (!set.isEmpty()) {
                        result.put(targets.get(i), set);
                    }
                }
            }
        }
        return result;
    }

    public void sendFilesToRemote(List<RemoteFile> files, Directory localDir, Directory remoteDir, TransferFileCallback callback) throws IOException {
        // 进度条的账本从这一刻起归这次传输：界面 50ms 读一次，别让它在等重建的几秒里
        // 先把上一轮的分段/已完成字节画出来（那几秒正是"看着像在传别的东西"的时段）
        sendProgress.reset();
        blockProgress.reset();
        top.weixiansen574.hybridfilexfer.core.HFXService.resetVerifyProgress();
        // 304：刚取消过的话先等电脑把通道重建回来（重建要 1~3 秒），
        // 否则用的还是已经关掉的旧通道，用户"取消完立刻再传"必定失败
        awaitRebuild(10000);
        beginTransfer();
        try {
            // 「请求接收」和「续传查询」必须在<b>同一次</b>控制通道占用里发出去。
            //
            // 电脑端的 handle_receive 读完 REQUEST_RECEIVE 就紧接着同步读 RESUME_QUERY；
            // 以前这两帧分两次拿锁，中间会话线程（每 250ms 一次暂停同步）会插进一帧
            // PAUSE_STATE —— 电脑端读到的第一帧成了 22，直接报
            // "expected RESUME_QUERY, got 22" 并把整条会话断开（实测踩到过）。
            java.util.Map<String, java.util.BitSet> resume;
            synchronized (controlLock) {
                ctChannel.writeShort(ControllerIdentifiers.REQUEST_RECEIVE);//请求对方接收
                // 305：问电脑"这些文件已经收到哪些块了"，能续传的就只补缺的块
                resume = queryResume(files, localDir, remoteDir);
            }
            sendFiles(files, localDir, remoteDir, callback, resume);
        } finally {
            endTransfer();
        }
    }

    /** 306：一个文件的续传信息（目标路径 + 大小 + 修改时间 + 位图） */
    private static final class ResumeEntry {
        final String target;
        final long size;
        final long mtime;
        final java.util.BitSet bits;

        ResumeEntry(String target, long size, long mtime, java.util.BitSet bits) {
            this.target = target;
            this.size = size;
            this.mtime = mtime;
            this.bits = bits;
        }
    }

    /**
     * 306：这次要拉的文件里，手机本地已经收到一部分的（按 <文件名>.hfxpart 日志判断）。
     *
     * <p>只处理"顶层文件"：目录里的子文件的目标路径要靠电脑端遍历才能算出来，
     * 这里不做，所以选文件夹拉取时不会续传（单文件拉取是最常见的场景）。
     */
    private java.util.List<ResumeEntry> buildResumeState(List<RemoteFile> files, Directory localDir) {
        java.util.List<ResumeEntry> result = new java.util.ArrayList<>();
        for (RemoteFile file : files) {
            if (file.isDirectory() || file.getSize() <= 0) {
                continue;
            }
            String target = localDir.path + file.getName();
            ResumeJournal journal = ResumeJournal.load(target, file.getSize(),
                    file.lastModified(), FileBlock.BLOCK_SIZE);
            if (journal == null || journal.blocksDone() == 0) {
                continue;
            }
            result.add(new ResumeEntry(target, file.getSize(), file.lastModified(), journal.bitset()));
            top.weixiansen574.hybridfilexfer.core.HFXLog.d("resume: phone already has "
                    + journal.blocksDone() + " block(s) of " + target);
        }
        return result;
    }

    /** 306：把"我已经有哪些块"发给电脑（即使一条都没有也要发，电脑在等这一帧） */
    private void sendResumeState(java.util.List<ResumeEntry> resume) throws IOException {
        synchronized (controlLock) {
            ctChannel.writeShort(ControllerIdentifiers.RESUME_STATE);
            ctChannel.writeInt(resume.size());
            for (ResumeEntry entry : resume) {
                // 位图按"整字节"发（BitSet.toByteArray 会去掉尾部 0 字节，长度按它算就行）
                byte[] bits = entry.bits.toByteArray();
                long blocks = (entry.size + FileBlock.BLOCK_SIZE - 1) / FileBlock.BLOCK_SIZE;
                int needed = (int) ((blocks + 7) / 8);
                if (bits.length < needed) {
                    byte[] grown = new byte[needed];
                    System.arraycopy(bits, 0, grown, 0, bits.length);
                    bits = grown;
                }
                ctChannel.writeUTF(entry.target);
                ctChannel.writeLong(entry.size);
                ctChannel.writeLong(entry.mtime);
                ctChannel.writeInt(bits.length);
                if (bits.length > 0) {
                    ctChannel.writeFully(bits);
                }
            }
        }
    }

    private static java.util.Map<String, java.util.BitSet> toResumeMap(java.util.List<ResumeEntry> entries) {
        java.util.Map<String, java.util.BitSet> map = new java.util.HashMap<>();
        for (ResumeEntry entry : entries) {
            if (!entry.bits.isEmpty()) {
                map.put(entry.target, entry.bits);
            }
        }
        return map;
    }

    public void sendFilesToShelf(List<RemoteFile> files, Directory localDir, Directory remoteDir, TransferFileCallback callback) throws IOException {
        // 同上：这次传输的进度账本从现在开始
        sendProgress.reset();
        blockProgress.reset();
        top.weixiansen574.hybridfilexfer.core.HFXService.resetVerifyProgress();
        // 304：同上，等重建完成再开始
        awaitRebuild(10000);
        beginTransfer();
        try {
            // 306：这些文件我已经收到哪些块（纯本地 IO，放在控制锁外面算，别拖住会话线程）
            java.util.List<ResumeEntry> resume = buildResumeState(files, localDir);
            // 同 sendFilesToRemote：请求帧和续传状态帧必须一次发完，中间不能插进 PAUSE_STATE，
            // 否则电脑端 handle_send 读到的就是错位的帧。
            synchronized (controlLock) {
                ctChannel.writeShort(ControllerIdentifiers.REQUEST_SEND);
                ctChannel.writeInt(files.size());
                for (RemoteFile file : files) {
                    ctChannel.writeUTF(file.getPath());
                }
                ctChannel.writeUTF(localDir.path);
                ctChannel.writeInt(localDir.fileSystem);
                ctChannel.writeUTF(remoteDir.path);
                // 电脑看完 RESUME_STATE 才开始发，只补缺的块
                sendResumeState(resume);
            }
            receiveFiles(callback, toResumeMap(resume));
        } finally {
            endTransfer();
        }
    }

    /** 当前这条连接是不是正在传输（会话线程据此决定要不要做会阻塞的控制指令） */
    public boolean isTransferring() {
        return transferring;
    }

    /** 当前有多少条传输通道（含已经被取消关掉的，重建时按它算要恢复几条） */
    public int getTransferCount() {
        List<TransferConnection> list = connections;
        return list == null ? 0 : list.size();
    }

    // ------------------------------------------------------------------
    // 304：取消后重建传输通道
    // ------------------------------------------------------------------

    /**
     * 开始重建：记下取消前有几条通道，并把已经关掉的通道从表里摘掉。
     *
     * <p>列表换成写时复制：界面线程（getChannelDetails / getChannelNames）会并发读它。
     *
     * @return 取消前的通道数（重建的目标条数）
     */
    public synchronized int beginRebuild() {
        List<TransferConnection> old = connections;
        CopyOnWriteArrayList<TransferConnection> live = new CopyOnWriteArrayList<>();
        int count = 0;
        if (old != null) {
            for (TransferConnection connection : old) {
                if (connection == null) {
                    continue;
                }
                count++;
                // 只留下"确实还能用"的通道：电脑端取消时我们会在 cancelByPeer 里把
                // 通道 markCancelled + close，这里必须把它们摘掉 —— 以前电脑端取消
                // 没人关通道，重建后这批旧 socket 也留在表里，6 条变 12 条，
                // 下一次发送一半线程一上来就写失败（用户报的"再取消一次手机端报错"）。
                if (connection.channel != null && connection.channel.isOpen()
                        && !connection.isCancelled()) {
                    live.add(connection);
                }
            }
        }
        connections = live;
        return count;
    }

    /** 重建时收下一条新的传输通道 */
    public synchronized void addRebuiltConnection(String name, SocketChannel socket) throws IOException {
        List<TransferConnection> list = connections;
        if (list == null) {
            list = new CopyOnWriteArrayList<>();
            connections = list;
        }
        list.add(new TransferConnection(name, new DataByteChannel(socket)));
    }

    /** 回一帧 REBUILD_ACK：这条通道收不收（收不下来时电脑会跳过它） */
    public void writeRebuildAck(boolean accepted) throws IOException {
        synchronized (controlLock) {
            byte[] frame = new byte[3];
            frame[0] = (byte) (ControllerIdentifiers.REBUILD_ACK >> 8);
            frame[1] = (byte) ControllerIdentifiers.REBUILD_ACK;
            frame[2] = (byte) (accepted ? 1 : 0);
            ctChannel.writeFully(frame);
        }
    }

    /**
     * 等一帧控制指令的 opcode；超时返回 {@link #CONTROL_TIMEOUT}。
     *
     * <p>只在会话线程、且确认没有别的读者时调用（重建握手期间控制通道归会话线程）。
     * SocketChannel 的阻塞读没有超时，这里临时切成非阻塞自己轮询 —— 否则重建窗口
     * 一到就只能靠关 socket 才能醒过来。
     */
    public short readControlOpcode(int timeoutMs) throws IOException {
        // 整个"读一帧"必须和界面的请求/应答互斥。界面线程在传输/重建期间也会发
        // LIST_FILES / CHECK_PENDING，并且是在 controlLock 里等应答的；
        // 重建循环若不持锁就去读，会把界面的应答帧当成重建帧读走 ——
        // 帧流一错位，两边从此只能读到垃圾（实测 opcode 变成 28513、29299 这种）。
        synchronized (controlLock) {
            return readControlOpcodeLocked(timeoutMs);
        }
    }

    /** 调用方必须已经持有 {@link #controlLock} */
    private short readControlOpcodeLocked(int timeoutMs) throws IOException {
        // 别的读循环提前读到的帧头不能丢
        short pending = pendingOpcode;
        if (pending >= 0) {
            pendingOpcode = -1;
            return pending;
        }
        DataByteChannel channel = ctChannel;
        if (channel == null) {
            throw new IOException("control channel is not connected");
        }
        SocketChannel socketChannel = channel.getSocketChannel();
        if (socketChannel == null) {
            return channel.readShort();
        }
        ByteBuffer buffer = ByteBuffer.allocate(2);
        long deadline = System.currentTimeMillis() + Math.max(0, timeoutMs);
        socketChannel.configureBlocking(false);
        try {
            while (true) {
                int read = socketChannel.read(buffer);
                if (read < 0) {
                    throw new EOFException("control channel closed by peer");
                }
                if (buffer.position() >= 2) {
                    break;
                }
                if (System.currentTimeMillis() >= deadline) {
                    return CONTROL_TIMEOUT;
                }
                try {
                    Thread.sleep(15);
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                    throw new IOException("interrupted while waiting for control frame", e);
                }
            }
        } finally {
            try {
                socketChannel.configureBlocking(true);
            } catch (IOException ignored) {
                // 通道已经断了，下一轮读会报出来
            }
        }
        buffer.flip();
        short opcode = buffer.getShort();
        // 载荷必须在同一个锁里读掉：松手让界面线程去读，帧流立刻错位
        if (opcode == ControllerIdentifiers.REBUILD_REQ) {
            stashRebuildName(channel.readUTF());
        } else if (opcode == ControllerIdentifiers.PAUSE_REPLY) {
            applyPcPause(channel.readBoolean());
        } else if (opcode == ControllerIdentifiers.RESULT) {
            // 电脑端的收尾结论帧可能在我们取消之前一刻已经写出来了（它那时正在算
            // SHA-256 / 等我们的结论，用户随时可能按取消）。重建握手收到它不能当成
            // "帧序错位" 直接断线（实测 opcode 23 就是这么把会话打掉的），
            // 也不能只读两个字节就把载荷留在通道里 —— 那会和下一个帧头拼成假 opcode。
            // 这次传输已经作废，结论帧没人要了，就地读干净丢掉。
            discardResultPayload(channel);
        }
        if (opcode != ControllerIdentifiers.PAUSE_REPLY
                && opcode != ControllerIdentifiers.REBUILD_REQ
                && opcode != ControllerIdentifiers.REBUILD_ACK
                && opcode != ControllerIdentifiers.PING
                && opcode != ControllerIdentifiers.CANCEL) {
            // 帧流错位排查用：把原始两字节打出来（正常帧不会走到这里）
            top.weixiansen574.hybridfilexfer.core.HFXLog.d(String.format(
                    "readControlOpcode: unexpected %d bytes=%02x %02x",
                    (int) opcode, buffer.get(0), buffer.get(1)));
        }
        return opcode;
    }

    /** 读一帧 PAUSE_REPLY 的载荷（电脑是否暂停）并应用 */
    public void readAndApplyPauseReply() throws IOException {
        applyPcPause(ctChannel.readBoolean());
    }

    /** 读一帧 REBUILD_REQ 的通道名 */
    public String readRebuildName() throws IOException {
        return ctChannel.readUTF();
    }

    /**
     * 取消后等电脑把通道重建回来再开始新的传输。
     * 重建一批通道要 1~3 秒（每条都要过 400ms 的连接分类窗口），
     * 不等的话用户"取消完立刻再传"会撞上已经关掉的旧通道，直接失败。
     */
    private void awaitRebuild(long timeoutMs) {
        long deadline = System.currentTimeMillis() + Math.max(0, timeoutMs);
        while (needsRebuild && System.currentTimeMillis() < deadline) {
            try {
                Thread.sleep(50);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
                return;
            }
        }
    }


    /**
     * 取消正在进行的传输：**只关传输通道，控制通道留着**（304 取消不断连）。
     *
     * <p>以前这里连控制通道一起关，于是取消 = 整条会话拆除：手机端回主页、
     * 电脑端报「连接已断开」再重连，取消完还得等一个来回才能再传。
     * 现在：发一帧 CANCEL（让电脑知道这是取消不是断线）→ 只关传输通道 →
     * 置 needsRebuild，会话线程等电脑把通道重建回来，会话本身不断。
     *
     * <p>传输线程还在用缓冲区块，所以这里**不能**像 {@link #disconnect()} 那样释放缓冲：
     * 会把正在读写的 native 内存 free 掉（崩溃）。关掉通道后传输线程会抛
     * AsynchronousCloseException，等会话线程收尾时再统一释放。
     */
    public void abortTransfer() {
        abortTransfer(false);
    }

    /**
     * 取消正在进行的传输，并告诉电脑端「已传的部分要不要保留」。
     *
     * @param discardPartial true = 用户选了「不保留进度」：多发一帧
     *        {@link ControllerIdentifiers#CANCEL_DISCARD} 代替 {@code CANCEL}，电脑端收尾时
     *        把这次写到一半的文件和 {@code .hfxpart} 续传记录一起删掉（下次从头传）；
     *        false = 保留（现在的行为，下次同一个文件续传）。
     */
    public void abortTransfer(boolean discardPartial) {
        top.weixiansen574.hybridfilexfer.core.HFXLog.d("abortTransfer: close transfer channels only, discard="
                + discardPartial, new Exception("abortTransfer caller"));
        transferCancelled = true;
        needsRebuild = true;
        // 取消时把暂停闸门也放开：不放开的话 isTransferPaused() 一直是 true，
        // 会话线程会一直向电脑端上报"手机暂停了"，电脑端就停在暂停里（界面也停在
        // 已暂停）。下一次传输虽然会换新的闸门，但在这之前两边状态是不一致的。
        resumeTransfer();
        // 先给监听器"挂个号"再去通知电脑：电脑收到 CANCEL 会立刻连一批新通道过来，
        // 这期间 accept 线程要是还没被告知"有人在等传输通道"，那批连接会被当成
        // 上一轮的残留直接关掉，重建就永远等不到通道了。
        ServerListener current = listener;
        if (current != null) {
            current.wantTransfers(getTransferCount());
        }
        // 读写 socket 放到工作线程：本方法由界面线程调用（TransferScreen 的取消按钮），
        // 主线程碰网络会直接被 StrictMode 拦下抛 NetworkOnMainThreadException ——
        // 表现就是 CANCEL 一帧都没发出去，电脑端只能靠"通道被关"去猜，取消照样断连。
        List<TransferConnection> list = connections;
        Thread worker = new Thread(() -> {
            DataByteChannel notify = ctChannel;
            if (notify != null && notify.isOpen()) {
                try {
                    // 用户选了「不保留进度」就发 CANCEL_DISCARD：电脑端除了当取消处理，
                    // 收尾时还会把这次的不完整文件和 .hfxpart 续传记录一起删掉。
                    // 两帧都是"不带载荷"，电脑端的帧流不会因此错位。
                    short opcode = discardPartial
                            ? top.weixiansen574.hybridfilexfer.core.ControllerIdentifiers.CANCEL_DISCARD
                            : top.weixiansen574.hybridfilexfer.core.ControllerIdentifiers.CANCEL;
                    synchronized (controlLock) {
                        // DataByteChannel 直接写 socket，没有缓冲，不需要 flush
                        notify.writeShort(opcode);
                    }
                    top.weixiansen574.hybridfilexfer.core.HFXLog.d("abortTransfer: CANCEL written, discard="
                            + discardPartial);
                } catch (Throwable e) {
                    // 电脑可能已经不在了，发失败无所谓；下面照样关传输通道
                    top.weixiansen574.hybridfilexfer.core.HFXLog.d("abortTransfer: CANCEL write failed", e);
                }
            } else {
                top.weixiansen574.hybridfilexfer.core.HFXLog.d("abortTransfer: control channel already closed, skip CANCEL");
            }
            int closedChannels = 0;
            if (list != null) {
                for (TransferConnection connection : list) {
                    try {
                        // 先打标再关：SendFileCall / ReceiveFileCall 的 catch 据此
                        // 把这次的异常当"取消"而不是"通道错误"
                        connection.markCancelled();
                        connection.close();
                        closedChannels++;
                    } catch (IOException ignored) {
                    }
                }
            }
            top.weixiansen574.hybridfilexfer.core.HFXLog.d("abortTransfer: closed " + closedChannels
                    + " transfer channels, wantTransfers pending");
        }, "hfx-cancel");
        worker.start();
        try {
            // 只等一小会儿：关通道本身很快，等不到也不影响后面（会话线程会等 needsRebuild）
            worker.join(1500);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
    }

    /** 暂停当前传输（传输线程每块检查一次，期间不读不写） */
    public void pauseTransfer() {
        pauseGate.pause();
    }

    /** 继续当前传输 */
    public void resumeTransfer() {
        pauseGate.resume();
    }

    public boolean isTransferPaused() {
        return pauseGate.isPaused();
    }

    private void beginTransfer() {
        synchronized (channelProbeLock) {
            // 关键：先把可能正在飞的 exchangePause 收干净。exchangePause 全程持有 controlLock，
            // 拿到它就说明控制通道此刻没有别的读者；否则会话线程会卡在 readShort 里，
            // 和传输线程抢控制帧——两边各吃半帧，帧流整体错位
            // （表现就是电脑端读到 opcode 0、传输通道莫名 EOF）。
            synchronized (controlLock) {
                transferring = true;
                // 传输线程马上要读控制通道（RESULT / PAUSE_REPLY），会话线程从现在起只能写。
                sendingFiles = true;
                // 上一次的"已取消"不能留到这一次，否则真出错时会被当成取消吞掉。
                // peerCancelled 必须一起清：上一轮是电脑端点「取消→保留进度」的话，
                // 这一轮要是真出错，会被这个残留的旗吞成「已取消」，用户永远看不到原因。
                transferCancelled = false;
                peerCancelled = false;
            }
        }
        top.weixiansen574.hybridfilexfer.core.HFXLog.d("beginTransfer: transferring=true, sendingFiles=" + sendingFiles);
    }

    private void endTransfer() {
        synchronized (channelProbeLock) {
            synchronized (controlLock) {
                transferring = false;
                sendingFiles = false;
            }
        }
    }

    /**
     * 有没有已经断掉的传输通道（拔数据线 / 掉 WiFi）。
     *
     * <p>空闲时用非阻塞读探一下：读到 EOF 说明对端已经没了。
     * 真正传输期间不探测（会把数据读走），由传输线程的异常去发现。
     */
    public boolean hasDeadChannel() {
        synchronized (channelProbeLock) {
            if (transferring) {
                return false;
            }
            List<TransferConnection> list = connections;
            if (list == null || list.isEmpty()) {
                return false;
            }
            for (TransferConnection connection : list) {
                if (connection == null || connection.channel == null || !connection.channel.isOpen()) {
                    return true;
                }
                if (isPeerClosed(connection.channel)) {
                    return true;
                }
            }
            return false;
        }
    }

    private boolean isPeerClosed(DataByteChannel channel) {
        SocketChannel socketChannel = channel.getSocketChannel();
        if (socketChannel == null) {
            return false;
        }
        try {
            // 有数据在排队说明通道活着，而且这时候去 read 会把数据吃掉（块头被偷 1 字节，
            // 传输直接错位）。所以先看 available()，只有确实没数据时才用非阻塞读探 EOF。
            if (socketChannel.socket().getInputStream().available() > 0) {
                return false;
            }
            socketChannel.configureBlocking(false);
            try {
                int read = socketChannel.read(ByteBuffer.allocate(1));
                return read == -1;
            } finally {
                socketChannel.configureBlocking(true);
            }
        } catch (Throwable e) {
            // 探测本身出错，当成已经断开
            return true;
        }
    }

    /**
     * 发送一个心跳包。连接断开时这里会抛出异常，调用方据此重新开始监听。
     */
    public void writePing() throws IOException {
        synchronized (controlLock) {
            if (ctChannel == null) {
                throw new IOException("control channel is not connected");
            }
            ctChannel.writeShort(ControllerIdentifiers.PING);
        }
    }

    /**
     * 暂停状态同步（空闲时用）：把手机端当前的暂停状态告诉电脑，并读回电脑端的暂停状态。
     * 电脑端点了暂停/继续，这里就跟着改本地闸门，弹窗也就显示"已暂停"。
     *
     * <p>只在**手机不发送文件**时调用：发送期间控制通道的读取权归发送线程
     * （{@link #writePauseStateOnly} 只写不读，由发送线程读回 PAUSE_REPLY）。
     *
     * @return 电脑端是否暂停
     */
    public boolean exchangePause(boolean phonePaused) throws IOException {
        synchronized (controlLock) {
            if (ctChannel == null) {
                throw new IOException("control channel is not connected");
            }
            if (sendingFiles) {
                // 传输线程已经接管控制通道的读权（beginTransfer 在 controlLock 里置位）。
                // 这里必须重新确认：只看调用方的判断会有窗口——它判完"没传输"的下一纳秒
                // 传输线程就把读权拿走了，两个读者各吃半帧，帧流整体错位。
                writePauseStateLocked(phonePaused);
                return lastPcPaused;
            }
            ctChannel.writeShort(ControllerIdentifiers.PAUSE_STATE);
            ctChannel.writeBoolean(phonePaused);
            // 302：回报带 opcode（以前是裸 bool，传输期间会被结论帧读串）
            short opcode = ctChannel.readShort();
            if (opcode != ControllerIdentifiers.PAUSE_REPLY) {
                // 304：取消 / 重建期间读到"不是暂停回报"的帧是正常的 ——
                // 用户可能刚好在这一刻点了取消（电脑回的是 CANCEL），也可能电脑
                // 已经收尾完、开始发 REBUILD_REQ。这两种都要就地收下，
                // 不能抛异常，否则一次擦肩而过的帧就把整条会话打掉了。
                if (handleUnexpectedControlFrame(opcode)) {
                    return lastPcPaused;
                }
                top.weixiansen574.hybridfilexfer.core.HFXLog.d("exchangePause: BAD opcode=" + opcode + " sendingFiles=" + sendingFiles);
                throw new IOException("unexpected pause reply opcode " + opcode);
            }
            boolean pcPaused = ctChannel.readBoolean();
            applyPcPause(pcPaused);
            return pcPaused;
        }
    }

    /**
     * 302：发送期间只把暂停状态**写**出去（不读回报）。
     *
     * <p>手机发送文件时，{@code HFXService.sendFiles} 的那个线程是控制通道唯一的读者，
     * 会话线程再去读就会把对方的 RESULT 吃掉。这里只写，电脑的回报由发送线程的读循环应用。
     */
    public void writePauseStateOnly(boolean phonePaused) throws IOException {
        synchronized (controlLock) {
            if (ctChannel == null) {
                throw new IOException("control channel is not connected");
            }
            writePauseStateLocked(phonePaused);
        }
    }

    /**
     * 304：暂停回报之外的控制帧，在"取消 / 重建"窗口里是正常现象，就地处理掉。
     *
     * @return true 表示这一帧已经处理完（调用方继续正常返回，别抛异常）
     */
    private boolean handleUnexpectedControlFrame(short opcode) throws IOException {
        switch (opcode) {
            case ControllerIdentifiers.CANCEL:
                // 电脑端点的是「中断发送」：这次传输作废，等它重建通道
                top.weixiansen574.hybridfilexfer.core.HFXLog.d("exchangePause: peer CANCEL");
                // 3.0.0：和 readResultFrameOrNull 一样，除了置标志还要关传输通道
                // （关 socket 才能让阻塞在 write 里的发块线程立刻退出）。
                // controlLock 在这条路径上是**已经持有**的，cancelByPeer 不碰它。
                cancelByPeer("exchangePause: peer CANCEL");
                return true;
            case ControllerIdentifiers.REBUILD_REQ: {
                // 电脑已经开始重建了：通道名先收好，交给 rebuildTransfers 用
                String name = ctChannel.readUTF();
                top.weixiansen574.hybridfilexfer.core.HFXLog.d("exchangePause: REBUILD_REQ " + name);
                stashRebuildName(name);
                cancelByPeer("exchangePause: REBUILD_REQ");
                return true;
            }
            default:
                return false;
        }
    }

    /** 只写一帧 PAUSE_STATE（调用方必须已持有 controlLock） */
    private void writePauseStateLocked(boolean phonePaused) throws IOException {
        byte[] frame = new byte[3];
        frame[0] = (byte) (ControllerIdentifiers.PAUSE_STATE >> 8);
        frame[1] = (byte) ControllerIdentifiers.PAUSE_STATE;
        frame[2] = (byte) (phonePaused ? 1 : 0);
        ctChannel.writeFully(frame);
    }

    public List<RemoteFile> listLocalFiles(String path) throws RemoteException {
        return listLocalFiles(ioService, path);
    }

    public boolean deleteLocalFile(String file) throws RemoteException {
        return ioService.deleteFile(file);
    }

    public boolean createLocalDir(String parent, String child) throws RemoteException {
        return ioService.appendAndMkdirs(parent, child);
    }

    public List<RemoteFile> listClientFiles(String path) throws IOException {
        synchronized (controlLock) {
            // 传输刚结束就刷新目录时，通道里还排着电脑回的 PAUSE_REPLY；
            // 不先吃掉的话会被当成"列表长度"，读线程卡死在 readUTF（真机实测）。
            drainStaleControlFrames();
            return doListClientFiles(path);
        }
    }

    private List<RemoteFile> doListClientFiles(String path) throws IOException {
        ctChannel.writeShort(ControllerIdentifiers.LIST_FILES);
        ctChannel.writeUTF(path);
        // 应答不带 opcode：先把可能混在前面的 PAUSE_REPLY 之类吃掉，别把它当成列表长度
        skipStaleFramesBeforeReply();
        int listSize = ctChannel.readInt();
        if (listSize == -1) {
            return null;
        }
        ArrayList<RemoteFile> remoteFiles = new ArrayList<>(listSize);
        //| name       | path       | lastModified | size    | isDirectory |
        //| ---------- | ---------- | ------------ | ------- | ----------- |
        //| String:UTF | String:UTF | long:8b      | long:8b | boolean     |
        for (int i = 0; i < listSize; i++) {
            RemoteFile remoteFile = new RemoteFile(
                    ctChannel.readUTF(),//name
                    ctChannel.readUTF(),//path
                    ctChannel.readLong(),//lastModified
                    ctChannel.readLong(),//size
                    ctChannel.readBoolean()//isDirectory
            );
            remoteFiles.add(remoteFile);
        }
        return remoteFiles;
    }

    public boolean deleteRemoteFile(String file) throws IOException {
        synchronized (controlLock) {
            drainStaleControlFrames();
            ctChannel.writeShort(ControllerIdentifiers.DELETE_FILE);
            ctChannel.writeUTF(file);
            skipStaleFramesBeforeReply();
            return ctChannel.readBoolean();
        }
    }

    public boolean createRemoteDir(String parent, String child) throws IOException {
        synchronized (controlLock) {
            drainStaleControlFrames();
            ctChannel.writeShort(ControllerIdentifiers.MKDIR);
            ctChannel.writeUTF(parent);
            ctChannel.writeUTF(child);
            skipStaleFramesBeforeReply();
            return ctChannel.readBoolean();
        }
    }

    public static List<RemoteFile> listLocalFiles(IIOService ioService, String path) throws RemoteException {
        int[] chunkIds = ioService.listFiles(path);
        if (chunkIds == null) {
            return null;
        }
        ArrayList<RemoteFile> remoteFiles = new ArrayList<>();
        for (int chunkId : chunkIds) {
            remoteFiles.addAll(ioService.getAndRemoveFileListSlice(chunkId));
        }
        return remoteFiles;
    }

    public List<String> getConnectionListINames() {
        List<TransferConnection> snapshot = connections;
        if (snapshot == null) {
            return new ArrayList<>();
        }
        List<String> names = new ArrayList<>(snapshot.size());
        for (TransferConnection connection : snapshot) {
            // 已经断开的通道不计入（WiFi 断开/切网后会残留在这里）
            if (connection.channel == null || !connection.channel.isOpen()) {
                continue;
            }
            names.add(connection.iName);
        }
        return names;
    }

    /** 会话结束：通知对端、关闭本连接的所有通道并释放缓冲；共享监听套接字保持打开。 */
    public synchronized void disconnect() {
        top.weixiansen574.hybridfilexfer.core.HFXLog.d("disconnect: called",
                new Exception("disconnect caller"));
        // 可能还有线程停在暂停闸门里，先叫醒再关
        pauseGate.resume();
        unregister(this);
        releaseBuffers();
        DataByteChannel channel = ctChannel;
        ctChannel = null;
        top.weixiansen574.hybridfilexfer.core.HFXLog.d("disconnect: 控制通道="
                + (channel == null ? "null" : String.valueOf(channel.getRemoteAddress()))
                + " 传输通道=" + (connections == null ? "null" : String.valueOf(connections.size())), null);
        if (channel != null) {
            // 收尾每一步都用 Throwable 兜住，别让某一步的异常把后面的步骤全跳过：
            // 以前 writeShort(SHUTDOWN) 抛的不是 IOException（运行时异常）时，这个方法直接跳出，
            // 控制通道既不发 FIN 也不关 —— 手机端 /proc/net/tcp 里这条连接一直挂着，
            // 电脑端只能等心跳超时（原本 100 秒，压到 12 秒）才发现手机停了服务。
            // 兜住之后电脑端 0.4 秒就收到「手机端已停止服务」。
            try {
                //通知断开连接
                channel.writeShort(ControllerIdentifiers.SHUTDOWN);
            } catch (Throwable ignored) {
            }
            // 再把 FIN 打出去：直接 close() 不一定让对端看到 EOF（adb 转发这条通道实测如此）
            try {
                java.nio.channels.SocketChannel rawChannel = channel.getSocketChannel();
                if (rawChannel != null) {
                    rawChannel.shutdownOutput();
                }
            } catch (Throwable ignored) {
            }
            //释放控制通道，避免反复重连时残留socket
            try {
                channel.close();
            } catch (Throwable ignored) {
            }
        }
        closeConnections(connections);
        connections = null;
    }

    /**
     * 307：持久化配置里的限速（MB/s），用于 SpeedLimiter 还没初始化的那几秒。
     * <p>
     * 由 ServerService 在 onCreate 里带 Context 读出来写一次 —— 那时离握手还早。
     * 0 是合法值（不限速），所以不需要额外的「已知」标志：这里永远反映配置文件。
     */
    private static volatile int configuredLimitFallbackMbps;

    public static void setConfiguredLimitFallback(int mbps) {
        configuredLimitFallbackMbps = Math.max(0, mbps);
    }

    /** 旧接口：等价于断开本连接（监听套接字由 ServerService 统一管理）。 */
    public void close() {
        disconnect();
    }

    @Override
    protected WriteFileCall createWriteFileCall(LinkedBlockingDeque<ByteBuffer> buffers, int dequeCount, java.util.Map<String, java.util.BitSet> resumeBits) {
        return new DroidWriteFileCall(buffers, dequeCount, ioService, resumeBits);
    }

    @Override
    protected ReadFileCall createReadFileCall(LinkedBlockingDeque<ByteBuffer> buffers, List<RemoteFile> files, Directory localDir, Directory remoteDir, int operateThreadCount, String[] channelNames, java.util.Map<String, java.util.BitSet> resumeBits) {
        return new DroidReadFileCall(ioService,buffers,files,localDir,remoteDir,operateThreadCount, channelNames, resumeBits);
    }

    /**
     * 307：两端的限速值（MB/s，0 = 不限速）→ 协商后的值。
     * <p>
     * 0 表示「我这端不限速」，<b>不参与 min</b> —— 一端填 0 不会把另一端的限制抹掉。
     * 电脑端 session.rs 的 negotiate_limit_mb 是同一套规则，两边必须一致，
     * 否则两端算出的生效值不同，就会出现「一边限一边不限」的怪象。
     */
    /** 限速值的说法（日志用）：0 写「不限速」，别写成「0 MB/s」—— 那读起来像堵死。 */
    private static String limitText(int mbps) {
        return mbps <= 0 ? "不限速" : mbps + " MB/s";
    }

    private static int negotiateLimitMbps(int phoneMbps, int pcMbps) {
        boolean phoneOn = phoneMbps > 0;
        boolean pcOn = pcMbps > 0;
        if (phoneOn && pcOn) {
            return Math.min(phoneMbps, pcMbps);
        }
        if (phoneOn) {
            return phoneMbps;
        }
        return pcOn ? pcMbps : 0;
    }
}