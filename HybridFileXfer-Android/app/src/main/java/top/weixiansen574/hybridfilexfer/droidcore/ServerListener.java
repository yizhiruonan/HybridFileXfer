package top.weixiansen574.hybridfilexfer.droidcore;

import java.io.IOException;
import java.nio.ByteBuffer;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;
import java.nio.charset.StandardCharsets;
import java.util.ArrayDeque;
import java.util.Arrays;

import top.weixiansen574.hybridfilexfer.core.HFXService;

/**
 * 共享监听套接字上的"连接分类器"。
 *
 * <p>手机同时接多台电脑时，所有电脑的控制通道和传输通道都连到同一个端口，而协议里
 * 没有字段能区分「这是另一台电脑的控制通道」还是「当前电脑的传输通道」。唯一可靠的
 * 区分方法是看连接建立后谁先说话：
 *
 * <ul>
 *   <li>控制通道：TCP 连上后立刻写出 "HFXC" 协议头（4 字节）；</li>
 *   <li>传输通道：握手全部完成之前一个字节都不发（参见 session.rs / HFXClient）。</li>
 * </ul>
 *
 * <p>所以每 accept 到一个连接，先观察一小段时间（{@link #CLASSIFY_WINDOW_MS}）：
 * 看到协议头就是控制通道，始终安静就是传输通道。控制通道被暂存起来，等当前会话
 * 握手结束后再处理，绝不会被错当成传输通道。
 *
 * <p>只在监听线程（ServerService 的 accept 循环）里使用，本身不是线程安全的。
 */
public class ServerListener implements AutoCloseable {

    private static final byte[] CLIENT_HEADER = HFXService.CLIENT_HEADER.getBytes(StandardCharsets.UTF_8);
    /**
     * 判断"这个连接有没有先说话"的观察窗口。控制通道的协议头是连上后立刻写出的
     * （同一台机器上实测 1ms 内就到），100ms 已经是二十倍余量。
     *
     * <p>这个窗口是**按通道串行付**的：14 条通道 × 400ms = 5.6 秒，全花在"等安静"上，
     * 用户看到的就是"插上线要等好几秒才连上"。压到 100ms 后同样 14 条只要 1.4 秒，
     * 而控制通道几乎不可能超过 100ms 还不说话（真要超了，也只是别的电脑刚接入那种极少数情况，
     * 电脑端会自己重连）。
     */
    private static final int CLASSIFY_WINDOW_MS = 100;
    private static final int ACCEPT_POLL_MS = 20;
    /** 暂存的控制通道上限：超出说明有人在反复重连，直接关掉避免堆积 */
    private static final int MAX_STASHED = 8;

    private static final int KIND_CONTROL = 1;
    private static final int KIND_TRANSFER = 2;
    private static final int KIND_INVALID = 3;
    private static final int KIND_CLOSED = 4;

    /** 已经握手完成、等待接入的控制通道（协议头已经被读掉） */
    public static final class PendingControl {
        public final SocketChannel socket;
        public final byte[] header;

        PendingControl(SocketChannel socket, byte[] header) {
            this.socket = socket;
            this.header = header;
        }
    }

    private final ServerSocketChannel shared;
    private final ArrayDeque<PendingControl> stashed = new ArrayDeque<>();
    private final ByteBuffer headerBuffer = ByteBuffer.allocate(4);

    /**
     * 304：取消后重建时，已经接入但还没被会话领走的传输通道。
     *
     * <p>以前空闲时收到"安静"连接（没先说协议头）一律当成上一轮的残留关掉。
     * 那个假设在重建时不成立：电脑收到 CANCEL 之后会立刻连一批新的传输通道过来，
     * 而这时手机这边还在等 CANCEL 的收尾，accept 线程照旧把它们关掉的话，
     * 重建就永远等不到通道（每一条都要等满超时才放弃）。
     *
     * <p>所以挂上"有人在等传输通道"的号之后，安静连接先留着等人来领；
     * 号撤销时（重建结束/超时）统一关掉。
     */
    private final ArrayDeque<SocketChannel> spareTransfers = new ArrayDeque<>();
    /** 当前有多少条传输通道被等着的会话预定了（>0 时安静连接不再丢弃） */
    private int transfersWanted = 0;
    private static final int MAX_SPARE = 16;

    public ServerListener(ServerSocketChannel shared) {
        this.shared = shared;
    }

    /**
     * 取下一个控制通道；超时返回 null。
     * 空闲时偶发收到的"安静"连接（多半是上一轮会话残留的传输通道）直接关掉。
     */
    public PendingControl nextControl(long timeoutMs) throws IOException {
        long deadline = System.currentTimeMillis() + Math.max(0, timeoutMs);
        while (System.currentTimeMillis() < deadline) {
            PendingControl pending = stashed.pollFirst();
            if (pending != null) {
                return pending;
            }
            SocketChannel socket = accept(deadline);
            if (socket == null) {
                continue;
            }
            int kind = classify(socket);
            if (kind == KIND_CONTROL) {
                return new PendingControl(socket, takeHeader());
            }
            // 304：有会话在等重建的传输通道时，安静连接先存着，别当成残留关掉
            if (kind == KIND_TRANSFER && stashTransfer(socket)) {
                continue;
            }
            closeQuietly(socket);
        }
        return null;
    }

    // ------------------------------------------------------------------
    // 304：取消后重建传输通道
    // ------------------------------------------------------------------

    /** 会话准备重建：宣布要 count 条传输通道（在通知电脑之前调用） */
    public synchronized void wantTransfers(int count) {
        transfersWanted = Math.max(transfersWanted, Math.max(0, count));
    }

    /** 重建结束：撤销预定，把没人领的连接关掉 */
    public synchronized void endWantTransfers() {
        transfersWanted = 0;
        SocketChannel socket;
        while ((socket = spareTransfers.pollFirst()) != null) {
            closeQuietly(socket);
        }
    }

    /**
     * 会话线程取一条已经接入的传输通道；超时返回 null。
     *
     * <p>只在 accept 线程里 accept/分类，所以这里只能等 accept 线程把连接放进队列。
     * 等待期间不持有 this 的锁 —— 持有的话 accept 线程就进不来，连接永远进不了队列。
     */
    public SocketChannel takeTransferChannel(long timeoutMs) {
        long deadline = System.currentTimeMillis() + Math.max(0, timeoutMs);
        do {
            synchronized (this) {
                SocketChannel socket = spareTransfers.pollFirst();
                if (socket != null) {
                    return socket;
                }
            }
            try {
                Thread.sleep(20);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
                return null;
            }
        } while (System.currentTimeMillis() < deadline);
        return null;
    }

    /** accept 线程：把一条安静的连接存进待领队列（没有预定或队列满时返回 false） */
    private synchronized boolean stashTransfer(SocketChannel socket) {
        if (transfersWanted <= 0 || spareTransfers.size() >= MAX_SPARE) {
            return false;
        }
        spareTransfers.addLast(socket);
        return true;
    }

    /**
     * 当前会话正在等一条传输通道。期间收到的控制通道（别的电脑刚连进来）先暂存，
     * 不会误当作传输通道；超时返回 null。
     */
    public SocketChannel takeTransfer(long timeoutMs) throws IOException {
        long deadline = System.currentTimeMillis() + Math.max(0, timeoutMs);
        while (System.currentTimeMillis() < deadline) {
            SocketChannel socket = accept(deadline);
            if (socket == null) {
                continue;
            }
            int kind = classify(socket);
            if (kind == KIND_TRANSFER) {
                return socket;
            }
            if (kind == KIND_CONTROL) {
                PendingControl pending = new PendingControl(socket, takeHeader());
                if (stashed.size() < MAX_STASHED) {
                    stashed.addLast(pending);
                } else {
                    closeQuietly(socket);
                }
                continue;
            }
            closeQuietly(socket);
        }
        return null;
    }

    /**
     * 取一条「来自指定网卡」的传输通道。
     *
     * <p>为什么不能按到达顺序拿：电脑端是**并行**连所有通道的（这样某条连不上只等 1.5 秒，
     * 而不是一条条累加），到达顺序和手机广播的网卡顺序毫无关系。以前 accept 线程按先来后到
     * 交出去，手机再把「电脑报的名字」贴上去 —— 名字和实际链路就对不上了：
     * 用户看到的「bt-pan 18MB/s / wlan0#3 18B/s」，其实是蓝牙那条被贴了无线的名字。
     *
     * <p>现在按**远端地址所在网段**认人：电脑连 192.168.44.1 时源地址一定是 192.168.44.x，
     * 连 192.168.3.117 时源地址一定在 192.168.3.0/24。对不上的先存起来，留给下一轮再取。
     */
    public SocketChannel takeTransferFor(java.net.InetAddress wanted, long timeoutMs) throws IOException {
        long deadline = System.currentTimeMillis() + Math.max(0, timeoutMs);
        while (System.currentTimeMillis() < deadline) {
            SocketChannel stashedSocket = takeStashed(wanted);
            if (stashedSocket != null) {
                return stashedSocket;
            }
            SocketChannel socket = accept(deadline);
            if (socket == null) {
                continue;
            }
            int kind = classify(socket);
            if (kind == KIND_TRANSFER) {
                if (sameSubnet(socket, wanted)) {
                    return socket;
                }
                // 不是这一条网卡的：先存着，等轮到它对应的网卡时再取
                if (!stashTransfer(socket)) {
                    closeQuietly(socket);
                }
                continue;
            }
            if (kind == KIND_CONTROL) {
                PendingControl pending = new PendingControl(socket, takeHeader());
                if (stashed.size() < MAX_STASHED) {
                    stashed.addLast(pending);
                } else {
                    closeQuietly(socket);
                }
                continue;
            }
            closeQuietly(socket);
        }
        return null;
    }

    /** 从待领队列里挑一条和 wanted 同网段的 */
    private SocketChannel takeStashed(java.net.InetAddress wanted) {
        synchronized (this) {
            if (spareTransfers.isEmpty()) {
                return null;
            }
            java.util.Iterator<SocketChannel> it = spareTransfers.iterator();
            while (it.hasNext()) {
                SocketChannel socket = it.next();
                if (sameSubnet(socket, wanted)) {
                    it.remove();
                    return socket;
                }
            }
        }
        return null;
    }

    /** 这条连接的远端地址是不是和想要的网卡在同一网段（回环按回环比） */
    private static boolean sameSubnet(SocketChannel socket, java.net.InetAddress wanted) {
        if (wanted == null) {
            return true;
        }
        java.net.InetAddress remote;
        try {
            java.net.SocketAddress address = socket.getRemoteAddress();
            if (!(address instanceof java.net.InetSocketAddress)) {
                return false;
            }
            remote = ((java.net.InetSocketAddress) address).getAddress();
        } catch (Exception e) {
            return false;
        }
        if (remote == null) {
            return false;
        }
        if (wanted.isLoopbackAddress() || remote.isLoopbackAddress()) {
            return wanted.isLoopbackAddress() && remote.isLoopbackAddress();
        }
        byte[] a = wanted.getAddress();
        byte[] b = remote.getAddress();
        // 只比 /24：一个 /24 里就是同一张物理网卡（多开几条通道时它们本来就一样）
        return a.length == 4 && b.length == 4
                && a[0] == b[0] && a[1] == b[1] && a[2] == b[2];
    }

    /** 关掉暂存的控制通道；共享监听套接字由 HFXServer 负责关闭。 */
    @Override
    public void close() {
        PendingControl pending;
        while ((pending = stashed.pollFirst()) != null) {
            closeQuietly(pending.socket);
        }
        synchronized (this) {
            transfersWanted = 0;
            SocketChannel socket;
            while ((socket = spareTransfers.pollFirst()) != null) {
                closeQuietly(socket);
            }
        }
    }

    private byte[] takeHeader() {
        byte[] header = new byte[4];
        System.arraycopy(headerBuffer.array(), 0, header, 0, 4);
        headerBuffer.clear();
        return header;
    }

    private SocketChannel accept(long deadline) throws IOException {
        while (System.currentTimeMillis() < deadline) {
            SocketChannel socket = shared.accept();
            if (socket != null) {
                socket.configureBlocking(true);
                try {
                    socket.socket().setTcpNoDelay(true);
                } catch (Throwable ignored) {
                }
                return socket;
            }
            try {
                Thread.sleep(ACCEPT_POLL_MS);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
                throw new IOException("interrupted while accepting", e);
            }
        }
        return null;
    }

    /**
     * 观察连接的第一批字节：立刻说话 → 控制通道；窗口内一直安静 → 传输通道。
     * 读到的 4 个协议头字节保存在 headerBuffer 里，控制通道握手套接时不再重复读。
     */
    private int classify(SocketChannel socket) throws IOException {
        headerBuffer.clear();
        long deadline = System.currentTimeMillis() + CLASSIFY_WINDOW_MS;
        boolean closed = false;
        socket.configureBlocking(false);
        try {
            while (headerBuffer.position() < 4 && System.currentTimeMillis() < deadline) {
                int read;
                try {
                    read = socket.read(headerBuffer);
                } catch (IOException e) {
                    read = -1;
                }
                if (read == -1) {
                    closed = true;
                    break;
                }
                if (read == 0) {
                    try {
                        Thread.sleep(5);
                    } catch (InterruptedException e) {
                        Thread.currentThread().interrupt();
                        throw new IOException("interrupted while classifying", e);
                    }
                }
            }
        } finally {
            try {
                socket.configureBlocking(true);
            } catch (IOException ignored) {
                // 连接已经断了，继续走下面的分支统一关掉
            }
        }
        if (headerBuffer.position() == 4) {
            return Arrays.equals(headerBuffer.array(), CLIENT_HEADER) ? KIND_CONTROL : KIND_INVALID;
        }
        if (closed) {
            return KIND_CLOSED;
        }
        return headerBuffer.position() == 0 ? KIND_TRANSFER : KIND_INVALID;
    }

    private static void closeQuietly(SocketChannel socket) {
        if (socket == null) {
            return;
        }
        try {
            socket.close();
        } catch (IOException ignored) {
        }
    }
}
