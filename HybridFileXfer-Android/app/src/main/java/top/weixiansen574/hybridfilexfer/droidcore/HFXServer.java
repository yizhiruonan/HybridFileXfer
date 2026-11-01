package top.weixiansen574.hybridfilexfer.droidcore;

import android.os.RemoteException;

import java.io.IOException;
import java.net.InetSocketAddress;
import java.nio.ByteBuffer;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.FutureTask;
import java.util.concurrent.LinkedBlockingDeque;

import top.weixiansen574.async.BackstageTask;
import top.weixiansen574.hybridfilexfer.NativeMemory;
import top.weixiansen574.hybridfilexfer.aidl.IIOService;
import top.weixiansen574.hybridfilexfer.core.BlockProgress;
import top.weixiansen574.hybridfilexfer.core.ControllerIdentifiers;
import top.weixiansen574.hybridfilexfer.core.FileBlock;
import top.weixiansen574.hybridfilexfer.core.HFXService;
import top.weixiansen574.hybridfilexfer.core.ReadFileCall;
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
    /** 等一条传输通道的超时（电脑端整体握手超时是 15s） */
    private static final int TRANSFER_WAIT_MS = 12000;

    protected final IIOService ioService;
    /**
     * 控制通道的写锁。心跳线程与传输线程共用它，避免心跳字节插到一次传输的
     * 指令/回复之间，破坏协议。
     */
    public final Object controlLock = new Object();
    protected int remoteFileSystem;
    protected String remoteHomeDir;

    /** 电脑推送文件时的分块进度（给进度浮层画分块进度条） */
    public final BlockProgress blockProgress = new BlockProgress();

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
        this.ctChannel = ctChannel;
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
            connections = new ArrayList<>(interfaceList.size());
            for (int i = 0; i < interfaceList.size(); i++) {
                //对方连接通道是否成功
                boolean succeed = ctChannel.readBoolean();
                //对方所连接通道的名称（由控制器通道发送名称）
                String name = ctChannel.readUTF();
                if (succeed) {
                    //从共享套接字上取一条传输通道（别的电脑的控制通道会被暂存，不会认错）
                    SocketChannel socket = listener.takeTransfer(TRANSFER_WAIT_MS);
                    if (socket == null) {
                        ctChannel.writeBoolean(false);
                        throw new IOException("waiting for transfer channel " + name + " timed out");
                    }
                    connections.add(new TransferConnection(name, new DataByteChannel(socket)));
                    ctChannel.writeBoolean(true);
                    callback.onAccepted(name);
                } else {
                    ctChannel.writeBoolean(false);
                    callback.onAcceptFailed(name);
                }
            }
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
            ctChannel.writeShort(ControllerIdentifiers.CHECK_PENDING);
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

    public void sendFilesToRemote(List<RemoteFile> files, Directory localDir, Directory remoteDir, TransferFileCallback callback) throws IOException {
        synchronized (controlLock) {
            ctChannel.writeShort(ControllerIdentifiers.REQUEST_RECEIVE);//请求对方接收
            sendFiles(files, localDir, remoteDir, callback);
        }
    }

    public void sendFilesToShelf(List<RemoteFile> files, Directory localDir, Directory remoteDir, TransferFileCallback callback) throws IOException {
        synchronized (controlLock) {
            ctChannel.writeShort(ControllerIdentifiers.REQUEST_SEND);
            ctChannel.writeInt(files.size());
            for (RemoteFile file : files) {
                ctChannel.writeUTF(file.getPath());
            }
            ctChannel.writeUTF(localDir.path);
            ctChannel.writeInt(localDir.fileSystem);
            ctChannel.writeUTF(remoteDir.path);
            receiveFiles(callback);
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
            return doListClientFiles(path);
        }
    }

    private List<RemoteFile> doListClientFiles(String path) throws IOException {
        ctChannel.writeShort(ControllerIdentifiers.LIST_FILES);
        ctChannel.writeUTF(path);
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
            ctChannel.writeShort(ControllerIdentifiers.DELETE_FILE);
            ctChannel.writeUTF(file);
            return ctChannel.readBoolean();
        }
    }

    public boolean createRemoteDir(String parent, String child) throws IOException {
        synchronized (controlLock) {
            ctChannel.writeShort(ControllerIdentifiers.MKDIR);
            ctChannel.writeUTF(parent);
            ctChannel.writeUTF(child);
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
        unregister(this);
        releaseBuffers();
        DataByteChannel channel = ctChannel;
        ctChannel = null;
        if (channel != null) {
            try {
                //通知断开连接
                channel.writeShort(ControllerIdentifiers.SHUTDOWN);
            } catch (IOException ignored) {
            }
            //释放控制通道，避免反复重连时残留socket
            try {
                channel.close();
            } catch (IOException ignored) {
            }
        }
        closeConnections(connections);
        connections = null;
    }

    /** 旧接口：等价于断开本连接（监听套接字由 ServerService 统一管理）。 */
    public void close() {
        disconnect();
    }

    @Override
    protected WriteFileCall createWriteFileCall(LinkedBlockingDeque<ByteBuffer> buffers, int dequeCount) {
        return new DroidWriteFileCall(buffers, dequeCount, ioService);
    }

    @Override
    protected ReadFileCall createReadFileCall(LinkedBlockingDeque<ByteBuffer> buffers, List<RemoteFile> files, Directory localDir, Directory remoteDir, int operateThreadCount) {
        return new DroidReadFileCall(ioService,buffers,files,localDir,remoteDir,operateThreadCount);
    }
}