package top.weixiansen574.hybridfilexfer;

import android.content.Context;
import android.text.TextUtils;

import java.net.Inet4Address;
import java.net.InetAddress;
import java.net.NetworkInterface;
import java.net.SocketException;
import java.net.UnknownHostException;
import java.util.ArrayList;
import java.util.Enumeration;
import java.util.List;

import top.weixiansen574.hybridfilexfer.core.bean.ServerNetInterface;

/**
 * 网卡枚举与用户选择的持久化。
 */
public final class NetInterfaces {

    private NetInterfaces() {
    }

    /**
     * 枚举所有可用的IPv4网卡；插着数据线（USB 供电）时第一个是 USB_ADB（adb 转发到 127.0.0.1）。
     *
     * <p>没插线时不再把 USB_ADB 列进计划里——否则拔线后界面上还挂着 USB 通道。
     */
    public static ArrayList<NetInterfaceInfo> enumerate(Context context)
            throws SocketException, UnknownHostException {
        ArrayList<NetInterfaceInfo> netInterfaceList = new ArrayList<>();
        if (usbCablePresent(context)) {
            netInterfaceList.add(new NetInterfaceInfo(NetInterfaceInfo.USB_ADB, InetAddress.getByName("127.0.0.1")));
        }
        Enumeration<NetworkInterface> networkInterfaces = NetworkInterface.getNetworkInterfaces();
        while (networkInterfaces.hasMoreElements()) {
            NetworkInterface networkInterface = networkInterfaces.nextElement();
            Enumeration<InetAddress> inetAddresses = networkInterface.getInetAddresses();
            while (inetAddresses.hasMoreElements()) {
                InetAddress address = inetAddresses.nextElement();
                String ifName = networkInterface.getDisplayName();
                // 数据流量（运营商 NAT）在电脑端连不进来，不要列进计划里：
                // 高通/联发科/三星的蜂窝网卡名字分别是 rmnet*、ccmni*、pdp_ip*
                boolean cellular = ifName.startsWith("rmnet") || ifName.startsWith("ccmni")
                        || ifName.startsWith("pdp_ip");
                if (!address.isLoopbackAddress() && address instanceof Inet4Address && !cellular) {
                    NetInterfaceInfo item = new NetInterfaceInfo(networkInterface.getDisplayName(), address);
                    //VPN开的虚拟网卡，默认不勾选
                    if (item.name.startsWith("tun")) {
                        item.enabled = false;
                    }
                    netInterfaceList.add(item);
                }
            }
        }
        return netInterfaceList;
    }

    /**
     * 蓝牙共享网络（PAN）的网卡：安卓上一般叫 bt-pan，个别 ROM 是 bnep0。
     *
     * <p>它不是"真网卡"：手机开「蓝牙网络共享」、电脑用蓝牙连上手机之后才会出现，
     * 走的是 BNEP 上的以太网帧，带宽只有几 Mbps（够用但慢），所以只当补充通道。
     */
    public static boolean isBluetooth(String ifName) {
        String name = ifName.toLowerCase(java.util.Locale.US);
        return name.startsWith("bt-pan") || name.startsWith("bt_pan") || name.startsWith("bnep")
                || name.startsWith("btpan");
    }

    /** 设置界面里显示的名字（比 bt-pan / wlan0 好认），传输通道名仍然用原名当键 */
    public static String label(String ifName) {
        if (NetInterfaceInfo.USB_ADB.equals(ifName)) {
            return "USB 数据线";
        }
        if (isBluetooth(ifName)) {
            return "蓝牙共享网络";
        }
        String name = ifName.toLowerCase(java.util.Locale.US);
        if (name.startsWith("wlan") || name.startsWith("wifi")) {
            return "Wi-Fi";
        }
        if (name.startsWith("eth")) {
            return "以太网";
        }
        if (name.startsWith("rndis") || name.startsWith("usb")) {
            return "USB 网络共享";
        }
        return ifName;
    }

    /** 数据线插着吗（USB 供电）；查不到时按插着处理，保持旧行为 */
    private static boolean usbCablePresent(Context context) {
        try {
            android.content.Intent intent = context.registerReceiver(null,
                    new android.content.IntentFilter(android.content.Intent.ACTION_BATTERY_CHANGED));
            if (intent == null) {
                return true;
            }
            int plugged = intent.getIntExtra(android.os.BatteryManager.EXTRA_PLUGGED, 0);
            return plugged == android.os.BatteryManager.BATTERY_PLUGGED_USB
                    || plugged == android.os.BatteryManager.BATTERY_PLUGGED_AC;
        } catch (Throwable e) {
            return true;
        }
    }

    /**
     * 把保存的勾选状态/指定IP应用到刚枚举出来的列表上。
     */
    public static void applySaved(Context context, List<NetInterfaceInfo> list) {
        Config config = Config.getInstance(context);
        // 记住的是「被关掉的」，不是「被开着的」：蓝牙共享网络（bt-pan / bnep0）这类网卡是随用随出现的，
        // 用户以前保存的启用列表里永远不会有它，按老逻辑就一辈子开不起来（用户要的「加上蓝牙通道」）。
        String disabledNames = config.getDisabledInterfaceNames();
        boolean hasSaved = !TextUtils.isEmpty(disabledNames);
        List<String> disabled = hasSaved ? split(disabledNames, ',') : new ArrayList<String>();
        for (NetInterfaceInfo item : list) {
            item.enabled = !hasSaved || !disabled.contains(item.name);
            // 还没手动配置过时沿用老默认：VPN 的 tun 网卡不勾
            if (!hasSaved && item.name.startsWith("tun")) {
                item.enabled = false;
            }
        }
        String binds = config.getInterfaceBindAddresses();
        if (!TextUtils.isEmpty(binds)) {
            for (String entry : split(binds, ';')) {
                int index = entry.indexOf('=');
                if (index <= 0) {
                    continue;
                }
                String name = entry.substring(0, index);
                String bind = entry.substring(index + 1);
                for (NetInterfaceInfo item : list) {
                    if (item.name.equals(name)) {
                        item.bindAddress = bind;
                    }
                }
            }
        }
    }

    /**
     * 保存当前勾选状态与指定IP，供后台自动连接服务使用。
     */
    public static void save(Context context, List<NetInterfaceInfo> list) {
        StringBuilder enabled = new StringBuilder();
        StringBuilder disabled = new StringBuilder();
        StringBuilder binds = new StringBuilder();
        for (NetInterfaceInfo item : list) {
            if (item.enabled) {
                if (enabled.length() > 0) {
                    enabled.append(',');
                }
                enabled.append(item.name);
            }
            if (!TextUtils.isEmpty(item.bindAddress)) {
                if (binds.length() > 0) {
                    binds.append(';');
                }
                binds.append(item.name).append('=').append(item.bindAddress.trim());
            }
            if (!item.enabled) {
                if (disabled.length() > 0) {
                    disabled.append(',');
                }
                disabled.append(item.name);
            }
        }
        Config config = Config.getInstance(context);
        config.setEnabledInterfaceNames(enabled.toString());
        config.setDisabledInterfaceNames(disabled.toString());
        config.setInterfaceBindAddresses(binds.toString());
    }

    public static List<ServerNetInterface> selected(List<NetInterfaceInfo> list)
            throws UnknownHostException {
        List<ServerNetInterface> serverNetInterfaces = new ArrayList<>();
        for (NetInterfaceInfo item : list) {
            if (item.enabled) {
                serverNetInterfaces.add(item.toServerNetInterface());
            }
        }
        return serverNetInterfaces;
    }

    /**
     * 后台服务要使用的网卡列表（自适应）。
     * 用户没有手动勾选过时，把所有可用网卡都报给电脑：USB_ADB、WIFI、以太网……
     * 电脑那边能连上哪些就用哪些，连不上的通道会被自动跳过，
     * 于是"数据线 + WIFI"可以同时跑，只有WIFI时也能正常用。
     * 用户勾选过则按用户的选择来。
     */
    public static List<ServerNetInterface> forAutoConnect(Context context)
            throws SocketException, UnknownHostException {
        List<NetInterfaceInfo> list = enumerate(context);
        // 勾选状态只有一处来源：没被显式关掉的都算启用（tun 除外，见 applySaved）
        applySaved(context, list);
        return selected(list);
    }

    private static List<String> split(String value, char separator) {
        List<String> result = new ArrayList<>();
        int start = 0;
        for (int i = 0; i < value.length(); i++) {
            if (value.charAt(i) == separator) {
                result.add(value.substring(start, i));
                start = i + 1;
            }
        }
        result.add(value.substring(start));
        return result;
    }
}