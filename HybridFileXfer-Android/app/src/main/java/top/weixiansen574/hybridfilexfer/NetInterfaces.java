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

    /**
     * 有没有一条<b>能跑数据</b>的 USB 线（不是「有没有外部电源」）。
     *
     * <p>真机踩过：插墙充（AC）时这里也回 true（旧判据里带着 {@code BATTERY_PLUGGED_AC}），
     * 于是 {@link #enumerate} 凭空多列一条 {@code USB_ADB} —— 界面通道数虚高（12 条显示 13 条），
     * 那条通道万一被派了数据还会白连一次、失败后重试（用户看到的就是「有一条通道连不上」）。
     *
     * <p>判据分两层：
     * <ol>
     *   <li><b>必须走 USB 口供电</b>（{@code BATTERY_PLUGGED_USB}）：墙充走 AC、无线充电走
     *       WIRELESS，这两种都<b>不可能</b>有数据线，一律不报 USB_ADB；</li>
     *   <li><b>这条 USB 链路要真的被主机配置起来</b>（gadget 的 UDC 状态 = configured/suspended）。
     *       只看供电分不清「插电脑」和「插充电宝」—— 不少充电宝报的也是 USB 供电，而 USB_ADB
     *       通道靠 adb 转发，只有设备被电脑枚举之后才通。</li>
     * </ol>
     *
     * <p><b>{@code suspended} 为什么也算已连接</b>：宿主（电脑）睡眠 / 忙到不发 SOF 时，
     * UDC 状态就是 suspended —— 线还插着、电脑一醒就能通。这时候照旧报 USB_ADB 是<b>有意的</b>：
     * 电脑睡着的那段时间这条通道本来就连不上（跟电脑卡死分不开），代价是一次重连；
     * 而不报要等到「重新枚举」才回来，用户唤醒电脑后可能就一直少这条最快的通道。
     * 充电器不会走到 {@code suspended}（没枚举过），所以不会因此多报。
     *
     * <p><b>读不到系统节点怎么办</b>：第 1 层（AC / 无线充电 → 不报）不依赖任何系统节点，
     * 所以「插墙充多一条 USB 通道」这个 bug 在老内核、SELinux 严格的机型上照样修好；
     * 会退化的只有第 2 层（插充电宝那种「USB 供电但不是主机」），此时以
     * {@code BATTERY_PLUGGED_USB} 的结果为准 —— 也就是当作有数据线。
     * <b>绝不在这里回 false</b>：那会把真插着数据线的用户最快的那条通道判掉
     * （老 gadget / sysfs 被收紧的机型上一半速度就没了），代价比多一条连不上的通道大得多。
     *
     * <p>第 2 层读的是 {@code /sys/class/udc}（configfs gadget，内核 3.10+ / Android 6+ 基本都有）。
     * 老 gadget（{@code android_usb}）没有这个节点、个别厂商把 sysfs 收紧了，都只会「读不到」，
     * 不会误判成「没插」。真插数据线时行为不变。
     */
    private static boolean usbCablePresent(Context context) {
        try {
            android.content.Intent intent = context.registerReceiver(null,
                    new android.content.IntentFilter(android.content.Intent.ACTION_BATTERY_CHANGED));
            if (intent == null) {
                return true;
            }
            int plugged = intent.getIntExtra(android.os.BatteryManager.EXTRA_PLUGGED, 0);
            // 不是 USB 口供电 = 墙充 / 无线充电 / 没插：都不可能是数据线
            if (plugged != android.os.BatteryManager.BATTERY_PLUGGED_USB) {
                return false;
            }
            Boolean configured = usbLinkConfigured();
            return configured == null || configured;
        } catch (Throwable e) {
            return true;
        }
    }

    /**
     * USB 设备模式这条链路是不是已经被主机配置起来了（= 真数据线）。
     *
     * <p>读 USB gadget 的 UDC 状态（{@code /sys/class/udc/<控制器>/state}）：{@code configured}
     * 表示电脑已经把设备配置好（adb 转发就是这时候才通），{@code suspended} 是主机省电挂起
     * （线还连着，随时能通）；其余 {@code not attached} / {@code powered} / {@code default}
     * 只是在供电或者刚接触。
     *
     * <p>多个控制器时要看<b>全部</b>：任一报 configured/suspended 就算真数据线。只看第一条不行 ——
     * 多控制器机型（dwc3 + 厂商自定义 UDC 之类）上第一条恰好 not attached、第二条才是真链路时，
     * 会把真插着的数据线判掉，正好违反调用方「绝不误判成没插」的约定。
     *
     * @return null = 一个控制器都读不到（老内核 / 权限被挡），调用方按旧行为处理
     */
    private static Boolean usbLinkConfigured() {
        boolean readAny = false;
        try {
            java.io.File udcDir = new java.io.File("/sys/class/udc");
            String[] controllers = udcDir.list();
            if (controllers == null || controllers.length == 0) {
                return null;
            }
            for (String controller : controllers) {
                String state = firstLine(new java.io.File(new java.io.File(udcDir, controller), "state"));
                if (state == null) {
                    continue;
                }
                state = state.trim().toLowerCase(java.util.Locale.US);
                if (state.isEmpty()) {
                    continue;
                }
                readAny = true;
                if ("configured".equals(state) || "suspended".equals(state)) {
                    return Boolean.TRUE;
                }
            }
        } catch (Throwable ignored) {
            // 读不到就当不知道，交给调用方按旧行为处理
        }
        // 读到了、但每一条都不是「已配置/挂起」→ 确实只是供电（插充电宝）
        return readAny ? Boolean.FALSE : null;
    }

    /** 读文件第一行；读不到返回 null */
    private static String firstLine(java.io.File file) {
        java.io.BufferedReader reader = null;
        try {
            reader = new java.io.BufferedReader(new java.io.FileReader(file));
            return reader.readLine();
        } catch (Throwable e) {
            return null;
        } finally {
            if (reader != null) {
                try {
                    reader.close();
                } catch (Throwable ignored) {
                }
            }
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
        // 「被显式勾上过」的蓝牙网卡：蓝牙共享网络默认不勾，只认这份单独的记录
        String btNames = config.getEnabledBluetoothNames();
        List<String> btEnabled = TextUtils.isEmpty(btNames)
                ? new ArrayList<String>() : split(btNames, ',');
        for (NetInterfaceInfo item : list) {
            if (isBluetooth(item.name)) {
                // 蓝牙共享网络（PAN）只有 0.2~0.3 MB/s，多轨切分时是最慢那条，会拖住收尾，
                // 所以默认不勾；想用它就在设置里勾上，勾过之后按勾选状态记住。
                item.enabled = btEnabled.contains(item.name);
                continue;
            }
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
        StringBuilder bluetooth = new StringBuilder();
        for (NetInterfaceInfo item : list) {
            if (isBluetooth(item.name) && item.enabled) {
                // 蓝牙通道的勾选状态单独记一份（默认不勾），见 Config.getEnabledBluetoothNames()
                if (bluetooth.length() > 0) {
                    bluetooth.append(',');
                }
                bluetooth.append(item.name);
            }
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
        config.setEnabledBluetoothNames(bluetooth.toString());
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