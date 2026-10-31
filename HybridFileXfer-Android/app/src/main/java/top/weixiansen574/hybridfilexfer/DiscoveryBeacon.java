package top.weixiansen574.hybridfilexfer;

import android.content.Context;
import android.os.Build;
import android.util.Log;

import java.net.DatagramPacket;
import java.net.DatagramSocket;
import java.net.InetAddress;
import java.net.InterfaceAddress;
import java.net.NetworkInterface;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Enumeration;
import java.util.List;

/**
 * 局域网广播：告诉电脑"我在这里"。
 * <p>
 * 电脑端接收器同时监听 UDP 端口，发现手机后可以不用 adb、不用手动输 IP 就直接连接，
 * 这样没有数据线时也能自动走网络传输。协议很简单：
 * `HFX1 <控制端口> <设备名>`。
 */
public class DiscoveryBeacon {

    private static final String TAG = "DiscoveryBeacon";
    public static final int DISCOVERY_PORT = 5741;
    private static final String MAGIC = "HFX1";
    private static final long INTERVAL_MS = 2000;

    private final Context context;
    private final int serverPort;
    private Thread thread;
    private volatile boolean running;

    public DiscoveryBeacon(Context context, int serverPort) {
        this.context = context.getApplicationContext();
        this.serverPort = serverPort;
    }

    public synchronized void start() {
        if (running) {
            return;
        }
        running = true;
        thread = new Thread(this::loop, "HFX-Discovery");
        thread.setDaemon(true);
        thread.start();
    }

    public synchronized void stop() {
        running = false;
        Thread t = thread;
        thread = null;
        if (t != null) {
            t.interrupt();
        }
    }

    private void loop() {
        try (DatagramSocket socket = new DatagramSocket()) {
            socket.setBroadcast(true);
            String model = Build.MODEL == null ? "android" : Build.MODEL.replace(' ', '_');
            byte[] payload = (MAGIC + " " + serverPort + " " + model).getBytes(StandardCharsets.UTF_8);
            while (running) {
                for (InetAddress target : broadcastAddresses()) {
                    try {
                        socket.send(new DatagramPacket(payload, payload.length, target, DISCOVERY_PORT));
                    } catch (Exception e) {
                        Log.d(TAG, "send to " + target + " failed: " + e);
                    }
                }
                try {
                    Thread.sleep(INTERVAL_MS);
                } catch (InterruptedException e) {
                    break;
                }
            }
        } catch (Exception e) {
            Log.w(TAG, "discovery beacon stopped", e);
        }
    }

    private List<InetAddress> broadcastAddresses() {
        List<InetAddress> list = new ArrayList<>();
        try {
            list.add(InetAddress.getByName("255.255.255.255"));
            Enumeration<NetworkInterface> interfaces = NetworkInterface.getNetworkInterfaces();
            while (interfaces.hasMoreElements()) {
                NetworkInterface networkInterface = interfaces.nextElement();
                if (!networkInterface.isUp() || networkInterface.isLoopback()) {
                    continue;
                }
                for (InterfaceAddress address : networkInterface.getInterfaceAddresses()) {
                    InetAddress broadcast = address.getBroadcast();
                    if (broadcast != null) {
                        list.add(broadcast);
                    }
                }
            }
        } catch (Exception e) {
            Log.d(TAG, "cannot list broadcast addresses: " + e);
        }
        return list;
    }
}
