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
    /** 所属服务：写异常要问它「这次传输是不是已经被对端取消了」 */
    private final HFXService owner;

    /**
     * 通道被关掉之后，等「对端取消」标志落位的最长时间。
     *
     * <p>电脑端「取消 → 保留进度」的顺序是<b>先 shutdown 传输通道、再发 CANCEL</b>，
     * 手机端的发块线程会先撞上写异常，读控制通道的线程几十毫秒后才读到 CANCEL。
     * 等这么久就够它落位；真异常（拔网线）也只是晚 1.5 秒报出来。
     */
    private static final long CANCEL_SETTLE_MS = 1500L;

    /**
     * 一块数据分几片写。蓝牙共享网络实测只有 50~60 KB/s，一块 1MB 要写十几秒；
     * 整块写完才记字节的话，界面上那条通道十几秒都停在「0 B 正在发送数据…」，看着像坏了
     * （用户报的「蓝牙通道速率开始怎么是 0」）。按 64KB 一片写，边写边记已发字节，
     * 数字从第一秒就在动，速度也能更早测出来（慢通道降级判断不用等一块）。
     */
    private static final int SLICE_WRITE_BYTES = 64 * 1024;

    public SendFileCall(ReadFileCall readFileCall, TransferConnection connection,
                        TransferFileCallback callback, TransferPause pause, int channelIndex,
                        HFXService owner) {
        this.readFileCall = readFileCall;
        this.connection = connection;
        this.channel = connection.channel;
        this.callback = callback;
        this.pause = pause;
        this.channelIndex = channelIndex;
        this.owner = owner;
        connection.resetTotalTrafficInfo();
    }

    /**
     * 这次异常该按「取消」处理，还是按「通道错误」报？
     *
     * <p>判据（任一成立就是取消）：
     * <ol>
     *   <li>这条通道自己被打过「取消」标（本端取消 / 电脑端取消时 {@code shutdownTransferChannels} 打的）；</li>
     *   <li>{@link HFXService#isCancelLike()}：本次传输已经被判定为取消（对端 CANCEL 已读到、
     *       待重建、或本端取消）；</li>
     *   <li>异常来自通道被关（IOException），且在这之后的一个短窗口内（{@link #CANCEL_SETTLE_MS}）
     *       对端取消标志落位了 —— 这就是电脑端「先关通道再发 CANCEL」的竞态：
     *       这一刻标志还是 false，但它马上就要变 true。</li>
     * </ol>
     *
     * <p>只有三条都不成立才真的是通道错误（拔网线、电脑崩溃、缓冲区写坏）。
     */
    private boolean isCancelledLike(Throwable e) {
        if (connection.isCancelled()) {
            return true;
        }
        HFXService service = owner;
        if (service == null) {
            return false;
        }
        if (service.isCancelLike()) {
            return true;
        }
        if (e != null && !(e instanceof java.io.IOException)) {
            // 不是通道层的异常（比如缓冲区被并发写坏）：跟取消无关，别等
            return false;
        }
        // e == null 是「别的通道出事了」那个中断标记：一样要等，因为电脑端取消时
        // 全部通道是同时被 shutdown 的，这边几乎同时收到写异常 / 中断标记，
        // 谁先到都说明不了"是不是取消"，只有那个标志能说明。
        return service.awaitCancelSignal(CANCEL_SETTLE_MS);
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
                    if (readFileCall.isReaderFinished() && readFileCall.hasSpecialBlocks()) {
                        // 读线程已经收工，但公共队列里还有被救回来的块没人发：
                        // 这时发 EOF 会让这些块永远留在内存里 —— 电脑端就少块
                        // （真机「缺 1 #4312」就是这么来的：所有通道都拿到 END_POINT 收工了）。
                        Thread.sleep(2);
                        continue;
                    }
                    if (readFileCall.isReaderFinished()) {
                        top.weixiansen574.hybridfilexfer.core.HFXLog.d("通道收工(无活) " + connection.iName
                                + " 已发 " + connection.getUploadedBytes() + " 字节");
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
                        top.weixiansen574.hybridfilexfer.core.HFXLog.d("通道收工(端点) " + connection.iName
                                + " 已发 " + connection.getUploadedBytes() + " 字节");
                        channel.writeShort(TransferIdentifiers.EOF);
                        callback.onChannelComplete(connection.iName,
                                connection.getUploadedBytes(),
                                System.currentTimeMillis() - startTime);
                    } else if (fileBlock == ReadFileCall.INTERRUPT) {
                        channel.writeShort(TransferIdentifiers.END_OF_INTERRUPTED);
                        // 「别的通道断了 / 被关了」：如果这次传输已经被判定为取消（电脑端点「取消」时
                        // 会先关掉全部传输通道），这只是取消的连带反应，不当错误报；
                        // 真断线（拔网线）时下面这个判据不成立，照旧报中断。
                        if (!isCancelledLike(null)) {
                            callback.onChannelError(connection.iName,TransferFileCallback.ERROR_TYPE_INTERRUPT, null);
                        }
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
                // 一整个块要写满（write 只保证写一部分）；分片写，边写边记字节，
                // 慢通道（蓝牙）的行才不会十几秒一动不动
                int blockLimit = fileBlock.data.limit();
                while (fileBlock.data.position() < blockLimit) {
                    int from = fileBlock.data.position();
                    int to = Math.min(blockLimit, from + SLICE_WRITE_BYTES);
                    fileBlock.data.limit(to);
                    try {
                        channel.writeFully(fileBlock.data);
                    } finally {
                        fileBlock.data.limit(blockLimit);
                    }
                    connection.addUploadedBytes(to - from);
                    // 汇报这条通道的真实进度，读线程据此判断谁快谁慢（蓝牙那种要特殊对待）
                    readFileCall.noteChannelBytes(channelIndex, connection.getUploadedBytes());
                }
                // 这一块真的发出去了：进度条按块记真值（分段切割线 + 每段填充都从这里来）
                // 先把要用的字段取出来，再回收缓冲区、清掉引用（原因见下面）
                final String sentPath = fileBlock.path;
                final int sentIndex = fileBlock.index;
                final long sentSize = fileBlock.totalSize;
                readFileCall.recycleBuffer(fileBlock.data);
                // 还回池子之后必须把这个引用清掉。
                //
                // 下面 noteBlockSent、下一轮的 pollOwnBlock、pause.awaitResume() 里任何一处
                // 抛异常，catch 里那句「回收没发完的缓冲区」就会把**同一个** ByteBuffer 再还
                // 一次 —— 池子里于是有两个条目指向同一块内存，两条通道同时往里读，
                // 发出去的块长度/内容就错位了（真机症状：跑久了才犯、大小一致但
                // SHA-256 不一致、偶发「块长度与文件栅格不符」的短块）。
                fileBlock = null;
                readFileCall.noteBlockSent(connection.iName, sentPath, sentIndex, sentSize);
            }
        } catch (Exception e) {
            //若发生异常，通知其他传输通道，停止传输
            top.weixiansen574.hybridfilexfer.core.HFXLog.d("SendFileCall[" + connection.iName + "] FAILED, wroteLast=" + fileBlock, e);
            if (fileBlock != null && fileBlock.data != null) {
                // 回收没发完的文件分块缓冲区。
                // 两个条件都不能省：
                //  · data == null：控制块（EOF / 中断 / 读错误那几个常量块）本来就没有缓冲区，
                //    直接 add(null) 会在 LinkedBlockingDeque 里抛 NPE，把真正的异常盖掉；
                //  · fileBlock 已经在正常路径回收过的话，这里再还一次就是双重回收
                //    （正常路径回收后会把 fileBlock 置 null，所以这里只会看到"没还过"的）。
                readFileCall.recycleBuffer(fileBlock.data);
                fileBlock = null;
            }
            readFileCall.shutdownByConnectionBreak();
            // 取消时通道是被我们自己或电脑端关掉的，ClosedChannelException /
            // AsynchronousCloseException 都是预期内的收尾，不该再弹「通道错误」。
            //
            // 这里的判据不能只看 connection.isCancelled()：电脑端点「取消 → 保留进度」是
            // 先 shutdown 传输通道、再发 CANCEL 帧，写异常往往比 CANCEL 早到几十毫秒，
            // 那一刻 isCancelled() 还是 false —— 于是被判成「传输时发生异常」，
            // 界面弹出「发送失败：传输时发生异常，请返回重连」（真机复现的就是这一条）。
            // isCancelledLike() 会等那一小会儿，让「对端取消」标志落位。
            if (!isCancelledLike(e)) {
                callback.onChannelError(connection.iName,TransferFileCallback.ERROR_TYPE_EXCEPTION, e.toString());
            }
            throw e;
        }
        return null;
    }

}
