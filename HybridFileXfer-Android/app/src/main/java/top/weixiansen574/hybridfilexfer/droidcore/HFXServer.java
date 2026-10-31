package top.weixiansen574.hybridfilexfer.droidcore;

import android.os.RemoteException;

import java.io.IOException;
import java.net.InetSocketAddress;
import java.nio.ByteBuffer;
import java.nio.channels.ServerSocketChannel;
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
    public static HFXServer instance;
    protected final IIOService ioService;
    /**
     * 控制通道的写锁。心跳线程与传输线程共用它，避免心跳字节插到一次传输的
     * 指令/回复之间，破坏协议。
     */
    public final Object controlLock = new Object();
    protected ServerSocketChannel serverSocketChannel;
    protected int remoteFileSystem;
    protected String remoteHomeDir;

    public HFXServer(IIOService ioService) {
        this.ioService = ioService;
    }

    public void startServer(int port, List<ServerNetInterface> interfaceList, int localBufferCount, int remoteBufferCount, StartServerCallback callback) throws IOException {
        ServerSocketChannel serverSocketChannel;
        try {
            serverSocketChannel = ServerSocketChannel.open().bind(new InetSocketAddress(port));
        } catch (IOException e) {
            callback.onBindFailed(port);
            return;
        }
        this.serverSocketChannel = serverSocketChannel;
        callback.onStatedServer();
        //控制通道
        DataByteChannel ctChannel;
        //传输通道
        List<TransferConnection> connections;
        //第一此accept的为控制通道
        conn:
        while (true) {
            //协议判断
            ctChannel = new DataByteChannel(serverSocketChannel.accept());
            byte[] headerBytes = HFXServer.CLIENT_HEADER.getBytes(StandardCharsets.UTF_8);
            byte[] header = new byte[headerBytes.length];
            ctChannel.readFully(header);
            if (!Arrays.equals(headerBytes, header)) {
                ctChannel.writeBytes("protocol error\n");
                ctChannel.close();
                continue;
            }
            //版本判断
            int versionCode = ctChannel.readInt();
            if (versionCode != HFXServer.VERSION_CODE) {
                //返回当前服务端版本信息
                ctChannel.writeBoolean(false);//版本未正确匹配
                ctChannel.writeInt(HFXServer.VERSION_CODE);
                ctChannel.close();
                continue;
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
                    //accept通道
                    connections.add(new TransferConnection(name, new DataByteChannel(serverSocketChannel.accept())));
                    ctChannel.writeBoolean(true);
                    callback.onAccepted(name);
                } else {
                    ctChannel.writeBoolean(false);
                    callback.onAcceptFailed(name);
                }
            }
            if (connections.isEmpty()) {
                //一条都没连上，回去等电脑重新连接
                ctChannel.close();
                callback.onError(new IOException("no transfer channel could be established"));
                continue conn;
            }
            break;
        }

        LinkedBlockingDeque<ByteBuffer> buffers = this.buffers;
        //告知对方创建的缓冲区块数量
        ctChannel.writeInt(remoteBufferCount);
        if (!ctChannel.readBoolean()) {
            callback.onPcOOM();
            serverSocketChannel.close();
            return;
        }
        //创建缓冲区块
        for (int i = 0; i < localBufferCount; i++) {
            ByteBuffer buffer = NativeMemory.allocateLargeBuffer(FileBlock.BLOCK_SIZE);
            if (buffer != null) {
                buffers.add(buffer);
            } else {
                //释放缓冲区块内存
                for (ByteBuffer buff : buffers) {
                    NativeMemory.freeBuffer(buff);
                }
                buffers.clear();
                ctChannel.writeBoolean(false);
                serverSocketChannel.close();
                callback.onMeOOM(i, localBufferCount);
                return;
            }
        }
        ctChannel.writeBoolean(true);
        //读取对方文件系统信息
        this.remoteFileSystem = ctChannel.readInt();
        //读取对方设定的主目录
        this.remoteHomeDir = ctChannel.readUTF();
        this.ctChannel = ctChannel;
        this.connections = connections;
        callback.onConnectSuccess();
    }

    public void closeServerSocket() {
        if (serverSocketChannel != null) {
            try {
                serverSocketChannel.close();
            } catch (IOException e) {
                e.printStackTrace();
            }
        }
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
            if (connections != null) {
                for (TransferConnection connection : connections) {
                    if (connection == null || connection.channel == null) {
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
        try {
            if (connections != null) {
                for (TransferConnection connection : connections) {
                    if (connection == null || connection.channel == null) {
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
        List<String> names = new ArrayList<>(connections.size());
        for (TransferConnection connection : connections) {
            names.add(connection.iName);
        }
        return names;
    }

    public void disconnect(){
        //释放缓冲区块内存
        for (ByteBuffer buffer : buffers) {
            NativeMemory.freeBuffer(buffer);
        }
        buffers.clear();
        if (ctChannel != null) {
            try {
                //通知断开连接
                ctChannel.writeShort(ControllerIdentifiers.SHUTDOWN);
            } catch (IOException ignored) {
            }
            //释放控制通道，避免反复重连时残留socket
            try {
                ctChannel.close();
            } catch (IOException ignored) {
            }
            ctChannel = null;
        }
        if (serverSocketChannel != null) {
            try {
                serverSocketChannel.close();
            } catch (IOException ignored) {
            }
        }
        //关闭所有用于传输的连接
        if (connections != null) {
            for (TransferConnection connection : connections) {
                try {
                    connection.close();
                } catch (IOException ignored) {
                }
            }
        }
    }

    public void close(){
        //释放并清理缓冲区块
        for (ByteBuffer buffer : buffers) {
            NativeMemory.freeBuffer(buffer);
        }
        buffers.clear();
        //关闭serverSocket，释放端口绑定，不用管其他accept的Socket
        if (serverSocketChannel != null) {
            try {
                serverSocketChannel.close();
            } catch (IOException e) {
                e.printStackTrace();
            }
        }
    }

    @Override
    protected WriteFileCall createWriteFileCall(LinkedBlockingDeque<ByteBuffer> buffers, int dequeCount) {
        return new DroidWriteFileCall(buffers,dequeCount,ioService);
    }

    @Override
    protected ReadFileCall createReadFileCall(LinkedBlockingDeque<ByteBuffer> buffers, List<RemoteFile> files, Directory localDir, Directory remoteDir, int operateThreadCount) {
        return new DroidReadFileCall(ioService,buffers,files,localDir,remoteDir,operateThreadCount);
    }
}