// 构建脚本：将 assets/app.ico 嵌入 Windows 可执行文件资源，
// 使打包后的 exe 在资源管理器/任务栏/快捷方式中显示程序图标。
fn main() {
    // 图标缺失时给出提示而不中断构建
    let icon = std::path::Path::new("assets/app.ico");
    if icon.exists() {
        println!("cargo:rerun-if-changed=assets/app.ico");
        if let Err(e) = winresource::WindowsResource::new().set_icon("assets/app.ico").compile() {
            println!("cargo:warning=嵌入图标失败: {e}");
        }
    }
}
