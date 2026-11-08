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

    public void readFully(ByteBuffer dst) throws IOException {
        synchronized (readLock) {
            while (dst.hasRemaining()) {
                if (origin.read(dst) == -1) {
                    throw new EOFException();
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
