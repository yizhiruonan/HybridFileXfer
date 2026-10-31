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
     * 枚举所有可用的IPv4网卡，第一个固定为USB_ADB（adb转发到127.0.0.1）。
     */
    public static ArrayList<NetInterfaceInfo> enumerate(Context context)
            throws SocketException, UnknownHostException {
        ArrayList<NetInterfaceInfo> netInterfaceList = new ArrayList<>();
        netInterfaceList.add(new NetInterfaceInfo(NetInterfaceInfo.USB_ADB, InetAddress.getByName("127.0.0.1")));
        Enumeration<NetworkInterface> networkInterfaces = NetworkInterface.getNetworkInterfaces();
        while (networkInterfaces.hasMoreElements()) {
            NetworkInterface networkInterface = networkInterfaces.nextElement();
            Enumeration<InetAddress> inetAddresses = networkInterface.getInetAddresses();
            while (inetAddresses.hasMoreElements()) {
                InetAddress address = inetAddresses.nextElement();
                if (!address.isLoopbackAddress() && address instanceof Inet4Address
                        && !networkInterface.getDisplayName().startsWith("rmnet_data"/*数据流量除外*/)) {
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
     * 把保存的勾选状态/指定IP应用到刚枚举出来的列表上。
     */
    public static void applySaved(Context context, List<NetInterfaceInfo> list) {
        Config config = Config.getInstance(context);
        String enabledNames = config.getEnabledInterfaceNames();
        if (!TextUtils.isEmpty(enabledNames)) {
            List<String> names = split(enabledNames, ',');
            for (NetInterfaceInfo item : list) {
                item.enabled = names.contains(item.name);
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
        }
        Config config = Config.getInstance(context);
        config.setEnabledInterfaceNames(enabled.toString());
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
        if (TextUtils.isEmpty(Config.getInstance(context).getEnabledInterfaceNames())) {
            for (NetInterfaceInfo item : list) {
                // VPN 的 tun 网卡默认不用
                item.enabled = !item.name.startsWith("tun");
            }
        } else {
            applySaved(context, list);
        }
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