package top.weixiansen574.hybridfilexfer.core;

import java.util.concurrent.Callable;

import top.weixiansen574.hybridfilexfer.core.callback.TransferFileCallback;
import top.weixiansen574.nio.DataByteChannel;

public class SendFileCall implements Callable<Void> {
    private final ReadFileCall readFileCall;
    private final DataByteChannel channel;
    private final TransferConnection connection;
    private final TransferFileCallback callback;
    private final TransferPause pause;
    /** 这条通道在分段计划里的序号（自己只领自己那段的块） */
    private final int channelIndex;

    public SendFileCall(ReadFileCall readFileCall, TransferConnection connection,
                        TransferFileCallback callback, TransferPause pause, int channelIndex) {
        this.readFileCall = readFileCall;
        this.connection = connection;
        this.channel = connection.channel;
        this.callback = callback;
        this.pause = pause;
        this.channelIndex = channelIndex;
        connection.resetTotalTrafficInfo();
    }

    @Override
    public Void call() throws Exception {
        FileBlock fileBlock = null;
        try {
            long startTime = System.currentTimeMillis();
            while (true) {
                if (pause != null) {
                    pause.awaitResume();
                }
                // 只领自己那一段的块；没有就看看能不能把别人没产出的段切一半过来。
                // 都干完了（读文件线程也结束了）就发 EOF 收工 —— 空闲通道必须收工，
                // 否则电脑端会一直等这条通道的 EOF，整个传输收不了尾。
                FileBlock block = readFileCall.pollOwnBlock(channelIndex, 5);
                if (block == null) {
                    block = readFileCall.pollSpecialBlock();
                }
                if (block == null) {
                    if (readFileCall.trySteal(channelIndex)) {
                        continue;
                    }
                    if (readFileCall.isReaderFinished()) {
                        channel.writeShort(TransferIdentifiers.EOF);
                        callback.onChannelComplete(connection.iName,
                                connection.getUploadedBytes(),
                                System.currentTimeMillis() - startTime);
                        break;
                    }
                    Thread.sleep(2);
                    continue;
                }
                fileBlock = block;
                if (fileBlock.isFile && fileBlock.data != null) {
                    SpeedLimiter.throttle(fileBlock.data.capacity());
                }
                //-1为特殊块
                if (fileBlock.fileIndex == -1) {
                    if (fileBlock == ReadFileCall.END_POINT) {
                        channel.writeShort(TransferIdentifiers.EOF);
                        callback.onChannelComplete(connection.iName,
                                connection.getUploadedBytes(),
                                System.currentTimeMillis() - startTime);
                    } else if (fileBlock == ReadFileCall.INTERRUPT) {
                        channel.writeShort(TransferIdentifiers.END_OF_INTERRUPTED);
                        callback.onChannelError(connection.iName,TransferFileCallback.ERROR_TYPE_INTERRUPT, null);
                    } else if (fileBlock == ReadFileCall.READ_ERROR) {
                        channel.writeShort(TransferIdentifiers.END_OF_READ_ERROR);
                        callback.onChannelError(connection.iName,TransferFileCallback.ERROR_TYPE_READ_ERROR, null);
                    } else if (fileBlock == ReadFileCall.WRITE_ERROR) {
                        channel.writeShort(TransferIdentifiers.END_OF_WRITE_ERROR);
                        callback.onChannelError(connection.iName,TransferFileCallback.ERROR_TYPE_WRITE_ERROR, null);
                    }
                    break;
                }
                channel.writeShort(fileBlock.isFile ? TransferIdentifiers.FILE :
                        TransferIdentifiers.FOLDER);//是否文件夹
                channel.writeInt(fileBlock.fileIndex);
                channel.writeUTF(fileBlock.path);//路径
                channel.writeLong(fileBlock.lastModified);//修改日期
                if (!fileBlock.isFile) continue;
                channel.writeLong(fileBlock.totalSize);//总大小
                channel.writeInt(fileBlock.index);//索引
                channel.writeInt(fileBlock.getLength());//单块的长度

                callback.onFileUploading(connection.iName, fileBlock.path,
                        fileBlock.getStartPosition() + fileBlock.getLength(),
                        fileBlock.totalSize);

                fileBlock.data.flip();
                // 303：块校验和（CRC32），电脑端写盘前校验这一块
                channel.writeInt(HFXService.crc32(fileBlock.data));
                // 一整个块要写满（write 只保证写一部分）
                channel.writeFully(fileBlock.data);
                readFileCall.recycleBuffer(fileBlock.data);
                connection.addUploadedBytes(fileBlock.getLength());
            }
        } catch (Exception e) {
            //若发生异常，通知其他传输通道，停止传输
            top.weixiansen574.hybridfilexfer.core.HFXLog.d("SendFileCall[" + connection.iName + "] FAILED, wroteLast=" + fileBlock, e);
            if (fileBlock != null){
                //回收文件分块的ByteBuffer，否则导致这个Buffer免费了
                readFileCall.recycleBuffer(fileBlock.data);
            }
            readFileCall.shutdownByConnectionBreak();
            // 用户点「取消」时通道是被我们自己关掉的，ClosedChannelException /
            // AsynchronousCloseException 都是预期内的收尾，不该再弹"通道错误"。
            if (!connection.isCancelled()) {
                callback.onChannelError(connection.iName,TransferFileCallback.ERROR_TYPE_EXCEPTION, e.toString());
            }
            throw e;
        }
        return null;
    }

}
