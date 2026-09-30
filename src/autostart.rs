//! # 开机自启动（任务计划程序）
//!
//! 通过 Windows 任务计划程序（Task Scheduler 2.0 COM 接口）注册一个
//! “当前用户登录时启动本程序”的计划任务：
//!
//! - **不需要管理员权限**：任务以当前用户身份、交互令牌运行
//!   （`LogonType = InteractiveToken`、`RunLevel = LUA`），普通用户即可创建；
//! - 相比注册表 `Run` 项，登录触发不依赖自启动目录，也不需要程序以
//!   管理员身份安装，且可在“任务计划程序”面板中查看/删除；
//! - **全局唯一任务**：debug / release / 被移动到任意位置的副本，操作的
//!   都是同一条 [`TASK_NAME`] 任务——不会因多份程序产生多条任务或脏数据；
//! - **静默启动**：任务的启动动作带上 [`AUTOSTART_ARG`] 参数，本次启动据此
//!   判断"是开机自启动拉起来的"，把主窗口直接隐藏到托盘，不打扰用户
//!   （手动双击启动仍然正常显示界面，托盘菜单可随时唤出）；
//! - **路径跟随最后一次启动**：每次程序启动都调用 [`sync_registration`]，
//!   把任务的执行路径替换为当前正在运行的 exe。因此无论从哪个副本启动，
//!   下次开机登录时启动的都是最后一次运行的那一份；
//! - 任务存在即视为开启，删除即关闭——任务本身就是开关状态的唯一
//!   事实来源（与路径无关，路径由启动同步负责），无需另外持久化。
//!
//! 所有 COM 调用前按需初始化 COM（gpui 主线程可能已初始化过，
//! 此时直接复用即可），用完后配额释放。

use std::path::{Path, PathBuf};

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

/// 计划任务启动动作附加的参数。开机自启动拉起的进程据此知道"我不是用户
/// 手动点的"，应把主窗口静默隐藏到托盘，而不是弹到用户面前。
pub const AUTOSTART_ARG: &str = "--from-autostart";

/// 本次进程是否由开机自启动的计划任务拉起。
///
/// 只看第一个非可执行文件名的参数：手动启动不带该参数 → 正常显示界面；
/// 开机自启动带上 → 静默进托盘。
pub fn launched_by_task() -> bool {
    std::env::args_os().skip(1).any(|a| a == AUTOSTART_ARG)
}

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

/// 当前是否已开启开机启动（计划任务是否存在，与任务指向的路径无关）
pub fn is_enabled() -> bool {
    let owns = init_com();
    let result: Result<bool, String> = (|| {
        let service = connect_task_service()?;
        let folder = root_folder(&service)?;
        task_exists(&folder, TASK_NAME)
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
    if enabled {
        let exe = std::env::current_exe().map_err(|e| format!("获取程序路径失败: {e}"))?;
        register_task(&service, &folder, TASK_NAME, &exe)
    } else {
        delete_task(&folder, TASK_NAME)
    }
}

/// 启动同步：任务的执行路径或启动参数不是当前期望值时，重新注册。
/// 返回是否执行了重新注册。
///
/// 每次程序启动都应调用（且在单实例检查之前）：debug / release / 被
/// 移动到任意位置的副本，哪一份最后启动，下次开机登录就启动哪一份。
/// 任务不存在（未开启自启动）时不动。
pub fn sync_registration() -> Result<bool, String> {
    let current = std::env::current_exe().map_err(|e| format!("获取程序路径失败: {e}"))?;
    let owns = init_com();
    let result: Result<bool, String> = (|| {
        let service = connect_task_service()?;
        let folder = root_folder(&service)?;
        if !task_exists(&folder, TASK_NAME)? {
            return Ok(false); // 未开启自启动，无事可做
        }
        match registered_action(&folder, TASK_NAME)? {
            // 路径一致且已带静默启动参数：无需重写
            Some((p, args)) if p == current && args.as_deref() == Some(AUTOSTART_ARG) => Ok(false),
            _ => {
                register_task(&service, &folder, TASK_NAME, &current)?;
                Ok(true)
            }
        }
    })();
    if owns {
        unsafe { CoUninitialize() };
    }
    result
}

/// 任务是否存在
fn task_exists(folder: &ITaskFolder, name: &str) -> Result<bool, String> {
    Ok(unsafe { folder.GetTask(&BSTR::from(name)) }.is_ok())
}

/// 任务注册的执行动作：(exe 路径, 启动参数)。无可用 Exec 动作时返回 None
fn registered_action(
    folder: &ITaskFolder,
    name: &str,
) -> Result<Option<(PathBuf, Option<String>)>, String> {
    let task = unsafe { folder.GetTask(&BSTR::from(name)) }
        .map_err(|e| format!("读取计划任务失败: {e}"))?;
    let definition = unsafe { task.Definition() }.map_err(|e| format!("读取任务定义失败: {e}"))?;
    let actions = unsafe { definition.Actions() }.map_err(|e| format!("读取任务动作失败: {e}"))?;
    let mut count = 0i32;
    unsafe { actions.Count(&mut count) }.map_err(|e| format!("读取动作数量失败: {e}"))?;
    for i in 1..=count {
        let Ok(action) = (unsafe { actions.get_Item(i) }) else { continue };
        let Ok(exec) = action.cast::<IExecAction>() else { continue };
        let mut path = BSTR::default();
        if unsafe { exec.Path(&mut path) }.is_err() {
            continue;
        }
        let mut arguments = BSTR::default();
        let args = if unsafe { exec.Arguments(&mut arguments) }.is_ok() {
            Some(arguments.to_string()).filter(|s| !s.is_empty())
        } else {
            None
        };
        return Ok(Some((PathBuf::from(path.to_string()), args)));
    }
    Ok(None)
}

/// 注册（同名覆盖更新）一个"用户登录时启动 exe"的任务
fn register_task(
    service: &ITaskService,
    folder: &ITaskFolder,
    name: &str,
    exe: &Path,
) -> Result<(), String> {
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

    // ---- 动作：启动 exe ----
    let actions: IActionCollection = unsafe { definition.Actions() }
        .map_err(|e| format!("读取任务动作失败: {e}"))?;
    let action: IAction = unsafe { actions.Create(TASK_ACTION_EXEC) }
        .map_err(|e| format!("创建启动动作失败: {e}"))?;
    let exec: IExecAction = action
        .cast::<IExecAction>()
        .map_err(|e| format!("获取启动动作失败: {e}"))?;
    unsafe { exec.SetPath(&BSTR::from(exe.display().to_string())) }
        .map_err(|e| format!("设置启动路径失败: {e}"))?;
    // 带上静默启动标记：开机登录时窗口直接进托盘，不弹到用户面前
    unsafe { exec.SetArguments(&BSTR::from(AUTOSTART_ARG)) }
        .map_err(|e| format!("设置启动参数失败: {e}"))?;

    // ---- 注册（同名任务覆盖更新）----
    let empty = VARIANT::default();
    unsafe {
        folder.RegisterTaskDefinition(
            &BSTR::from(name),
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

/// 删除任务。不存在（文件未找到 0x80070002）也视为成功，保证幂等
fn delete_task(folder: &ITaskFolder, name: &str) -> Result<(), String> {
    if let Err(e) = unsafe { folder.DeleteTask(&BSTR::from(name), 0) } {
        if (e.code().0 as u32) != 0x8007_0002 {
            return Err(format!("删除计划任务失败: {e}"));
        }
    }
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

    /// 测试专用任务名。绝不操作真实的 [`TASK_NAME`]：测试进程的
    /// `current_exe()` 是 cargo 的测试二进制（`deps/xxx-<hash>.exe`），
    /// 如果用它注册真实任务，登录时启动的会是测试进程而非本程序，
    /// 且 cargo 清理构建产物后该路径直接失效——开机自启动随之失灵。
    const TEST_TASK_NAME: &str = "auto-shutdown-test";

    /// 注册 / 存在检查 / 删除的往返与幂等，全程只操作测试任务
    #[test]
    fn register_roundtrip() {
        let owns = init_com();
        let result: Result<(), String> = (|| {
            let service = connect_task_service()?;
            let folder = root_folder(&service)?;
            let exe = std::env::current_exe().map_err(|e| format!("获取测试程序路径失败: {e}"))?;

            // 清掉上次运行可能的残留
            let _ = delete_task(&folder, TEST_TASK_NAME);
            assert!(!task_exists(&folder, TEST_TASK_NAME)?, "残留任务未清理");

            register_task(&service, &folder, TEST_TASK_NAME, &exe)?;
            assert!(task_exists(&folder, TEST_TASK_NAME)?, "注册后任务应存在");
            // 启动动作必须带上静默启动标记，否则开机自启动会弹界面
            let (path, args) =
                registered_action(&folder, TEST_TASK_NAME)?.expect("注册后应能读到执行动作");
            assert_eq!(path, exe, "执行路径应为注册时的 exe");
            assert_eq!(
                args.as_deref(),
                Some(AUTOSTART_ARG),
                "启动参数应包含静默启动标记"
            );

            delete_task(&folder, TEST_TASK_NAME)?;
            assert!(!task_exists(&folder, TEST_TASK_NAME)?, "删除后任务应不存在");
            // 再删一次也应成功（幂等）
            delete_task(&folder, TEST_TASK_NAME)?;
            Ok(())
        })();
        if owns {
            unsafe { CoUninitialize() };
        }
        result.expect("测试任务往返失败");
    }
}
