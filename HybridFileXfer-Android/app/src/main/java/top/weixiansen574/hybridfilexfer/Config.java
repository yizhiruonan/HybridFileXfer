package top.weixiansen574.hybridfilexfer;

import android.content.Context;
import android.content.SharedPreferences;

public class Config {
    public static final int MODE_NORMAL = 0;
    public static final int MODE_ROOT = 1;
    private static Config instance;
    SharedPreferences preferences;

    public Config(SharedPreferences preferences) {
        this.preferences = preferences;
    }

    public static synchronized Config getInstance(Context context) {
        if (instance == null){
            instance = new Config(context.getApplicationContext().getSharedPreferences("config",Context.MODE_PRIVATE));
        }
        return instance;
    }

    public int getMode(){
        return preferences.getInt("mode",MODE_NORMAL);
    }

    public void setMode(int mode){
        preferences.edit().putInt("mode",mode).apply();
    }

    public int getServerPort() {
        return 5740;
    }

    /** 同时连接的电脑数量上限；每台电脑占用一份缓冲区块内存（默认 8 MiB） */
    public int getMaxConnections() {
        return Math.max(1, Math.min(5, preferences.getInt("max_connections", 3)));
    }

    public void setMaxConnections(int count) {
        preferences.edit().putInt("max_connections", Math.max(1, Math.min(5, count))).apply();
    }

    public int getLocalBufferCount() {
        return preferences.getInt("local_buffer_count",256);
    }

    public void setLocalBufferCount(int count){
        preferences.edit().putInt("local_buffer_count",count).apply();
    }

    public int getRemoteBufferCount() {
        return preferences.getInt("remote_buffer_count",512);
    }

    public void setRemoteBufferCount(int count){
        preferences.edit().putInt("remote_buffer_count",count).apply();
    }

    public void setClientIOMode(int mode){
        preferences.edit().putInt("client_io_mode",mode).apply();
    }

    public int getClientIOMode(){
        return preferences.getInt("client_io_mode", MODE_NORMAL);
    }

    public void setConnectServerControllerIp(String ip){
        preferences.edit().putString("connect_server_controller_ip",ip).apply();
    }

    public String getConnectServerControllerIp(){
        return preferences.getString("connect_server_controller_ip","");
    }

    /** 是否允许在后台自动等待电脑连接（插上数据线即连接） */
    public boolean isAutoConnectEnabled() {
        return preferences.getBoolean("auto_connect", true);
    }

    public void setAutoConnectEnabled(boolean enabled) {
        preferences.edit().putBoolean("auto_connect", enabled).apply();
    }

    /** 已启用的网卡名称，逗号分隔，例如 "USB_ADB,wlan0" */
    public String getEnabledInterfaceNames() {
        return preferences.getString("enabled_interfaces", "");
    }

    /** 界面缩放比例（0.8 - 1.1） */
    public float getUiScale() {
        return preferences.getFloat("ui_scale", 1.0f);
    }

    /** 电脑接收文件夹的子目录（可选），留空表示用电脑端设置的接收文件夹 */
    /** 手机端浏览电脑后选定的绝对接收目录，空表示用电脑端默认目录 */
    /** 每张网卡开几条传输通道（1-4），越大越能压满带宽 */
    /** 被单独关闭的传输通道名（如 wlan0#3），逗号分隔 */
    public java.util.Set<String> getDisabledChannels() {
        java.util.Set<String> set = new java.util.LinkedHashSet<>();
        String raw = preferences.getString("disabled_channels", "");
        for (String part : raw.split(",")) {
            if (!part.trim().isEmpty()) {
                set.add(part.trim());
            }
        }
        return set;
    }

    public void setDisabledChannels(java.util.Set<String> channels) {
        preferences.edit().putString("disabled_channels", String.join(",", channels)).apply();
    }

    /** 上次各通道的速率，格式 "wlan0=70.2,wlan0#2=68.9"（MB/s） */
    public String getChannelSpeeds() {
        return preferences.getString("channel_speeds", "");
    }

    public void setChannelSpeeds(String speeds) {
        preferences.edit().putString("channel_speeds", speeds).apply();
    }

    /** 上次传输的峰值速率（字节/秒），0 表示没有记录 */
    public long getLastPeakBytesPerSecond() {
        return preferences.getLong("last_peak_bps", 0L);
    }

    public void setLastPeakBytesPerSecond(long bytesPerSecond) {
        preferences.edit().putLong("last_peak_bps", bytesPerSecond).apply();
    }

    /**
     * 主题模式，对应 Miuix 0.9 的 ColorSchemeMode 枚举序号：
     * 0 跟随系统 / 1 浅色 / 2 深色 / 3 Monet（跟随系统）/ 4 Monet（浅色）/ 5 Monet（深色）
     */
    public int getThemeMode() {
        return preferences.getInt("theme_mode", 0);
    }

    public void setThemeMode(int mode) {
        preferences.edit().putInt("theme_mode", Math.max(0, Math.min(5, mode))).apply();
    }

    /** 限速（MB/s），0 表示不限速 */
    public int getSpeedLimitMbps() {
        return preferences.getInt("speed_limit_mbps", 0);
    }

    public void setSpeedLimitMbps(int mbps) {
        preferences.edit().putInt("speed_limit_mbps", Math.max(0, Math.min(1000, mbps))).apply();
    }

    public int getChannelsPerInterface() {
        return preferences.getInt("channels_per_interface", 2);
    }

    public void setChannelsPerInterface(int count) {
        preferences.edit().putInt("channels_per_interface", Math.max(1, Math.min(4, count))).apply();
    }

    public String getRemoteAbsDir() {
        return preferences.getString("remote_abs_dir", "");
    }

    public void setRemoteAbsDir(String dir) {
        preferences.edit().putString("remote_abs_dir", dir).apply();
    }

    public String getRemoteSubDir() {
        return preferences.getString("remote_sub_dir", "");
    }

    /** 电脑推送到手机时的保存目录（绝对路径，空表示默认 Downloads/HybridFileXfer） */
    public String getPushDir() {
        return preferences.getString("push_dir", "");
    }

    public void setPushDir(String dir) {
        preferences.edit().putString("push_dir", dir == null ? "" : dir).apply();
    }

    /** 更新来源仓库，格式 user/repo，默认原项目 */
    public String getUpdateRepo() {
        // 更新固定在项目自己的仓库，不再暴露给用户在设置里改
        return preferences.getString("update_repo", "yizhiruonan/HybridFileXfer");
    }

    public void setUpdateRepo(String repo) {
        preferences.edit().putString("update_repo", repo).apply();
    }

    public void setRemoteSubDir(String dir) {
        preferences.edit().putString("remote_sub_dir", dir).apply();
    }

    public void setUiScale(float scale) {
        preferences.edit().putFloat("ui_scale", scale).apply();
    }

    public void setEnabledInterfaceNames(String names) {
        preferences.edit().putString("enabled_interfaces", names).apply();
    }

    /** 各网卡指定的电脑IP，格式 "wlan0=192.168.1.100;wlan1=" */
    public String getInterfaceBindAddresses() {
        return preferences.getString("interface_bind_addresses", "");
    }

    public void setInterfaceBindAddresses(String value) {
        preferences.edit().putString("interface_bind_addresses", value).apply();
    }
}