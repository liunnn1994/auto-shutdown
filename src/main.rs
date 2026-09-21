//! # auto-shutdown —— 自动关机守护（心跳失联自动关机）
//!
//! 工作方式概览：
//!
//! 1. 一台常驻的 WebSocket 心跳服务端（任何 socket 服务均可；本项目以
//!    插在市电上的 ESP32 为例，参考 `python/heartbeat_server.py`，可原样移植）；
//! 2. 本软件在 PC 上运行：主界面负责配置与状态展示，托盘常驻，
//!    后台线程每 3 秒发送一次加密心跳；
//! 3. 若连续 60 秒心跳无响应 → 判定服务失联 → 弹出置顶的 60 秒
//!    关机倒计时，倒计时结束无人干预则自动关机。
//!
//! 模块划分：
//! - [`protocol`] 通信协议常量与报文定义
//! - [`crypto`]   报文加解密（AES-256-CTR + HMAC-SHA256）
//! - [`monitor`]  心跳 / 扫描 / 测试（后台线程）
//! - [`tray`]     系统托盘
//! - [`autostart`] 开机启动开关（任务计划程序，无需管理员权限）
//! - [`win32`]    原生窗口辅助（隐藏 / 恢复 / 拦截最小化）
//! - [`countdown`] 关机倒计时弹窗
//! - [`app`]      主界面与事件主控
//! - [`logger`]   文件日志（~/.auto-shutdown/logs，按天分割，保留 30 天）
//!
//! release 构建下标记为纯 GUI 程序，启动时不再弹出控制台黑框；
//! debug 构建保留控制台以便查看日志输出。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod autostart;
mod countdown;
mod crypto;
mod events;
mod logger;
mod monitor;
mod protocol;
mod tray;
mod win32;

use std::cell::RefCell;
use std::rc::Rc;

use futures::StreamExt;
use gpui_kit::component::Root;
use gpui_kit::{AppContext as _, Entity, TitlebarOptions, WindowBounds, WindowOptions, px, size};

use app::{AppModel, MAIN_WINDOW_TITLE};
use events::{AppEvent, MonitorCommand};

fn main() {
    // 单实例保护：已有实例运行时，把它的主窗口恢复置前，然后直接退出
    if !win32::ensure_single_instance(MAIN_WINDOW_TITLE) {
        return;
    }

    // 文件日志：~/.auto-shutdown/logs/，按天分割，自动清理 30 天前的日志
    logger::init();
    crate::log_info!("程序启动 v{}", env!("CARGO_PKG_VERSION"));
    // panic 也留痕（release 下 panic = abort，hook 仍会先执行再退出）
    std::panic::set_hook(Box::new(|info| {
        crate::log_error!("panic: {info}");
    }));

    // 事件通道：所有后台来源（托盘 / 心跳线程 / 倒计时弹窗）统一发到这里，
    // 由 UI 主循环串行处理，避免多线程同时操作 UI 状态。
    let (event_tx, mut event_rx) = futures::channel::mpsc::unbounded::<AppEvent>();
    // 命令通道：UI -> 心跳监控线程
    let (cmd_tx, cmd_rx) = futures::channel::mpsc::unbounded::<MonitorCommand>();

    let app = gpui_kit::application().with_assets(gpui_kit::assets::Assets);
    app.run(move |cx| {
        gpui_kit::init(cx);

        // 注册事件发送端（win32 子类化过程需要用它通知“窗口已隐藏”）
        win32::set_event_tx(event_tx.clone());

        // 创建系统托盘（必须在主线程）
        tray::create_tray(event_tx.clone());

        // 启动心跳监控线程（内部自带 tokio runtime）
        monitor::spawn(cmd_rx, event_tx.clone());

        cx.spawn(async move |cx| {
            // 计算主窗口初始位置（屏幕居中，高度与最小高度一致，紧凑显示）
            let bounds = cx.update(|cx| WindowBounds::centered(size(px(560.), px(430.)), cx));

            // AppModel 实体通过槽位从窗口构建闭包里带出来：
            // 窗口根视图必须是 Root，因此 open_window 返回的是 Root 的句柄，
            // 而事件处理需要的是 AppModel 实体本身。
            let model_slot: Rc<RefCell<Option<Entity<AppModel>>>> = Rc::new(RefCell::new(None));
            let slot_for_closure = model_slot.clone();

            let main_window = cx
                .open_window(main_window_options(Some(bounds)), |window, cx| {
                    let model =
                        cx.new(|cx| AppModel::new(event_tx.clone(), cmd_tx.clone(), window, cx));
                    *slot_for_closure.borrow_mut() = Some(model.clone());
                    // 第一层视图必须是 Root
                    let view: gpui_kit::AnyView = model.into();
                    cx.new(|cx| Root::new(view, window, cx))
                })
                .expect("打开主窗口失败");

            let model = model_slot.borrow().clone().expect("主视图实体未初始化");
            drop(model_slot);

            // 事件主循环
            while let Some(event) = event_rx.next().await {
                // 退出：直接结束整个应用
                if matches!(event, AppEvent::Quit) {
                    crate::log_info!("收到退出事件，程序退出");
                    cx.update(|cx| cx.quit());
                    break;
                }
                // 通过窗口句柄进入，既能拿到 Window（改输入框内容等需要），
                // 也能拿到 App 上下文更新 AppModel 实体。
                let _ = main_window.update(cx, |_, window, cx| {
                    model.update(cx, |model, cx| model.handle_event(event, window, cx))
                });
            }
        })
        .detach();
    });
}

/// 主窗口参数：自绘标题栏（拦截关闭/最小化到托盘的关键前提）+ 固定初始尺寸
fn main_window_options(window_bounds: Option<WindowBounds>) -> WindowOptions {
    WindowOptions {
        window_bounds,
        titlebar: Some(TitlebarOptions {
            title: Some(MAIN_WINDOW_TITLE.into()),
            appears_transparent: true,
            traffic_light_position: None,
        }),
        // Windows 上由 TitleBar 自绘控制按钮（TitleBar::window_options 的做法）
        app_owns_titlebar_drag: true,
        window_min_size: Some(size(px(520.), px(390.))),
        ..Default::default()
    }
}
