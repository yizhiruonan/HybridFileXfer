package top.weixiansen574.hybridfilexfer.droidcore;

import java.io.IOException;
import java.util.List;

import top.weixiansen574.async.BackstageTask;
import top.weixiansen574.hybridfilexfer.core.bean.ServerNetInterface;
import top.weixiansen574.hybridfilexfer.droidcore.callback.StartServerCallback;

public class StartServerTask extends BackstageTask<StartServerCallback> {
    private final HFXServer server;
    private final int port;
    private final List<ServerNetInterface> interfaceList;
    private final int localBufferCount;
    private final int remoteBufferCount;

    public StartServerTask(StartServerCallback uiHandler, HFXServer server, int port, List<ServerNetInterface> interfaceList, int localBufferCount, int remoteBufferCount) {
        super(uiHandler);
        this.server = server;
        this.port = port;
        this.interfaceList = interfaceList;
        this.localBufferCount = localBufferCount;
        this.remoteBufferCount = remoteBufferCount;
    }

    @Override
    protected void onStart(StartServerCallback callback) throws Throwable {
        // 旧的一次性启动入口：绑定共享端口、等一台电脑接入。
        // 后台服务（ServerService）已经接管了常驻监听，这里只保留给旧界面用。
        ServerListener listener = new ServerListener(HFXServer.bindShared(port));
        boolean connected = false;
        try {
            ServerListener.PendingControl control = listener.nextControl(60000);
            if (control == null) {
                throw new IOException("timed out waiting for the computer");
            }
            connected = server.startServer(listener, control, interfaceList,
                    localBufferCount, remoteBufferCount, callback);
        } finally {
            listener.close();
            HFXServer.closeShared();
        }
        if (!connected) {
            throw new IOException("handshake failed");
        }
    }

    @Override
    protected void onError(Throwable th) {
        server.close();
    }
}
