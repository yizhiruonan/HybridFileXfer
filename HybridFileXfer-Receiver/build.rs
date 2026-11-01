fn main() {
    // fluent = Slint 内置的 Windows 11 (WinUI/Fluent) 控件外观
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("slint 编译失败");

    // 把应用图标编进 exe（资源管理器 / 任务栏显示的图标）
    #[cfg(windows)]
    {
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
}
