fn main() {
    // fluent = Slint 内置的 Windows 11 (WinUI/Fluent) 控件外观
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("slint 编译失败");

    // 把应用图标和版本信息编进 exe（资源管理器 / 任务栏显示的图标，属性里的来源）
    #[cfg(windows)]
    {
        let target = std::env::var("TARGET").unwrap_or_default();
        if target.contains("-gnu") || target.contains("-gnullvm") {
            compile_gnu_resource();
        } else {
            compile_msvc_resource();
        }
    }
}

/// MSVC 目标：交给 winres（它内部调用 rc.exe）
#[cfg(windows)]
fn compile_msvc_resource() {
    let mut res = winres::WindowsResource::new();
    res.set_icon("ui/receiver.ico");
    // 完整的版本信息：未签名程序带上这些，杀软启发式的误判率会低一些，
    // 用户在"属性 → 详细信息"里也能看到来源
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    res.set("ProductName", "HybridFileXfer 接收器");
    res.set("FileDescription", "多轨快传 · 电脑接收器");
    res.set("CompanyName", "HybridFileXfer contributors");
    res.set("LegalCopyright", "GPL-3.0");
    res.set("OriginalFilename", "HybridFileXfer-Receiver.exe");
    res.set("InternalName", "HybridFileXfer-Receiver");
    res.set("ProductVersion", &version);
    res.set("FileVersion", &version);
    if let Err(e) = res.compile() {
        // 没有 rc.exe 时不阻断构建，只是 exe 没有自定义图标
        println!("cargo:warning=编译 Windows 资源失败（exe 将没有自定义图标）：{e}");
    }
}

/// GNU / gnullvm 目标（例如 ARM64）：自己写 .rc 再用 windres 编。
///
/// 不走 winres 是因为它判定「是不是 GNU 目标」用的是 `contains("windows-gnu")`，
/// 而 `aarch64-pc-windows-gnullvm` 里是 `windows-gnullvm` —— 匹配不上，它就按
/// MSVC 那条路去找 rc.exe，找不到只打一句 warning 然后不管，于是 ARM64 版
/// exe 出来没有图标也没有版本信息（属性里一片空白，杀软启发式也更容易误判）。
#[cfg(windows)]
fn compile_gnu_resource() {
    use std::path::PathBuf;
    use std::process::Command;

    let target = std::env::var("TARGET").unwrap_or_default();
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let mut it = version.split('.').map(|s| s.parse::<u32>().unwrap_or(0));
    let (a, b, c) = (it.next().unwrap_or(0), it.next().unwrap_or(0), it.next().unwrap_or(0));

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let rc_path = out_dir.join("receiver_generated.rc");
    let icon = std::fs::canonicalize("ui/receiver.ico")
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "ui/receiver.ico".into());
    // 用 r#"..."# 原样写 .rc（资源脚本不认 '\n' 转义，得是真换行）。
    //
    // 编码：windres 默认按本地代码页（简中机器是 936/GBK）解释 .rc，而这里写出去
    // 的是 UTF-8，中文就会变成一串乱码（å¤šè½¨...）。实测 llvm-windres **不认**
    // 脚本里的 #pragma code_page，只认命令行 --codepage=65001（见下面调用）；
    // GNU windres 则相反，认 pragma。两处都写上。
    let rc_text = format!(
        r#"#pragma code_page(65001)
1 ICON "{icon}"
1 VERSIONINFO
FILEVERSION {a},{b},{c},0
PRODUCTVERSION {a},{b},{c},0
FILEOS 0x4
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "080404b0"
    BEGIN
      VALUE "ProductName", "HybridFileXfer 接收器"
      VALUE "FileDescription", "多轨快传 · 电脑接收器"
      VALUE "CompanyName", "HybridFileXfer contributors"
      VALUE "LegalCopyright", "GPL-3.0"
      VALUE "OriginalFilename", "HybridFileXfer-Receiver.exe"
      VALUE "InternalName", "HybridFileXfer-Receiver"
      VALUE "ProductVersion", "{version}"
      VALUE "FileVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x804, 1200
  END
END
"#
    );
    std::fs::write(&rc_path, rc_text).expect("写资源脚本失败");

    // windres 由构建脚本通过 HFX_WINDRES 传进来（见 _hfx-verify/buildrecv-arm64.ps1）；
    // 没传就按 target 名去 PATH 里猜一个。
    let windres = std::env::var("HFX_WINDRES").unwrap_or_else(|_| {
        let prefix = target.replace("pc-windows-gnullvm", "w64-mingw32");
        format!("{prefix}-windres")
    });
    println!("cargo:rerun-if-env-changed=HFX_WINDRES");

    let obj = out_dir.join("receiver_res.o");
    match Command::new(&windres)
        .arg("--codepage=65001")
        .arg("-I")
        .arg("ui")
        .arg(&rc_path)
        .arg("-O")
        .arg("coff")
        .arg("-o")
        .arg(&obj)
        .output()
    {
        Ok(o) if o.status.success() => println!("cargo:rustc-link-arg={}", obj.display()),
        Ok(o) => println!(
            "cargo:warning=windres 编译资源失败（exe 将没有图标和版本信息）：{}",
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => println!("cargo:warning=找不到 windres（{windres}）：{e}"),
    }

    println!("cargo:rerun-if-changed=ui/receiver.ico");
    println!("cargo:rerun-if-changed=build.rs");
}
