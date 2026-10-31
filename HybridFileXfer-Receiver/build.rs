fn main() {
    // fluent = Slint 内置的 Windows 11 (WinUI/Fluent) 控件外观
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("slint 编译失败");
}
