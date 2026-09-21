//! # 开机自启动（任务计划程序）
//!
//! 通过 Windows 任务计划程序（Task Scheduler 2.0 COM 接口）注册一个
//! “当前用户登录时启动本程序”的计划任务：
//!
//! - **不需要管理员权限**：任务以当前用户身份、交互令牌运行
//!   （`LogonType = InteractiveToken`、`RunLevel = LUA`），普通用户即可创建；
//! - 相比注册表 `Run` 项，登录触发不依赖自启动目录，也不需要程序以
//!   管理员身份安装，且可在“任务计划程序”面板中查看/删除；
//! - 任务存在即视为开启，删除即关闭——任务本身就是开关状态的唯一
//!   事实来源，无需另外持久化。
//!
//! 所有 COM 调用前按需初始化 COM（gpui 主线程可能已初始化过，
//! 此时直接复用即可），用完后配额释放。

use windows::core::{BSTR, Interface as _};
use windows::Win32::Foundation::VARIANT_BOOL;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::TaskScheduler::{
    IAction, IActionCollection, IExecAction, ILogonTrigger,
    IRegistrationInfo, ITaskDefinition, ITaskFolder, ITaskService, ITrigger, ITriggerCollection,
    ITaskSettings, IPrincipal, TASK_ACTION_EXEC, TASK_CREATE_OR_UPDATE, TASK_LOGON_INTERACTIVE_TOKEN,
    TASK_RUNLEVEL_LUA, TASK_TRIGGER_LOGON,
};
use windows::Win32::System::Variant::VARIANT;

/// 任务计划程序 2.0 的 coclass（"Schedule.Service"，即 ITaskService 的实现）。
/// 注意 windows crate 导出的 `CLSID_CTaskScheduler` 是 1.0 的旧接口 coclass，
/// 对 `ITaskService` 查询会得到 E_NOINTERFACE，不能用。
const CLSID_SCHEDULE_SERVICE: windows::core::GUID =
    windows::core::GUID::from_u128(0x0f87369f_a4e5_4cfc_bd3e_73e6154572dd);

/// 计划任务名（根目录下）
const TASK_NAME: &str = "auto-shutdown";
/// 任务描述
const TASK_DESCRIPTION: &str = "自动关机守护：用户登录时自动启动";

/// VARIANT_BOOL 的 true（COM 约定为 -1）
const VB_TRUE: VARIANT_BOOL = VARIANT_BOOL(-1);

/// 按需初始化 COM，返回是否需要调用 `CoUninitialize` 配额释放。
fn init_com() -> bool {
    // RPC_E_CHANGED_MODE（本线程已以其他模式初始化）时 hr 为错误值，
    // 但 COM 本身已可用，直接复用，不调用 CoUninitialize
    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    hr.is_ok()
}

/// 任务计划服务（已连接本机）
fn connect_task_service() -> Result<ITaskService, String> {
    let service: ITaskService = unsafe {
        CoCreateInstance(&CLSID_SCHEDULE_SERVICE, None, CLSCTX_INPROC_SERVER)
    }
    .map_err(|e| format!("创建任务计划服务失败: {e}"))?;
    let empty = VARIANT::default();
    unsafe { service.Connect(&empty, &empty, &empty, &empty) }
        .map_err(|e| format!("连接任务计划服务失败: {e}"))?;
    Ok(service)
}

/// 根任务文件夹
fn root_folder(service: &ITaskService) -> Result<ITaskFolder, String> {
    unsafe { service.GetFolder(&BSTR::from("\\")) }.map_err(|e| format!("打开任务计划根目录失败: {e}"))
}

/// 当前是否已开启开机启动（计划任务是否存在）
pub fn is_enabled() -> bool {
    let owns = init_com();
    let result: Result<bool, String> = (|| {
        let service = connect_task_service()?;
        let folder = root_folder(&service)?;
        Ok(unsafe { folder.GetTask(&BSTR::from(TASK_NAME)) }.is_ok())
    })();
    if owns {
        unsafe { CoUninitialize() };
    }
    result.unwrap_or(false)
}

/// 开启 / 关闭开机启动。失败时返回错误描述（供界面与日志展示）。
pub fn set_enabled(enabled: bool) -> Result<(), String> {
    let owns = init_com();
    let result = inner_set_enabled(enabled);
    if owns {
        unsafe { CoUninitialize() };
    }
    result
}

fn inner_set_enabled(enabled: bool) -> Result<(), String> {
    let service = connect_task_service()?;
    let folder = root_folder(&service)?;

    if !enabled {
        // 任务不存在（文件未找到）也视为成功，保证开关幂等
        if let Err(e) = unsafe { folder.DeleteTask(&BSTR::from(TASK_NAME), 0) } {
            if (e.code().0 as u32) != 0x8007_0002 {
                return Err(format!("删除计划任务失败: {e}"));
            }
        }
        return Ok(());
    }

    let definition: ITaskDefinition =
        unsafe { service.NewTask(0) }.map_err(|e| format!("创建任务定义失败: {e}"))?;

    // ---- 基本信息 ----
    let info: IRegistrationInfo = unsafe { definition.RegistrationInfo() }
        .map_err(|e| format!("读取任务注册信息失败: {e}"))?;
    unsafe { info.SetDescription(&BSTR::from(TASK_DESCRIPTION)) }
        .map_err(|e| format!("设置任务描述失败: {e}"))?;
    unsafe { info.SetAuthor(&BSTR::from("auto-shutdown")) }
        .map_err(|e| format!("设置任务作者失败: {e}"))?;

    // ---- 设置：电池供电也照常启动、不限运行时长（默认 72h 会被
    //      任务计划程序杀掉进程，守护程序必须设为 PT0S 不限）----
    let settings: ITaskSettings = unsafe { definition.Settings() }
        .map_err(|e| format!("读取任务设置失败: {e}"))?;
    unsafe { settings.SetDisallowStartIfOnBatteries(VARIANT_BOOL(0)) }
        .map_err(|e| format!("设置电池选项失败: {e}"))?;
    unsafe { settings.SetStopIfGoingOnBatteries(VARIANT_BOOL(0)) }
        .map_err(|e| format!("设置电池选项失败: {e}"))?;
    unsafe { settings.SetExecutionTimeLimit(&BSTR::from("PT0S")) }
        .map_err(|e| format!("设置运行时长失败: {e}"))?;

    // ---- 主体：当前用户、交互令牌、最低权限（无需管理员） ----
    let principal: IPrincipal = unsafe { definition.Principal() }
        .map_err(|e| format!("读取任务主体失败: {e}"))?;
    unsafe { principal.SetLogonType(TASK_LOGON_INTERACTIVE_TOKEN) }
        .map_err(|e| format!("设置登录类型失败: {e}"))?;
    unsafe { principal.SetRunLevel(TASK_RUNLEVEL_LUA) }
        .map_err(|e| format!("设置运行级别失败: {e}"))?;

    // ---- 触发器：当前用户登录时 ----
    let triggers: ITriggerCollection = unsafe { definition.Triggers() }
        .map_err(|e| format!("读取任务触发器失败: {e}"))?;
    let trigger: ITrigger = unsafe { triggers.Create(TASK_TRIGGER_LOGON) }
        .map_err(|e| format!("创建登录触发器失败: {e}"))?;
    let logon: ILogonTrigger = trigger
        .cast::<ILogonTrigger>()
        .map_err(|e| format!("获取登录触发器失败: {e}"))?;
    unsafe { logon.SetUserId(&BSTR::from(current_user())) }
        .map_err(|e| format!("设置触发用户失败: {e}"))?;
    unsafe { trigger.SetEnabled(VB_TRUE) }.map_err(|e| format!("启用触发器失败: {e}"))?;

    // ---- 动作：启动本程序 ----
    let exe = std::env::current_exe().map_err(|e| format!("获取程序路径失败: {e}"))?;
    let actions: IActionCollection = unsafe { definition.Actions() }
        .map_err(|e| format!("读取任务动作失败: {e}"))?;
    let action: IAction = unsafe { actions.Create(TASK_ACTION_EXEC) }
        .map_err(|e| format!("创建启动动作失败: {e}"))?;
    let exec: IExecAction = action
        .cast::<IExecAction>()
        .map_err(|e| format!("获取启动动作失败: {e}"))?;
    unsafe { exec.SetPath(&BSTR::from(exe.display().to_string())) }
        .map_err(|e| format!("设置启动路径失败: {e}"))?;

    // ---- 注册（同名任务覆盖更新）----
    let empty = VARIANT::default();
    unsafe {
        folder.RegisterTaskDefinition(
            &BSTR::from(TASK_NAME),
            &definition,
            TASK_CREATE_OR_UPDATE.0,
            &empty,
            &empty,
            TASK_LOGON_INTERACTIVE_TOKEN,
            &empty,
        )
    }
    .map_err(|e| format!("注册计划任务失败: {e}"))?;
    Ok(())
}

/// 当前用户（"DOMAIN\user" 形式），登录触发器需要指定用户
fn current_user() -> String {
    let domain = std::env::var("USERDOMAIN").unwrap_or_default();
    let user = std::env::var("USERNAME").unwrap_or_default();
    if domain.is_empty() {
        user
    } else {
        format!(r"{domain}\{user}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 开关幂等且读写一致：记住初始状态，切换后恢复，不影响用户现有配置
    #[test]
    fn toggle_roundtrip() {
        let initial = is_enabled();

        set_enabled(true).expect("注册计划任务失败");
        assert!(is_enabled(), "注册后应读到已开启");

        set_enabled(false).expect("删除计划任务失败");
        assert!(!is_enabled(), "删除后应读到已关闭");
        // 再删一次也应成功（幂等）
        set_enabled(false).expect("重复删除应幂等");

        set_enabled(initial).expect("恢复初始状态失败");
        assert_eq!(is_enabled(), initial);
    }
}
