package top.weixiansen574.hybridfilexfer.core.callback;

import java.util.List;

import top.weixiansen574.hybridfilexfer.core.bean.TrafficInfo;

public interface TransferFileCallback {
    int ERROR_TYPE_EXCEPTION = 1;
    int ERROR_TYPE_INTERRUPT = 2;
    int ERROR_TYPE_READ_ERROR = 3;
    int ERROR_TYPE_WRITE_ERROR = 4;

    void onFileUploading(String iName, String path, long targetSize, long totalSize);

    void onFileDownloading(String iName, String path, long targetSize, long totalSize);

    void onSpeedInfo(List<TrafficInfo> trafficInfoList);

    void onChannelComplete(String iName, long traffic, long time);

    void onChannelError(String iName, int errorType, String message);//异常信息 or 断开

    void onReadFileError(String message);

    void onWriteFileError(String message);

    void onComplete(boolean isUpload,long traffic, long time);

    /**
     * 303：收尾整文件校验的结果（默认空实现，界面想显示"校验通过"再覆写）。
     *
     * @param ok        true = 两端文件 SHA-256 全部一致
     * @param fileCount 参与校验的文件数
     */
    default void onVerifyChecked(boolean ok, int fileCount) {
    }

    //传输通道有其中一个断开时
    void onIncomplete();

}
