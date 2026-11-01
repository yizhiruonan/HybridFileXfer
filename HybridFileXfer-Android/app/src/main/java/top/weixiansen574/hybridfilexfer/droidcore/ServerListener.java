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
     * （同一台机器上通常 1ms 内就到），400ms 留了足够的余量；代价是当前电脑的每条
     * 传输通道要多等 400ms，一台电脑最多 8 条通道，仍在电脑端 15s 的握手超时之内。
     */
    private static final int CLASSIFY_WINDOW_MS = 400;
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
            closeQuietly(socket);
        }
        return null;
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

    /** 关掉暂存的控制通道；共享监听套接字由 HFXServer 负责关闭。 */
    @Override
    public void close() {
        PendingControl pending;
        while ((pending = stashed.pollFirst()) != null) {
            closeQuietly(pending.socket);
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
