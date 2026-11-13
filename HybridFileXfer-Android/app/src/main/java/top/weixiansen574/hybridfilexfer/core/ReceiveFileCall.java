package top.weixiansen574.hybridfilexfer.core;

import top.weixiansen574.hybridfilexfer.core.callback.TransferFileCallback;
import top.weixiansen574.nio.DataByteChannel;

import java.io.EOFException;
import java.io.IOException;
import java.nio.ByteBuffer;
import java.util.concurrent.Callable;

public class ReceiveFileCall implements Callable<Void> {
    private final WriteFileCall writeFileCall;
    private final int tIndex;
    private final DataByteChannel channel;
    private final TransferFileCallback callback;
    private final TransferConnection connection;
    private final String iName;
    private final TransferPause pause;

    public ReceiveFileCall(int tIndex, TransferConnection connection,WriteFileCall writeFileCall,TransferFileCallback callback, TransferPause pause) {
        this.pause = pause;
        this.writeFileCall = writeFileCall;
        this.tIndex = tIndex;
        this.connection = connection;
        this.channel = connection.channel;
        this.callback = callback;
        iName = connection.iName;
        connection.resetTotalTrafficInfo();
        //this.dis = dataInputStream;
    }

    @Override
    public Void call() throws Exception {
        long startTime = System.currentTimeMillis();
        try {
            while (true) {
                if (pause != null) {
                    pause.awaitResume();
                }
                short header = channel.readShort();
                switch (header) {
                    case TransferIdentifiers.FOLDER: {
                        int fileIndex = channel.readInt();
                        String path = channel.readUTF();
                        long lastModified = channel.readLong();
                        writeFileCall.putBlock(new FileBlock(false, fileIndex, path, lastModified, 0, 0, null), tIndex);
                        break;
                    }
                    case TransferIdentifiers.FILE: {
                        int fileIndex = channel.readInt();
                        String path = channel.readUTF();
                        long lastModified = channel.readLong();
                        long totalSize = channel.readLong();
                        int index = channel.readInt();
                        int length = channel.readInt();
                        // 303：这一块的 CRC32，写盘前必须一致
                        int expectedCrc = channel.readInt();
                        callback.onFileDownloading(iName, path,
                                index * (long) FileBlock.BLOCK_SIZE + length,
                                totalSize);
                        ByteBuffer buffer = writeFileCall.getBuffer();
                        buffer.clear();
                        buffer.limit(length);
                        int read;
                        while (buffer.hasRemaining()) {
                            read = channel.read(buffer);
                            if (read == -1) {
                                throw new EOFException();
                            }
                            connection.addDownloadedBytes(read);
                        }
                        channel.readFully(buffer);
                        // 303：块内容与发送端不一致就别写盘了，直接报错
                        ByteBuffer peek = buffer.duplicate();
                        peek.position(0);
                        int actualCrc = HFXService.crc32(peek);
                        if (actualCrc != expectedCrc) {
                            throw new IOException("数据校验失败：块 " + index + " 的 CRC32 不符（"
                                    + Integer.toHexString(actualCrc) + " != " + Integer.toHexString(expectedCrc) + "）");
                        }
                        writeFileCall.putBlock(new FileBlock(true, fileIndex, path, lastModified, totalSize, index, buffer), tIndex);
                        break;
                    }
                    case TransferIdentifiers.EOF:
                        //System.out.println(iName + " 接收完成");
                        writeFileCall.finishChannel(tIndex);
                        callback.onChannelComplete(iName,
                                connection.getDownloadedBytes(),
                                System.currentTimeMillis() - startTime);
                        return null;
                    case TransferIdentifiers.END_OF_INTERRUPTED:
                        //System.out.println("传输通道：" + iName + " 已中断，因其他通道断开");
                        writeFileCall.cancel();
                        callback.onChannelError(iName,
                                TransferFileCallback.ERROR_TYPE_INTERRUPT,null);
                        return null;
                    case TransferIdentifiers.END_OF_READ_ERROR:
                        //System.out.println("传输通道：" + iName + " 已取消传输，因为读取文件时发生异常");
                        writeFileCall.cancel();
                        callback.onChannelError(iName,
                                TransferFileCallback.ERROR_TYPE_READ_ERROR,null);
                        return null;
                    case TransferIdentifiers.END_OF_WRITE_ERROR:
                        //System.out.println("传输通道：" + iName + " 已取消传输，因为写入文件时发生异常");
                        writeFileCall.cancel();
                        callback.onChannelError(iName,
                                TransferFileCallback.ERROR_TYPE_WRITE_ERROR,null);
                        return null;
                }
            }
        } catch (IOException e){
            writeFileCall.finishChannel(tIndex);
            // 用户点「取消」时通道是我们自己关的，异常属于预期收尾
            if (!connection.isCancelled()) {
                callback.onChannelError(iName,
                        TransferFileCallback.ERROR_TYPE_EXCEPTION,e.toString());
            }
            throw e;
        }
    }
}
