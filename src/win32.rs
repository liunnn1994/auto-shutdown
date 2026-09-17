//! # Windows 原生窗口辅助
//!
//! gpui 目前没有公开“隐藏窗口 / 拦截最小化 / 暴露 HWND”的 API，
//! 因此这里通过窗口标题找到原生 HWND，再用 Win32 API 实现托盘应用
//! 需要的几个能力：
//!
//! - [`hide_window`] / [`show_window`]：最小化到托盘 & 从托盘恢复；
//! - [`hook_minimize_to_tray`]：子类化窗口过程，把“最小化”改成“隐藏到托盘”
//!   （gpui 的原生最小化只会缩到任务栏，不会进托盘）；
//! - 关闭按钮的拦截不需要 Win32：gpui 提供 `on_window_should_close`，
//!   返回 false 即可阻止窗口销毁（见 app.rs）。

use std::sync::atomic::{AtomicIsize, Ordering};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, FindWindowW, IsIconic, IsWindowVisible, SetForegroundWindow,
    SetWindowLongPtrW, SetWindowPos, ShowWindow, GWLP_WNDPROC, HWND_TOP, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SW_HIDE, SW_RESTORE, SW_SHOW, WM_SYSCOMMAND,
};

/// 在主窗口标题栏上拦截到的最小化命令（低 16 位是系统命令码）
const SC_MINIMIZE: usize = 0xF020;

/// 通过窗口标题查找顶层窗口的 HWND
fn find_window(title: &str) -> Option<HWND> {
    let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    let hwnd = unsafe { FindWindowW(PCWSTR::null(), PCWSTR(wide.as_ptr())) }.ok()?;
    if hwnd.is_invalid() {
        None
    } else {
        Some(hwnd)
    }
}

/// 隐藏窗口（最小化到托盘）
pub fn hide_window(title: &str) {
    if let Some(hwnd) = find_window(title) {
        unsafe { let _ = ShowWindow(hwnd, SW_HIDE); };
    }
}

/// 显示并激活窗口（从托盘恢复）
pub fn show_window(title: &str) {
    let Some(hwnd) = find_window(title) else {
        return;
    };
    unsafe {
        if IsWindowVisible(hwnd).as_bool() {
            // 可见但可能被最小化：还原
            if IsIconic(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }
        } else {
            let _ = ShowWindow(hwnd, SW_SHOW);
        }
        // 提到最前并抢焦点（托盘点击来自用户输入，SetForegroundWindow 可用）
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOP),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
        let _ = SetForegroundWindow(hwnd);
    }
}

/// 被替换前的原始窗口过程（子类化时保存，其余消息都转发给它）
static ORIGINAL_WNDPROC: AtomicIsize = AtomicIsize::new(0);

/// 子类化后的窗口过程：把最小化改成隐藏，其余消息原样转发
unsafe extern "system" fn tray_subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        // WM_SYSCOMMAND 的 wparam 低 4 位是系统内部使用位，需屏蔽后再比较
        if msg == WM_SYSCOMMAND && (wparam.0 & 0xFFF0) == SC_MINIMIZE {
            let _ = ShowWindow(hwnd, SW_HIDE);
            return LRESULT(0);
        }
        let original = ORIGINAL_WNDPROC.load(Ordering::Relaxed);
        CallWindowProcW(
            Some(std::mem::transmute::<
                isize,
                unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
            >(original)),
            hwnd,
            msg,
            wparam,
            lparam,
        )
    }
}

/// 把标题为 `title` 的窗口的“最小化”行为改为“隐藏到托盘”。
///
/// 必须在窗口创建后、且在主线程上调用一次。
/// 实现方式是经典的“窗口子类化”：替换 GWLP_WNDPROC 并保存旧过程，
/// 我们处理完关心的消息后，把剩余消息转发回旧过程，gpui 感知不到差别。
pub fn hook_minimize_to_tray(title: &str) -> bool {
    let Some(hwnd) = find_window(title) else {
        eprintln!("[win32] 未找到窗口，无法拦截最小化: {title}");
        return false;
    };
    let prev = unsafe {
        SetWindowLongPtrW(hwnd, GWLP_WNDPROC, tray_subclass_proc as *const () as isize)
    };
    if prev == 0 {
        eprintln!("[win32] SetWindowLongPtrW 失败，最小化将只缩到任务栏");
        return false;
    }
    ORIGINAL_WNDPROC.store(prev, Ordering::Relaxed);
    true
}
