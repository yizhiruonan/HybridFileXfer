package top.weixiansen574.nio;

import java.io.DataInput;
import java.io.DataOutput;
import java.io.EOFException;
import java.io.IOException;
import java.nio.ByteBuffer;
import java.nio.channels.ByteChannel;
import java.nio.charset.StandardCharsets;

public class DataByteChannel implements ByteChannel, DataInput, DataOutput {
    private final ByteChannel origin;

    /**
     * 读缓冲：只给 readXxx 用。
     *
     * <p>以前读写共用同一个 {@code buffer}，而控制通道的读和写是两个线程：
     * 会话线程每 3 秒写一次心跳（{@code writeShort(PING)}），传输线程同时在
     * {@code readShort()} 里阻塞读结论帧。心跳那一下 {@code buffer.clear()} 会把
     * 读线程正在阻塞读的位置/limit 一起清掉，2 字节的帧就可能只送出 1 个字节，
     * 于是对端把下一帧的高位字节当成 opcode 读出来 —— 实测电脑端正好读出
     * {@code 00 00}（即 opcode 0），整条控制通道从此错位，接着会话线程报错
     * {@code disconnect()} 把 6 条传输通道一起关掉。读写缓冲分开，这种踩踏从根上消失。
     */
    private final ByteBuffer readBuffer = ByteBuffer.allocate(8);
    /** 写缓冲：只给 writeXxx 用 */
    private final ByteBuffer writeBuffer = ByteBuffer.allocate(8);
    /** 串行化短帧写入：一帧（opcode + 参数）不能被别的线程插在中间 */
    private final Object writeLock = new Object();
    /** 串行化读取（正常只有一个读者；这里防的是误用把 buffer 弄乱） */
    private final Object readLock = new Object();

    public DataByteChannel(ByteChannel origin) {
        this.origin = origin;
    }

    /** 底下真正的 SocketChannel；不是 SocketChannel 时返回 null。 */
    public java.nio.channels.SocketChannel getSocketChannel() {
        if (origin instanceof java.nio.channels.SocketChannel) {
            return (java.nio.channels.SocketChannel) origin;
        }
        return null;
    }

    /** 对端地址；不是 SocketChannel 时返回 null。 */
    public java.net.InetSocketAddress getRemoteAddress() {
        try {
            if (origin instanceof java.nio.channels.SocketChannel) {
                return (java.net.InetSocketAddress) ((java.nio.channels.SocketChannel) origin).getRemoteAddress();
            }
        } catch (Exception ignored) {
        }
        return null;
    }

    /**
     * 通道里已经到达、还没读走的字节数（0 表示暂时没有数据）。
     *
     * <p>给"先确认整帧到齐再读"的场合用：不能直接读，否则半个帧头会被吃掉，
     * 剩下的半个字节会和下一个帧头拼成一个假 opcode，整条控制通道从此错位。
     * 通道处于非阻塞模式时拿不到流，返回 0。
     */
    public int available() {
        java.nio.channels.SocketChannel socketChannel = getSocketChannel();
        if (socketChannel == null) {
            return 0;
        }
        try {
            return socketChannel.socket().getInputStream().available();
        } catch (Throwable e) {
            return 0;
        }
    }

    @Override
    public int read(ByteBuffer dst) throws IOException {
        return origin.read(dst);
    }

    @Override
    public int write(ByteBuffer src) throws IOException {
        return origin.write(src);
    }

    @Override
    public boolean isOpen() {
        return origin.isOpen();
    }

    @Override
    public void close() throws IOException {
        origin.close();
    }

    /**
     * 把整段字节写完。SocketChannel.write 可能只写一部分，短帧（opcode + 参数）
     * 必须整体送达，否则对端会把半个帧当成别的指令读。
     */
    public void writeFully(byte[] bytes) throws IOException {
        writeFully(ByteBuffer.wrap(bytes));
    }

    public void writeFully(ByteBuffer src) throws IOException {
        synchronized (writeLock) {
            while (src.hasRemaining()) {
                origin.write(src);
            }
        }
    }

    /**
     * 读超时（毫秒）。小于等于 0 表示一直等（传输通道用：大块数据本来就要等）。
     *
     * <p>控制通道设一个上限：以前这里是无上限的阻塞读，只要有一个帧对不上
     * （比如把残留的 PAUSE_REPLY 当成目录列表的长度），读线程就会永久卡住 ——
     * 它往往还握着 controlLock，会话线程连心跳都发不出去，电脑端 100 秒后就判超时断线。
     * 实测：一次正常传输收尾后界面刷新目录，就这么把整条会话拖死了。
     */
    private volatile int readTimeoutMs = 0;

    public void setReadTimeout(int timeoutMs) {
        this.readTimeoutMs = timeoutMs;
    }

    /**
     * 应答解析前的"退字"缓冲。
     *
     * <p>控制通道的应答大多不带 opcode（例如目录列表直接就是 {i32 数量}）。要判断
     * "这两个字节到底是应答，还是混在前面的 PAUSE_REPLY 帧头"，只能先试探读、不是帧
     * 再退回来 —— 没有这个缓冲，退回去的字节就丢了，帧流立刻错位。
     */
    private final byte[] pushback = new byte[8];
    private int pushbackLen;

    /** 非阻塞地往退字缓冲里补最多 max 个字节；返回缓冲里现有的字节数 */
    public int pollPushback(int max, long timeoutMs) throws IOException {
        synchronized (readLock) {
            java.nio.channels.SocketChannel socketChannel = getSocketChannel();
            if (socketChannel == null) {
                return pushbackLen;
            }
            long deadline = System.currentTimeMillis() + Math.max(0, timeoutMs);
            socketChannel.configureBlocking(false);
            try {
                while (pushbackLen < max && pushbackLen < pushback.length) {
                    int read = socketChannel.read(ByteBuffer.wrap(pushback, pushbackLen, pushback.length - pushbackLen));
                    if (read < 0) {
                        throw new EOFException();
                    }
                    if (read > 0) {
                        pushbackLen += read;
                    } else {
                        if (System.currentTimeMillis() >= deadline) {
                            break;
                        }
                        try {
                            Thread.sleep(5);
                        } catch (InterruptedException e) {
                            Thread.currentThread().interrupt();
                            throw new IOException("interrupted while peeking control frame", e);
                        }
                    }
                }
            } finally {
                try {
                    socketChannel.configureBlocking(true);
                } catch (IOException ignored) {
                }
            }
            return pushbackLen;
        }
    }

    /** 看退字缓冲里第 index 个字节（越界返回 0） */
    public byte peekPushback(int index) {
        synchronized (readLock) {
            return index >= 0 && index < pushbackLen ? pushback[index] : 0;
        }
    }

    public int pushbackCount() {
        synchronized (readLock) {
            return pushbackLen;
        }
    }

    /** 从退字缓冲里消耗 count 个字节 */
    public void consumePushback(int count) {
        synchronized (readLock) {
            int n = Math.max(0, Math.min(count, pushbackLen));
            System.arraycopy(pushback, n, pushback, 0, pushbackLen - n);
            pushbackLen -= n;
        }
    }

    /** 先把退字缓冲里的字节喂给 dst（调用方必须持有 readLock） */
    private void drainPushbackLocked(ByteBuffer dst) {
        if (pushbackLen <= 0) {
            return;
        }
        int n = Math.min(pushbackLen, dst.remaining());
        dst.put(pushback, 0, n);
        System.arraycopy(pushback, n, pushback, 0, pushbackLen - n);
        pushbackLen -= n;
    }

    public void readFully(ByteBuffer dst) throws IOException {
        int timeout = readTimeoutMs;
        java.nio.channels.SocketChannel socketChannel =
                timeout > 0 ? getSocketChannel() : null;
        if (socketChannel == null) {
            synchronized (readLock) {
                drainPushbackLocked(dst);
                while (dst.hasRemaining()) {
                    if (origin.read(dst) == -1) {
                        throw new EOFException();
                    }
                }
            }
            return;
        }
        synchronized (readLock) {
            drainPushbackLocked(dst);
            if (!dst.hasRemaining()) {
                return;
            }
            long deadline = System.currentTimeMillis() + timeout;
            socketChannel.configureBlocking(false);
            try {
                while (dst.hasRemaining()) {
                    int read = socketChannel.read(dst);
                    if (read < 0) {
                        throw new EOFException();
                    }
                    if (read == 0) {
                        if (System.currentTimeMillis() >= deadline) {
                            throw new java.net.SocketTimeoutException(
                                    "control channel read timed out after " + timeout + "ms");
                        }
                        try {
                            Thread.sleep(5);
                        } catch (InterruptedException e) {
                            Thread.currentThread().interrupt();
                            throw new IOException("interrupted while reading control frame", e);
                        }
                    }
                }
            } finally {
                try {
                    socketChannel.configureBlocking(true);
                } catch (IOException ignored) {
                }
            }
        }
    }

    @Override
    public void readFully(byte[] b) throws IOException {
        readFully(ByteBuffer.wrap(b));
    }

    @Override
    public void readFully(byte[] b, int off, int len) throws IOException {
        readFully(ByteBuffer.wrap(b, off, len));
    }

    @Override
    public int skipBytes(int n) throws IOException {
        ByteBuffer skipBuffer = ByteBuffer.allocate(n);
        return origin.read(skipBuffer);
    }

    @Override
    public boolean readBoolean() throws IOException {
        return readByte() != 0;
    }

    @Override
    public byte readByte() throws IOException {
        synchronized (readLock) {
            readBuffer.clear().limit(1);
            readFully(readBuffer);
            readBuffer.flip();
            return readBuffer.get();
        }
    }

    @Override
    public int readUnsignedByte() throws IOException {
        return readByte() & 0xFF;
    }

    @Override
    public short readShort() throws IOException {
        synchronized (readLock) {
            readBuffer.clear().limit(2);
            readFully(readBuffer);
            readBuffer.flip();
            return readBuffer.getShort();
        }
    }

    @Override
    public int readUnsignedShort() throws IOException {
        return readShort() & 0xFFFF;
    }

    @Override
    public char readChar() throws IOException {
        return (char) readShort();
    }

    @Override
    public int readInt() throws IOException {
        synchronized (readLock) {
            readBuffer.clear().limit(4);
            readFully(readBuffer);
            readBuffer.flip();
            return readBuffer.getInt();
        }
    }

    @Override
    public long readLong() throws IOException {
        synchronized (readLock) {
            readBuffer.clear().limit(8);
            readFully(readBuffer);
            readBuffer.flip();
            return readBuffer.getLong();
        }
    }

    @Override
    public float readFloat() throws IOException {
        return Float.intBitsToFloat(readInt());
    }

    @Override
    public double readDouble() throws IOException {
        return Double.longBitsToDouble(readLong());
    }

    @Override
    public String readLine() throws IOException {
        throw new UnsupportedOperationException("readLine() is not implemented.");
    }

    @Override
    public String readUTF() throws IOException {
        synchronized (readLock) {
            int length = readUnsignedShort();
            byte[] bytes = new byte[length];
            readFully(bytes);
            return new String(bytes, StandardCharsets.UTF_8);
        }
    }

    @Override
    public void write(int b) throws IOException {
        writeByte(b);
    }

    @Override
    public void write(byte[] b) throws IOException {
        write(b, 0, b.length);
    }

    @Override
    public void write(byte[] b, int off, int len) throws IOException {
        writeFully(ByteBuffer.wrap(b, off, len));
    }

    @Override
    public void writeBoolean(boolean v) throws IOException {
        writeByte(v ? 1 : 0);
    }

    @Override
    public void writeByte(int v) throws IOException {
        synchronized (writeLock) {
            writeBuffer.clear();
            writeBuffer.put((byte) v);
            writeBuffer.flip();
            writeFully(writeBuffer);
        }
    }

    @Override
    public void writeShort(int v) throws IOException {
        synchronized (writeLock) {
            writeBuffer.clear();
            writeBuffer.putShort((short) v);
            writeBuffer.flip();
            writeFully(writeBuffer);
        }
    }

    @Override
    public void writeChar(int v) throws IOException {
        writeShort(v);
    }

    @Override
    public void writeInt(int v) throws IOException {
        synchronized (writeLock) {
            writeBuffer.clear();
            writeBuffer.putInt(v);
            writeBuffer.flip();
            writeFully(writeBuffer);
        }
    }

    @Override
    public void writeLong(long v) throws IOException {
        synchronized (writeLock) {
            writeBuffer.clear();
            writeBuffer.putLong(v);
            writeBuffer.flip();
            writeFully(writeBuffer);
        }
    }

    @Override
    public void writeFloat(float v) throws IOException {
        writeInt(Float.floatToIntBits(v));
    }

    @Override
    public void writeDouble(double v) throws IOException {
        writeLong(Double.doubleToLongBits(v));
    }

    @Override
    public void writeBytes(String s) throws IOException {
        write(s.getBytes(StandardCharsets.UTF_8));
    }

    @Override
    public void writeChars(String s) throws IOException {
        for (char c : s.toCharArray()) {
            writeChar(c);
        }
    }

    @Override
    public void writeUTF(String s) throws IOException {
        byte[] bytes = s.getBytes(StandardCharsets.UTF_8);
        if (bytes.length > 65535) {
            throw new IOException("String too long");
        }
        synchronized (writeLock) {
            writeShort(bytes.length);
            writeFully(ByteBuffer.wrap(bytes));
        }
    }
}
