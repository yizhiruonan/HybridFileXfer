//! Windows 右键「发送到 → 发送到手机」快捷方式。
//!
//! 快捷方式放在 %APPDATA%/Microsoft/Windows/SendTo 里，目标就是本程序；
//! Windows 会把选中的文件路径当参数传进来，接收器识别到已存在的文件就当"发送到手机"。

use std::path::PathBuf;

const SHORTCUT_NAME: &str = "发送到手机.lnk";

pub fn shortcut_path() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(
        PathBuf::from(appdata)
            .join("Microsoft")
            .join("Windows")
            .join("SendTo")
            .join(SHORTCUT_NAME),
    )
}

pub fn exists() -> bool {
    shortcut_path().map(|p| p.exists()).unwrap_or(false)
}

/// 创建（已存在则跳过）
pub fn ensure() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = exe.parent().ok_or_else(|| "找不到程序目录".to_string())?;
    let lnk = shortcut_path().ok_or_else(|| "找不到 %APPDATA%".to_string())?;
    if lnk.exists() {
        return Ok(());
    }
    if let Some(parent) = lnk.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let ps = format!(
        "$s=(New-Object -ComObject WScript.Shell).CreateShortcut('{}'); $s.TargetPath='{}'; $s.WorkingDirectory='{}'; $s.IconLocation='{},0'; $s.Description='发送到手机'; $s.Save()",
        lnk.display(),
        exe.display(),
        dir.display(),
        exe.display()
    );
    // 用 -EncodedCommand（Base64 的 UTF-16LE）：避免中文路径/快捷方式名
    // 在 powershell.exe 的命令行里被按 ANSI 解析成乱码
    let encoded = encode_utf16le_base64(&ps);
    let mut command = std::process::Command::new("powershell");
    command.args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-EncodedCommand", &encoded]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let status = command.status().map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("PowerShell 返回 {status}"))
    }
}

/// PowerShell 的 -EncodedCommand 参数：UTF-16LE + Base64
fn encode_utf16le_base64(text: &str) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes: Vec<u8> = text.encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

/// 删除（找不到就算了）
pub fn remove() {
    if let Some(lnk) = shortcut_path() {
        let _ = std::fs::remove_file(lnk);
    }
}
