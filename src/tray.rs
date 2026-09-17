//! # 系统托盘
//!
//! gpui 没有内置托盘支持，这里使用 `tray-icon` crate 实现。
//! 托盘图标与菜单在本进程内创建后永久保留（`Box::leak`），随进程退出销毁。
//!
//! 交互约定：
//! - 左键双击托盘图标 → 显示主窗口；
//! - 右键托盘 → 菜单（显示主窗口 / 退出程序）。

use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{TrayIconBuilder, TrayIconEvent};

use crate::events::AppEvent;

/// 托盘菜单项 id：显示主窗口
const MENU_SHOW: &str = "show";
/// 托盘菜单项 id：退出程序
const MENU_QUIT: &str = "quit";

/// 创建托盘（必须在主线程调用）。
///
/// 所有菜单/图标事件通过 `events` 通道转发给 UI 主循环处理。
pub fn create_tray(events: crate::events::EventTx) {
    // ---- 菜单 ----
    type Acc = tray_icon::menu::accelerator::Accelerator;
    let show_item = MenuItem::with_id(MENU_SHOW, "显示主窗口", true, None::<Acc>);
    let quit_item = MenuItem::with_id(MENU_QUIT, "退出程序", true, None::<Acc>);
    let menu = Menu::with_items(&[&show_item, &quit_item]).expect("创建托盘菜单失败");

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
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        let ev = match e.id().as_ref() {
            MENU_SHOW => AppEvent::TrayShow,
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
