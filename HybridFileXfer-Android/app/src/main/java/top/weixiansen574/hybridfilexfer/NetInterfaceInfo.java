package top.weixiansen574.hybridfilexfer;

import android.text.TextUtils;

import java.net.InetAddress;
import java.net.UnknownHostException;

import top.weixiansen574.hybridfilexfer.core.bean.ServerNetInterface;

/**
 * 一块可用于传输的网卡（服务端视角）。
 * 与界面状态分开保存，便于持久化用户的勾选。
 */
public class NetInterfaceInfo {
    public static final String USB_ADB = "USB_ADB";

    public final String name;
    public final InetAddress address;
    /** 电脑端指定网卡IP，空字符串表示由系统默认分配 */
    public String bindAddress = "";
    public boolean enabled = true;
    /** 仅用于界面显示的状态文本 */
    public String state = "";

    public NetInterfaceInfo(String name, InetAddress address) {
        this.name = name;
        this.address = address;
    }

    public boolean isUsbAdb() {
        return USB_ADB.equals(name);
    }

    public ServerNetInterface toServerNetInterface() throws UnknownHostException {
        return new ServerNetInterface(name, address,
                TextUtils.isEmpty(bindAddress) ? null : InetAddress.getByName(bindAddress));
    }

    @Override
    public String toString() {
        return "NetInterfaceInfo{" + name + ", " + address + ", enabled=" + enabled + '}';
    }
}
