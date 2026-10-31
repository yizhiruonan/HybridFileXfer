package top.weixiansen574.hybridfilexfer.listadapter;

import android.annotation.SuppressLint;
import android.content.Context;
import android.text.Editable;
import android.text.TextWatcher;
import android.view.LayoutInflater;
import android.view.View;
import android.view.ViewGroup;
import android.widget.EditText;
import android.widget.ImageView;
import android.widget.TextView;
import android.widget.Toast;

import androidx.annotation.NonNull;
import androidx.recyclerview.widget.RecyclerView;

import java.io.IOException;
import java.net.InetAddress;
import java.net.SocketException;
import java.net.UnknownHostException;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

import top.weixiansen574.hybridfilexfer.NetCardIcon;
import top.weixiansen574.hybridfilexfer.NetInterfaceInfo;
import top.weixiansen574.hybridfilexfer.NetInterfaces;
import top.weixiansen574.hybridfilexfer.R;
import top.weixiansen574.hybridfilexfer.core.bean.ServerNetInterface;

public class NetCardsAdapter extends RecyclerView.Adapter<NetCardsAdapter.ViewHolder> {

    private final Context context;
    private List<NetInterfaceInfo> netInterfaceList;
    private boolean enableModify = true;

    public NetCardsAdapter(Context context) throws IOException {
        this.context = context;
        netInterfaceList = NetInterfaces.enumerate(context);
        for (NetInterfaceInfo item : netInterfaceList) {
            item.state = context.getString(R.string.not_run);
        }
        NetInterfaces.applySaved(context, netInterfaceList);
    }

    @NonNull
    @Override
    public ViewHolder onCreateViewHolder(@NonNull ViewGroup parent, int viewType) {
        return new ViewHolder(LayoutInflater.from(context).inflate(R.layout.item_net_card_info, parent, false));
    }

    @SuppressLint({"UseCompatLoadingForDrawables", "SetTextI18n"})
    @Override
    public void onBindViewHolder(@NonNull ViewHolder holder, int position) {
        NetInterfaceInfo item = netInterfaceList.get(position);
        holder.txvNameAndState.setText(item.name + " | " + item.state);
        holder.txvIP.setText(item.address.getHostAddress());
        boolean enableBindIpInput = !item.isUsbAdb();
        holder.imgInterfaceType.setImageDrawable(context.getDrawable(NetCardIcon.matchIdForIName(item.name)));

        holder.editClientBindIP.setEnabled(enableModify && enableBindIpInput);
        holder.cbEnable.setEnabled(enableModify);
        holder.cbEnable.setOnCheckedChangeListener(null);
        holder.cbEnable.setChecked(item.enabled);
        holder.cbEnable.setOnCheckedChangeListener((buttonView, isChecked) -> item.enabled = isChecked);

        holder.editClientBindIP.setText(item.bindAddress);
        holder.editClientBindIP.addTextChangedListener(new TextWatcher() {
            @Override
            public void beforeTextChanged(CharSequence s, int start, int count, int after) {

            }

            @Override
            public void onTextChanged(CharSequence s, int start, int before, int count) {
                item.bindAddress = s.toString();
            }

            @Override
            public void afterTextChanged(Editable s) {

            }
        });
    }

    @Override
    public int getItemCount() {
        return netInterfaceList.size();
    }

    public void changeItemState(String name, String state) {
        for (int i = 0; i < netInterfaceList.size(); i++) {
            NetInterfaceInfo item = netInterfaceList.get(i);
            if (item.name.equals(name)) {
                item.state = state;
                notifyItemChanged(i);
            }
        }
    }

    @Override
    public int getItemViewType(int position) {
        return position;
    }

    @Override
    public long getItemId(int position) {
        return position;
    }

    @SuppressLint("NotifyDataSetChanged")
    public void setEnableModify(boolean enableModify) {
        this.enableModify = enableModify;
        if (enableModify) {
            for (NetInterfaceInfo item : netInterfaceList) {
                item.state = context.getString(R.string.not_run);
            }
        }
        notifyDataSetChanged();
    }

    public List<ServerNetInterface> getSelectedInterfaces() {
        List<ServerNetInterface> serverNetInterfaceList = new ArrayList<>();
        for (NetInterfaceInfo netInterface : netInterfaceList) {
            if (netInterface.enabled) {
                try {
                    serverNetInterfaceList.add(netInterface.toServerNetInterface());
                } catch (Exception e) {
                    e.printStackTrace();
                    Toast.makeText(context, context.getString(R.string.invalid_ip) + netInterface.bindAddress, Toast.LENGTH_SHORT).show();
                    return null;
                }
            }
        }
        return serverNetInterfaceList;
    }

    public List<NetInterfaceInfo> getInterfaceList() {
        return netInterfaceList;
    }

    /**
     * 让界面上的勾选与后台服务实际使用的网卡保持一致。
     */
    @SuppressLint("NotifyDataSetChanged")
    public void setSelectedNames(List<String> names) {
        boolean changed = false;
        for (NetInterfaceInfo item : netInterfaceList) {
            boolean shouldEnable = names.contains(item.name);
            if (item.enabled != shouldEnable) {
                item.enabled = shouldEnable;
                changed = true;
            }
        }
        if (changed) {
            notifyDataSetChanged();
        }
    }

    /**
     * 重新枚举网卡，尽量保留用户此前的勾选和指定IP。
     */
    @SuppressLint("NotifyDataSetChanged")
    public void reload() throws SocketException, UnknownHostException {
        Map<String, NetInterfaceInfo> previous = new LinkedHashMap<>();
        for (NetInterfaceInfo item : netInterfaceList) {
            previous.put(item.name, item);
        }
        List<NetInterfaceInfo> refreshed = NetInterfaces.enumerate(context);
        for (NetInterfaceInfo item : refreshed) {
            NetInterfaceInfo old = previous.get(item.name);
            item.state = context.getString(R.string.not_run);
            if (old != null) {
                item.enabled = old.enabled;
                item.bindAddress = old.bindAddress;
            }
        }
        netInterfaceList = refreshed;
        notifyDataSetChanged();
    }


    public static class ViewHolder extends RecyclerView.ViewHolder {
        top.weixiansen574.hybridfilexfer.MiuixSwitch cbEnable;
        TextView txvNameAndState;
        TextView txvIP;
        EditText editClientBindIP;
        ImageView imgInterfaceType;

        public ViewHolder(@NonNull View itemView) {
            super(itemView);
            cbEnable = itemView.findViewById(R.id.cb_interface_enable);
            txvNameAndState = itemView.findViewById(R.id.txv_interface_name_and_state);
            txvIP = itemView.findViewById(R.id.txv_interface_ip);
            editClientBindIP = itemView.findViewById(R.id.edit_client_bind_ip);
            imgInterfaceType = itemView.findViewById(R.id.img_interface_type);
        }
    }
}