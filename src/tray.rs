//! # 系统托盘
//!
//! gpui 没有内置托盘支持，这里使用 `tray-icon` crate 实现。
//! 托盘图标与菜单在本进程内创建后永久保留（`Box::leak`），随进程退出销毁。
//!
//! 交互约定：
//! - 左键双击托盘图标 → 显示主窗口；
//! - 右键托盘 → 菜单（显示主窗口 / 开机启动开关 / 退出程序）。

use std::sync::OnceLock;

use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIconBuilder, TrayIconEvent};

use crate::events::AppEvent;

/// 托盘菜单项 id：显示主窗口
const MENU_SHOW: &str = "show";
/// 托盘菜单项 id：开机启动开关
const MENU_AUTOSTART: &str = "autostart";
/// 托盘菜单项 id：退出程序
const MENU_QUIT: &str = "quit";

/// muda 的菜单项内部基于 `Rc`，不实现 `Send`，而 `set_event_handler`
/// 要求闭包 `Send + Sync`。菜单（及复选项）在主线程创建，Windows 上
/// 菜单事件回调也始终在创建线程（主线程）派发，因此经此包装在闭包中
/// 持有菜单项实际从不跨线程使用，是安全的。
#[derive(Clone)]
struct SendItem(CheckMenuItem);
unsafe impl Send for SendItem {}
unsafe impl Sync for SendItem {}

impl SendItem {
    fn is_checked(&self) -> bool {
        self.0.is_checked()
    }
    fn set_checked(&self, checked: bool) {
        self.0.set_checked(checked)
    }
}

/// 复选项句柄：主界面开关与托盘菜单控制同一功能，
/// 这里全局存一份，供任意一侧同步 √ 状态。
static AUTOSTART_ITEM: OnceLock<SendItem> = OnceLock::new();

/// 同步托盘菜单“开机启动”复选项的 √ 状态（主界面切换后调用）
pub fn sync_autostart_checked(checked: bool) {
    if let Some(item) = AUTOSTART_ITEM.get() {
        item.set_checked(checked);
    }
}

/// 创建托盘（必须在主线程调用）。
///
/// 所有菜单/图标事件通过 `events` 通道转发给 UI 主循环处理。
pub fn create_tray(events: crate::events::EventTx) {
    // ---- 菜单 ----
    type Acc = tray_icon::menu::accelerator::Accelerator;
    let show_item = MenuItem::with_id(MENU_SHOW, "显示主窗口", true, None::<Acc>);
    // 复选项：√ 状态直接以计划任务的当前状态为准
    let autostart_item = CheckMenuItem::with_id(
        MENU_AUTOSTART,
        "开机启动",
        true,
        crate::autostart::is_enabled(),
        None::<Acc>,
    );
    let quit_item = MenuItem::with_id(MENU_QUIT, "退出程序", true, None::<Acc>);
    let menu = Menu::with_items(&[
        &show_item,
        &PredefinedMenuItem::separator(),
        &autostart_item,
        &PredefinedMenuItem::separator(),
        &quit_item,
    ])
    .expect("创建托盘菜单失败");

    // ---- 图标与托盘本体 ----
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("自动关机守护：心跳失联自动关机")
        .with_icon(build_icon())
        .build()
        .expect("创建托盘失败");

    // 托盘必须与进程同寿命，这里直接泄漏（数量只有 1 个，体积可忽略）。
    std::mem::forget(tray);

    // ---- 右键菜单事件 ----
    let tx = events.clone();
    let autostart_for_events = SendItem(autostart_item.clone());
    let _ = AUTOSTART_ITEM.set(autostart_for_events.clone());
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        let ev = match e.id().as_ref() {
            MENU_SHOW => AppEvent::TrayShow,
            MENU_AUTOSTART => {
                // 切换计划任务，成功后同步 √ 状态并通知主界面；失败则保持原状
                let target = !autostart_for_events.is_checked();
                match crate::autostart::set_enabled(target) {
                    Ok(()) => {
                        autostart_for_events.set_checked(target);
                        AppEvent::AutostartChanged(target)
                    }
                    Err(err) => {
                        eprintln!("[tray] 切换开机启动失败: {err}");
                        return;
                    }
                }
            }
            MENU_QUIT => AppEvent::Quit,
            _ => return,
        };
        let _ = tx.unbounded_send(ev);
    }));

    // ---- 托盘图标本身的事件（双击显示主窗口）----
    let tx = events;
    TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
        if matches!(e, TrayIconEvent::DoubleClick { .. }) {
            let _ = tx.unbounded_send(AppEvent::TrayShow);
        }
    }));
}

/// 程序化生成一枚 32x32 托盘图标：琥珀色圆角方块 + 白色闪电，
/// 意为“电力”主题。使用 4 倍超采样做简单的抗锯齿。
fn build_icon() -> tray_icon::Icon {
    const S: u32 = 32;
    const SS: u32 = 4; // 每像素超采样倍数

    // 闪电多边形（32px 坐标系）
    let bolt: [(f32, f32); 6] = [
        (19.0, 2.0),
        (8.0, 18.0),
        (15.0, 18.0),
        (12.0, 30.0),
        (24.0, 13.0),
        (16.5, 13.0),
    ];
    // 琥珀色（amber-500）背景、白色闪电
    let bg = (245.0, 158.0, 11.0);
    let fg = (255.0, 255.0, 255.0);

    let mut rgba = Vec::with_capacity((S * S * 4) as usize);
    for y in 0..S {
        for x in 0..S {
            let (mut r, mut g, mut b) = (0.0, 0.0, 0.0);
            let mut hit = 0u32;
            // 4x4 子像素采样
            for sy in 0..SS {
                for sx in 0..SS {
                    let px = x as f32 + (sx as f32 + 0.5) / SS as f32;
                    let py = y as f32 + (sy as f32 + 0.5) / SS as f32;
                    let color = if inside_bolt(px, py, &bolt) {
                        fg
                    } else if inside_rounded_square(px, py, S as f32, 7.0) {
                        bg
                    } else {
                        continue;
                    };
                    r += color.0;
                    g += color.1;
                    b += color.2;
                    hit += 1;
                }
            }
            let n = (SS * SS) as f32;
            let a = if hit == 0 { 0 } else { 255 };
            let (r, g, b) = if hit == 0 { (0.0, 0.0, 0.0) } else { (r / n, g / n, b / n) };
            rgba.extend_from_slice(&[r as u8, g as u8, b as u8, a]);
        }
    }

    tray_icon::Icon::from_rgba(rgba, S, S).expect("托盘图标尺寸非法")
}

/// 判断点是否在圆角方块内
fn inside_rounded_square(px: f32, py: f32, size: f32, radius: f32) -> bool {
    if px < 0.0 || py < 0.0 || px > size || py > size {
        return false;
    }
    let x = px.min(size - px);
    let y = py.min(size - py);
    // 四角区域才需要做圆角判断
    if x < radius && y < radius {
        let dx = radius - x;
        let dy = radius - y;
        dx * dx + dy * dy <= radius * radius
    } else {
        true
    }
}

/// 射线法判断点是否在多边形内
fn inside_bolt(px: f32, py: f32, poly: &[(f32, f32)]) -> bool {
    let mut inside = false;
    let n = poly.len();
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = poly[i];
        let (xj, yj) = poly[j];
        let intersect = (yi > py) != (yj > py)
            && px < (xj - xi) * (py - yi) / (yj - yi) + xi;
        if intersect {
            inside = !inside;
        }
        j = i;
    }
    inside
}
