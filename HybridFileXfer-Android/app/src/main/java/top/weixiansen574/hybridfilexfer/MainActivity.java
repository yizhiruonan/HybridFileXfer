package top.weixiansen574.hybridfilexfer;

import android.Manifest;
import android.app.ActivityManager;
import android.app.AlertDialog;
import android.content.ComponentName;
import android.content.Context;
import android.content.DialogInterface;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.os.Environment;
import android.provider.Settings;
import android.text.TextUtils;
import android.util.Log;
import android.view.View;
import android.widget.Button;
import android.widget.EditText;
import android.widget.ImageView;
import android.widget.Spinner;
import android.widget.TextView;
import android.widget.Toast;

import androidx.annotation.Nullable;
import androidx.appcompat.app.AppCompatActivity;
import androidx.core.app.ActivityCompat;
import androidx.core.content.ContextCompat;
import androidx.core.view.WindowCompat;

import com.google.android.material.bottomnavigation.BottomNavigationView;
import com.google.android.material.card.MaterialCardView;
import androidx.recyclerview.widget.LinearLayoutManager;
import androidx.recyclerview.widget.RecyclerView;

import java.io.IOException;
import java.net.SocketException;
import java.net.UnknownHostException;
import java.util.List;

import rikka.shizuku.Shizuku;
import top.weixiansen574.hybridfilexfer.core.bean.ServerNetInterface;
import top.weixiansen574.hybridfilexfer.listadapter.NetCardsAdapter;

public class MainActivity extends AppCompatActivity implements View.OnClickListener, ServerService.Listener {
    public static final int REQUEST_CODE_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION = 1;
    public static final int REQUEST_CODE_TRANSFER = 2;
    public static final int REQUEST_CODE_NOTIFICATION_PERMISSION = 3;
    public static final int RESULT_CODE_SERVER_DISCONNECT = 1;
    private static final String TAG = "MainActivity";
    private NetCardsAdapter netCardsAdapter;
    Button startServerBtn;
    Button toTransfer;
    Context context;
    private Config config;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        setContentView(R.layout.activity_main);
        context = this;
        config = Config.getInstance(context);
        WindowCompat.getInsetsController(getWindow(), getWindow().getDecorView())
                .setAppearanceLightStatusBars(!isNightMode());
        startServerBtn = findViewById(R.id.start_server);
        startServerBtn.setOnClickListener(this);
        toTransfer = findViewById(R.id.to_transfer);
        toTransfer.setOnClickListener(this);
        findViewById(R.id.refresh).setOnClickListener(this);

        RecyclerView recyclerView = findViewById(R.id.rec_view_net_cards);
        LinearLayoutManager layoutManager = new LinearLayoutManager(this);
        layoutManager.setOrientation(RecyclerView.VERTICAL);
        recyclerView.setLayoutManager(layoutManager);

        try {
            netCardsAdapter = new NetCardsAdapter(this);
            recyclerView.setAdapter(netCardsAdapter);
        } catch (IOException e) {
            Toast.makeText(this, e.getMessage(), Toast.LENGTH_SHORT).show();
            startServerBtn.setEnabled(false);
        }

        setupBottomNav();
        setupSettingsViews();

        requestNotificationPermission();
        if (!hasStoragePermission()) {
            promptForStoragePermission();
        } else {
            autoStartServerIfNeeded();
        }
    }

    /**
     * 自动连接、"发送到电脑"都需要读写文件的权限，第一次打开时提示一下。
     */
    private void promptForStoragePermission() {
        new AlertDialog.Builder(context)
                .setTitle(R.string.xu_yao_wen_jian_du_xie_quan_xian)
                .setMessage(R.string.storage_permission_message)
                .setPositiveButton(R.string.ok, (dialog, which) -> checkPermissionOrRequest())
                .setNegativeButton(R.string.cancel, null)
                .show();
    }

    private void setupBottomNav() {
        BottomNavigationView bottomNav = findViewById(R.id.bottom_nav);
        bottomNav.setOnItemSelectedListener(item -> {
            boolean home = item.getItemId() == R.id.tab_home;
            findViewById(R.id.view_home).setVisibility(home ? View.VISIBLE : View.GONE);
            findViewById(R.id.view_settings).setVisibility(home ? View.GONE : View.VISIBLE);
            setTitle(home ? R.string.app_name : R.string.settings);
            return true;
        });
    }

    private void setupSettingsViews() {
        findViewById(R.id.row_mode).setOnClickListener(v -> showModeDialog());
        findViewById(R.id.row_buffers).setOnClickListener(v -> showBufferSettingsDialog());
        findViewById(R.id.row_client).setOnClickListener(v -> startClient());
        findViewById(R.id.row_github).setOnClickListener(v ->
                openUrl("https://github.com/weixiansen574/HybridFileXfer"));
        findViewById(R.id.row_update).setOnClickListener(v ->
                openUrl("https://github.com/weixiansen574/HybridFileXfer/releases"));
        MiuixSwitch autoConnect = findViewById(R.id.switch_auto_connect);
        autoConnect.setChecked(config.isAutoConnectEnabled());
        autoConnect.setOnCheckedChangeListener((buttonView, isChecked) -> applyAutoConnect(isChecked));
        try {
            TextView version = findViewById(R.id.txv_version);
            String versionName = getPackageManager().getPackageInfo(getPackageName(), 0).versionName;
            version.setText(getString(R.string.settings_version, versionName));
        } catch (Exception ignored) {
        }
        updateSettingsUi();
    }

    /**
     * 连接模式选择（普通 / ROOT / ADB）。
     */
    private void showModeDialog() {
        String[] modes = getResources().getStringArray(R.array.select_mode);
        new AlertDialog.Builder(context)
                .setTitle(R.string.settings_mode_title)
                .setSingleChoiceItems(modes, config.getMode(), (dialog, which) -> {
                    config.setMode(which);
                    updateSettingsUi();
                    dialog.dismiss();
                })
                .setNegativeButton(R.string.cancel, null)
                .show();
    }

    private void updateSettingsUi() {
        TextView mode = findViewById(R.id.txv_mode_setting);
        if (mode != null) {
            String[] modes = getResources().getStringArray(R.array.select_mode);
            int index = Math.min(Math.max(config.getMode(), 0), modes.length - 1);
            mode.setText(modes[index]);
        }
        TextView buffers = findViewById(R.id.txv_buffers_value);
        if (buffers != null) {
            buffers.setText(getString(R.string.settings_buffers_value,
                    config.getLocalBufferCount(), config.getRemoteBufferCount()));
        }
    }

    private void syncAutoConnectSwitch() {
        MiuixSwitch autoConnect = findViewById(R.id.switch_auto_connect);
        if (autoConnect == null) {
            return;
        }
        autoConnect.setOnCheckedChangeListener(null);
        autoConnect.setChecked(config.isAutoConnectEnabled());
        autoConnect.setOnCheckedChangeListener((buttonView, isChecked) -> applyAutoConnect(isChecked));
    }

    private void applyAutoConnect(boolean enabled) {
        config.setAutoConnectEnabled(enabled);
        if (enabled) {
            Toast.makeText(context, R.string.auto_connect_enabled, Toast.LENGTH_SHORT).show();
            autoStartServerIfNeeded();
        } else {
            ServerService.stopServer(context);
            Toast.makeText(context, R.string.auto_connect_disabled, Toast.LENGTH_SHORT).show();
        }
    }

    private void openUrl(String url) {
        try {
            startActivity(new Intent(Intent.ACTION_VIEW, Uri.parse(url)));
        } catch (Exception e) {
            Toast.makeText(context, e.getMessage(), Toast.LENGTH_SHORT).show();
        }
    }

    private boolean isNightMode() {
        int mode = getResources().getConfiguration().uiMode
                & android.content.res.Configuration.UI_MODE_NIGHT_MASK;
        return mode == android.content.res.Configuration.UI_MODE_NIGHT_YES;
    }

    private void requestNotificationPermission() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU
                && ContextCompat.checkSelfPermission(this, Manifest.permission.POST_NOTIFICATIONS)
                != PackageManager.PERMISSION_GRANTED) {
            ActivityCompat.requestPermissions(this,
                    new String[]{Manifest.permission.POST_NOTIFICATIONS},
                    REQUEST_CODE_NOTIFICATION_PERMISSION);
        }
    }

    private boolean hasStoragePermission() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            return Environment.isExternalStorageManager();
        }
        return ContextCompat.checkSelfPermission(this, Manifest.permission.WRITE_EXTERNAL_STORAGE)
                == PackageManager.PERMISSION_GRANTED;
    }

    /**
     * 允许自动连接时，打开应用就让后台服务开始监听，电脑一连上就能传文件。
     */
    private void autoStartServerIfNeeded() {
        if (!config.isAutoConnectEnabled() || ServerService.isRunning()) {
            return;
        }
        if (!hasStoragePermission()) {
            return;
        }
        try {
            List<ServerNetInterface> interfaces = NetInterfaces.forAutoConnect(context);
            if (interfaces.isEmpty()) {
                return;
            }
            ServerService.startServer(context, config.getMode(), interfaces,
                    config.getLocalBufferCount(), config.getRemoteBufferCount());
        } catch (Exception e) {
            Log.w(TAG, "auto start failed", e);
        }
    }

    @Override
    protected void onResume() {
        super.onResume();
        ServerService.addListener(this);
        syncUiWithServer();
        syncAutoConnectSwitch();
        updateSettingsUi();
    }

    @Override
    protected void onPause() {
        ServerService.removeListener(this);
        super.onPause();
    }

    /**
     * 让界面与后台服务的状态保持一致。
     */
    private void syncUiWithServer() {
        if (netCardsAdapter == null) {
            return;
        }
        int state = ServerService.getState();
        boolean running = state != ServerService.STATE_STOPPED;
        boolean connected = ServerService.isConnected();
        netCardsAdapter.setEnableModify(!running);
        startServerBtn.setEnabled(true);
        startServerBtn.setText(running ? R.string.ting_zhi_fu_wu
                : R.string.qi_dong_fu_wu_qi_bing_deng_dai_lian_jie);
        toTransfer.setEnabled(connected);
        updateStatusCard(state, connected);
        if (running) {
            netCardsAdapter.setSelectedNames(ServerService.getInterfaceNames());
            List<String> accepted = ServerService.getConnectedInterfaceNames();
            for (String name : ServerService.getInterfaceNames()) {
                netCardsAdapter.changeItemState(name,
                        accepted.contains(name) ? getString(R.string.yi_lian_jie)
                                : getString(R.string.deng_dai_lian_jie));
            }
        }
    }

    /**
     * 顶部状态卡片：绿色=已连接，灰色=等待/未启动。
     */
    private void updateStatusCard(int state, boolean connected) {
        MaterialCardView card = findViewById(R.id.card_status);
        ImageView icon = findViewById(R.id.img_status);
        TextView title = findViewById(R.id.txv_status_title);
        TextView subtitle = findViewById(R.id.txv_status_subtitle);
        TextView channels = findViewById(R.id.txv_channels_value);
        TextView mode = findViewById(R.id.txv_mode_value);
        if (card == null || title == null) {
            return;
        }
        boolean running = state != ServerService.STATE_STOPPED;
        int channelCount = connected ? Math.max(ServerService.getConnectedInterfaceNames().size(), 1)
                : (running ? ServerService.getInterfaceNames().size() : 0);
        channels.setText(String.valueOf(channelCount));
        int modeIndex = config.getMode();
        mode.setText(modeIndex == 1 ? "ROOT" : modeIndex == 2 ? "ADB" : getString(R.string.normal));

        if (connected) {
            card.setCardBackgroundColor(ContextCompat.getColor(context, R.color.green_soft));
            icon.setImageResource(R.drawable.baseline_check_circle_24);
            icon.setColorFilter(ContextCompat.getColor(context, R.color.green));
            title.setText(R.string.status_connected);
            subtitle.setText(getString(R.string.status_connected_hint,
                    android.text.TextUtils.join(", ", ServerService.getConnectedInterfaceNames())));
        } else if (state == ServerService.STATE_STARTING) {
            card.setCardBackgroundColor(ContextCompat.getColor(context, R.color.gray_soft));
            icon.setImageResource(R.drawable.baseline_phonelink_ring_24);
            icon.setColorFilter(ContextCompat.getColor(context, R.color.text_secondary));
            title.setText(R.string.status_starting);
            subtitle.setText(R.string.status_starting_hint);
        } else if (state == ServerService.STATE_WAITING) {
            card.setCardBackgroundColor(ContextCompat.getColor(context, R.color.gray_soft));
            icon.setImageResource(R.drawable.baseline_phonelink_ring_24);
            icon.setColorFilter(ContextCompat.getColor(context, R.color.text_secondary));
            title.setText(R.string.status_waiting);
            subtitle.setText(R.string.status_waiting_hint);
        } else {
            card.setCardBackgroundColor(ContextCompat.getColor(context, R.color.gray_soft));
            icon.setImageResource(R.drawable.baseline_phonelink_ring_24);
            icon.setColorFilter(ContextCompat.getColor(context, R.color.text_secondary));
            title.setText(R.string.status_stopped);
            subtitle.setText(R.string.status_stopped_hint);
        }
    }

    @Override
    public void onServerStateChanged(int state) {
        syncUiWithServer();
    }

    @Override
    public void onInterfaceAccepted(String name) {
        netCardsAdapter.changeItemState(name, getString(R.string.yi_lian_jie));
    }

    @Override
    public void onInterfaceAcceptFailed(String name) {
        netCardsAdapter.changeItemState(name, getString(R.string.lian_jie_shi_bai));
    }

    @Override
    public void onBindFailed(int port) {
        Toast.makeText(context, getString(R.string.service_start_failed, port), Toast.LENGTH_SHORT).show();
    }

    @Override
    public void onOutOfMemory(int created, int needed) {
        Toast.makeText(context, getString(R.string.buffer_block_creation_failed_toast, created, needed),
                Toast.LENGTH_LONG).show();
    }

    @Override
    public void onRemoteOutOfMemory() {
        Toast.makeText(context, R.string.dui_fang_nei_cun_bu_zu, Toast.LENGTH_LONG).show();
    }

    @Override
    public void onServerError(String message) {
        Toast.makeText(context, message, Toast.LENGTH_SHORT).show();
    }

    @Override
    public void onClick(View v) {
        int id = v.getId();
        if (id == R.id.start_server) {
            if (ServerService.isRunning()) {
                disconnect(true);
            } else {
                startServer();
            }
        } else if (id == R.id.to_transfer) {
            startActivityForResult(new Intent(context, TransferActivity.class), REQUEST_CODE_TRANSFER);
        } else if (id == R.id.refresh) {
            if (!ServerService.isRunning()) {
                try {
                    netCardsAdapter.reload();
                } catch (SocketException | UnknownHostException e) {
                    Toast.makeText(context, e.getMessage(), Toast.LENGTH_SHORT).show();
                }
            } else {
                Toast.makeText(context, R.string.qing_xian_ting_zhi_fu_wu_duan, Toast.LENGTH_SHORT).show();
            }
        }
    }

    public void startServer() {
        if (checkPermissionOrRequest()) {
            Toast.makeText(context, R.string.xu_yao_wen_jian_du_xie_quan_xian, Toast.LENGTH_LONG).show();
            return;
        }
        List<ServerNetInterface> selectedInterfaces = netCardsAdapter.getSelectedInterfaces();
        if (selectedInterfaces == null) {
            return;
        }
        if (selectedInterfaces.isEmpty()) {
            Toast.makeText(context, R.string.mei_you_wang_ka_xuan_ze, Toast.LENGTH_SHORT).show();
            return;
        }
        long availableMemoryMB = getAvailableMemoryMB();
        if (config.getLocalBufferCount() > availableMemoryMB) {
            Toast.makeText(context, R.string.shou_ji_ke_yong_nei_cun_bu_zu, Toast.LENGTH_SHORT).show();
            return;
        }
        int mode = config.getMode();
        if (mode != 0 && checkShizukuOrReq(mode)) {
            return;
        }
        config.setMode(mode);
        // 记住这次的选择，后台自动连接服务会沿用
        NetInterfaces.save(context, netCardsAdapter.getInterfaceList());
        ServerService.startServer(context, mode, selectedInterfaces,
                config.getLocalBufferCount(), config.getRemoteBufferCount());
        netCardsAdapter.setEnableModify(false);
        startServerBtn.setText(R.string.ting_zhi_fu_wu);
        Toast.makeText(context, R.string.fu_wu_yi_qi_dong, Toast.LENGTH_SHORT).show();
    }

    private boolean checkPermissionOrRequest() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            // 检查是否有所有文件访问权限
            if (Environment.isExternalStorageManager()) {
                // 已经获得文件访问权限
                return false;
            } else {
                // 请求文件访问权限
                Intent intent = new Intent(Settings.ACTION_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION);
                //添加包名，不然要在设置中翻列表
                intent.setData(Uri.parse("package:" + context.getPackageName()));
                startActivityForResult(intent, REQUEST_CODE_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION);
                return true;
            }
        } else {
            // 检查是否有存储权限
            if (ContextCompat.checkSelfPermission(this, android.Manifest.permission.WRITE_EXTERNAL_STORAGE) == PackageManager.PERMISSION_GRANTED) {
                // 已经获得存储权限
                return false;
            } else {
                // 请求存储权限
                ActivityCompat.requestPermissions(this, new String[]{
                        android.Manifest.permission.WRITE_EXTERNAL_STORAGE,
                        Manifest.permission.READ_EXTERNAL_STORAGE}, 0);
                return true;
            }
        }
    }

    @Override
    public void onBackPressed() {
        BottomNavigationView bottomNav = findViewById(R.id.bottom_nav);
        if (bottomNav != null && bottomNav.getSelectedItemId() == R.id.tab_settings) {
            bottomNav.setSelectedItemId(R.id.tab_home);
            return;
        }
        super.onBackPressed();
    }

    @Override
    protected void onActivityResult(int requestCode, int resultCode, @Nullable Intent data) {
        super.onActivityResult(requestCode, resultCode, data);
        if (requestCode == REQUEST_CODE_TRANSFER) {
            if (resultCode == RESULT_CODE_SERVER_DISCONNECT) {
                // 传输时通道断了，重新开始监听，等电脑重连
                ServerService.restartSession(context);
            }
        } else if (requestCode == REQUEST_CODE_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION) {
            // 授权回来后就开始等待电脑连接
            autoStartServerIfNeeded();
            syncUiWithServer();
        }
    }


    /**
     * 缓冲区设置对话框（设置页和菜单共用）。
     */
    private void showBufferSettingsDialog() {
    
                View view = View.inflate(context, R.layout.dialog_buffer_settings, null);
                EditText editLocalCount = view.findViewById(R.id.edit_local_buffer_count);
                EditText editRemoteCount = view.findViewById(R.id.edit_remote_buffer_count);
                editLocalCount.setText(String.valueOf(config.getLocalBufferCount()));
                editRemoteCount.setText(String.valueOf(config.getRemoteBufferCount()));
                editLocalCount.setHint(getString(R.string.dang_qian_zui_gao_ke_she_zhi,getAvailableMemoryMB()));
    
                AlertDialog dialog = new AlertDialog.Builder(context)
                        .setTitle(R.string.she_zhi_huan_chong_qu_kuai_shu)
                        .setView(view)
                        .setNegativeButton(R.string.cancel, null)
                        .setPositiveButton(R.string.ok, null)
                        .create();
    
                dialog.setOnShowListener(dialogInterface -> {
                    Button positiveButton = dialog.getButton(AlertDialog.BUTTON_POSITIVE);
                    positiveButton.setOnClickListener(v -> {
                        String localCountStr = editLocalCount.getText().toString();
                        String remoteCountStr = editRemoteCount.getText().toString();
    
                        // 清除之前的错误提示
                        editLocalCount.setError(null);
                        editRemoteCount.setError(null);
    
                        // 校验是否为空
                        if (localCountStr.isEmpty()) {
                            editLocalCount.setError(getString(R.string.huan_chong_qu_kuai_shu_bu_neng_wei_kong));
                            return;
                        }
                        if (remoteCountStr.isEmpty()) {
                            editRemoteCount.setError(getString(R.string.huan_chong_qu_kuai_shu_bu_neng_wei_kong));
                            return;
                        }
    
                        int localCount = Integer.parseInt(localCountStr);
                        int remoteCount = Integer.parseInt(remoteCountStr);
    
                        // 校验是否大于 16
                        if (localCount <= 16) {
                            editLocalCount.setError(getString(R.string.zui_xiao_ke_she_zhi_16));
                            return;
                        }
                        if (remoteCount <= 16) {
                            editRemoteCount.setError(getString(R.string.zui_xiao_ke_she_zhi_16));
                            return;
                        }
    
                        config.setLocalBufferCount(localCount);
                        config.setRemoteBufferCount(remoteCount);
                        dialog.dismiss();
                    });
                });
    
                dialog.show();
    }

    private void startClient() {
        //客户端一样要申请存储权限
        if (checkPermissionOrRequest()) {
            return;
        }
        View view = View.inflate(context, R.layout.dialog_connect_server, null);
        EditText editIp = view.findViewById(R.id.edit_server_controller_ip);
        editIp.setText(config.getConnectServerControllerIp());
        EditText editMainDir = view.findViewById(R.id.edit_home_dir);
        editMainDir.setText(Environment.getExternalStorageDirectory().getAbsolutePath());
        Spinner spinner = view.findViewById(R.id.spinner_mode);
        spinner.setSelection(config.getClientIOMode());
        AlertDialog dialog = new AlertDialog.Builder(context)
                .setView(view)
                .setTitle(R.string.connect_to_server)
                .setNegativeButton(R.string.cancel, null)
                .setPositiveButton(R.string.ok, null)
                .show();

        dialog.getButton(DialogInterface.BUTTON_POSITIVE).setOnClickListener(v -> {
            String ip = editIp.getText().toString();
            if (TextUtils.isEmpty(ip)) {
                editIp.setError(getString(R.string.please_enter_the_server_ip));
                return;
            }
            if (TextUtils.isEmpty(editMainDir.getText())) {
                editMainDir.setError(getString(R.string.please_enter_the_home_dir));
                return;
            }
            int mode = spinner.getSelectedItemPosition();
            if (mode != 0) {
                if (checkShizukuOrReq(mode)) {
                    return;
                }
            }
            config.setConnectServerControllerIp(ip);
            config.setClientIOMode(mode);
            Intent intent = new Intent(context, ClientActivity.class);
            intent.putExtra("io_mode", spinner.getSelectedItemPosition());
            intent.putExtra("controller_ip", ip);
            intent.putExtra("home_dir", editMainDir.getText().toString());
            startActivity(intent);
            dialog.dismiss();
        });
    }

    public boolean checkShizukuOrReq(int mode) {
        if (!Shizuku.pingBinder()) {
            showInstallShizukuDialog(context);
            return true;
        } else {
            if (Shizuku.checkSelfPermission() != 0) {
                Toast.makeText(context, R.string.wei_shou_quan_sui_ti_shi, Toast.LENGTH_SHORT).show();
                Shizuku.requestPermission(1);
                return true;
            }
        }
        System.out.println("Shizuku UID:" + Shizuku.getUid());
        if (mode == 1) {
            if (Shizuku.getUid() != 0) {
                new AlertDialog.Builder(context)
                        .setTitle(R.string.shizuku_startup_method_are_inconsistent_title)
                        .setMessage(R.string.shizuku_startup_method_are_inconsistent_message_root)
                        .setPositiveButton(R.string.ok, null)
                        .show();
                return true;
            }
        } else if (mode == 2) {
            if (Shizuku.getUid() != 2000) {
                new AlertDialog.Builder(context)
                        .setTitle(R.string.shizuku_startup_method_are_inconsistent_title)
                        .setMessage(R.string.shizuku_startup_method_are_inconsistent_message_adb)
                        .setPositiveButton(R.string.ok, null)
                        .show();
                return true;
            }
        }
        return false;
    }

    public void showInstallShizukuDialog(Context context) {
        // 创建对话框
        new AlertDialog.Builder(context)
                .setTitle(R.string.shizuku_not_running)
                .setMessage(R.string.shizuku_not_running_message)
                .setPositiveButton(R.string.open_shizuku, (dialog, which) -> {
                    try {
                        ComponentName componentName = new ComponentName("moe.shizuku.privileged.api", "moe.shizuku.starter.MainActivity");
                        Intent intent = new Intent();
                        intent.setComponent(componentName);
                        context.startActivity(intent);
                    } catch (Exception e) {
                        Toast.makeText(context, R.string.please_install_shizuku, Toast.LENGTH_SHORT).show();
                        Intent intent = new Intent(Intent.ACTION_VIEW, Uri.parse("https://shizuku.rikka.app/"));
                        context.startActivity(intent);
                    }
                })
                .setNeutralButton(R.string.install_sui, (dialog, which) -> {
                    // 打开指定链接
                    Intent intent = new Intent(Intent.ACTION_VIEW, Uri.parse("https://github.com/RikkaApps/Sui/releases"));
                    context.startActivity(intent);
                })
                .setNegativeButton(R.string.cancel, null)
                .show();
    }

    private void changeToStartState() {
        netCardsAdapter.setEnableModify(true);
        startServerBtn.setEnabled(true);
        startServerBtn.setText(R.string.qi_dong_fu_wu_qi_bing_deng_dai_lian_jie);
    }

    private void disconnect(boolean toast) {
        ServerService.stopServer(context);
        netCardsAdapter.setEnableModify(true);
        toTransfer.setEnabled(false);
        startServerBtn.setText(R.string.qi_dong_fu_wu_qi_bing_deng_dai_lian_jie);
        startServerBtn.setEnabled(true);
        if (toast) {
            Toast.makeText(context, R.string.fu_wu_yi_guan_bi, Toast.LENGTH_SHORT).show();
        }
    }

    private long getAvailableMemoryMB() {
        ActivityManager activityManager = context.getSystemService(ActivityManager.class);
        ActivityManager.MemoryInfo memoryInfo = new ActivityManager.MemoryInfo();
        activityManager.getMemoryInfo(memoryInfo);

        long totalMemory = memoryInfo.totalMem;

        long availableMemory = memoryInfo.availMem;

        long totalMemoryMB = totalMemory / (1024 * 1024);
        long availableMemoryMB = availableMemory / (1024 * 1024);

        return (long) (availableMemoryMB - (totalMemoryMB * 0.05));
    }

    // 注意：应用退出后后台服务会继续监听，这样插上电脑就能直接发送文件。
}