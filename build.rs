// 构建脚本：将 assets/app.ico 嵌入 Windows 可执行文件资源，
// 使打包后的 exe 在资源管理器/任务栏/快捷方式中显示程序图标。
//
// 注意：本程序不需要管理员权限，也不嵌入 UAC manifest——gpui 的
// 预编译静态库 gpui.lib 自带一份 manifest 资源，再嵌一份会因资源
// ID 冲突（CVT1100 duplicate resource）导致链接失败。
// 开机自启动走任务计划程序（普通用户即可创建），见 autostart.rs。
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
