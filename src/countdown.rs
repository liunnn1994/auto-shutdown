//! # 关机倒计时弹窗
//!
//! 心跳服务失联（心跳丢失超过 60 秒）时弹出的**置顶**独立窗口：
//!
//! - 一个不断刷新的 60 秒倒计时（`ProgressCircle` 环形进度 + 剩余秒数）；
//! - 三个标准 `Button`：
//!   - **立即关机**：马上执行关机命令；
//!   - **取消关机**：关闭弹窗，心跳恢复后重新开始监测；
//!   - **稍后关机 (30s)**：关闭弹窗，30 秒后再次弹出完整倒计时。
//! - 若倒计时期间服务恢复（心跳恢复），主控逻辑会直接关闭本弹窗。
//!
//! 窗口使用 `WindowKind::PopUp`，在 Windows 上对应 `WS_EX_TOPMOST`，
//! 始终悬浮在所有普通窗口之上。
//!
//! 注意：内容直接用组件库组件布局，不走 `Dialog` overlay ——
//! PopUp 窗口里 overlay 的渲染不可靠（曾出现过整窗空白）。

use std::time::Duration;

use futures::channel::mpsc::UnboundedSender;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::label::Label;
use gpui_kit::component::progress::ProgressCircle;
use gpui_kit::component::{ActiveTheme, Icon, IconName, Root, Sizable as _, Size};
use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, Context, FontWeight, ParentElement as _, Render, Styled as _, Window,
    WindowBounds, WindowOptions, div, px, size,
};

use crate::events::{AppEvent, CountdownChoice};
use crate::protocol::COUNTDOWN_SECONDS;

/// 倒计时窗口标题（也用于 Win32 查找窗口句柄，必须唯一）
pub const COUNTDOWN_TITLE: &str = "自动关机守护 - 关机倒计时警告";
/// 倒计时窗口尺寸
const WINDOW_SIZE: (f32, f32) = (520., 360.);

pub struct CountdownView {
    /// 剩余秒数（每秒 -1，归零触发关机）
    remaining: u32,
    /// 事件通道（按钮点击 / 归零关机都从这里发给主循环）
    events: UnboundedSender<AppEvent>,
}

impl CountdownView {
    pub fn new(events: UnboundedSender<AppEvent>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        // 每秒 tick 一次：递减 remaining 并刷新界面；归零后通知主循环关机。
        let tx = events.clone();
        cx.spawn(async move |weak, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                // update 返回 Err 说明视图已被销毁（窗口关闭），结束 tick 循环
                let Ok(remaining) = weak.update(cx, |view, cx| {
                    view.remaining = view.remaining.saturating_sub(1);
                    cx.notify();
                    view.remaining
                }) else {
                    break;
                };
                if remaining == 0 {
                    let _ = tx.unbounded_send(AppEvent::ShutdownNow);
                    break;
                }
            }
        })
        .detach();

        Self {
            remaining: COUNTDOWN_SECONDS as u32,
            events,
        }
    }
}

impl Render for CountdownView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl gpui_kit::IntoElement {
        let theme = cx.theme();
        let danger = theme.danger;
        let muted = theme.muted_foreground;
        let pct = self.remaining as f32 / COUNTDOWN_SECONDS as f32 * 100.;

        // 各按钮共用的事件发送端（闭包按值捕获，逐个克隆）
        let tx_shutdown = self.events.clone();
        let tx_cancel = self.events.clone();
        let tx_later = self.events.clone();

        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(
                v_flex()
                    .gap_4()
                    .items_center()
                    // ---- 标题 ----
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(Icon::new(IconName::TriangleAlert).large().text_color(danger))
                            .child(
                                Label::new("心跳失联警告")
                                    .text_size(px(20.))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(danger),
                            ),
                    )
                    // ---- 说明 ----
                    .child(
                        v_flex().gap_1().items_center().child(
                            Label::new("心跳服务已失联超过 60 秒，服务端可能已断电或离线。")
                                .text_color(muted),
                        ),
                    )
                    // ---- 环形倒计时 ----
                    .child(
                        ProgressCircle::new("countdown")
                            .with_size(Size::Size(px(128.)))
                            .color(danger)
                            .value(pct)
                            .child(
                                v_flex().items_center().child(
                                    div()
                                        .text_size(px(40.))
                                        .font_weight(FontWeight::BOLD)
                                        .text_color(danger)
                                        .child(format!("{}", self.remaining)),
                                ),
                            ),
                    )
                    .child(Label::new("秒后自动关机").text_color(muted))
                    // ---- 按钮组 ----
                    .child(
                        h_flex()
                            .mt_2()
                            .gap_2()
                            .child(
                                Button::new("shutdown")
                                    .danger()
                                    .label("立即关机")
                                    .on_click(move |_, _, _| {
                                        let _ = tx_shutdown.unbounded_send(AppEvent::ShutdownNow);
                                    }),
                            )
                            .child(
                                Button::new("cancel")
                                    .ghost()
                                    .label("取消关机")
                                    .on_click(move |_, _, _| {
                                        let _ = tx_cancel
                                            .unbounded_send(AppEvent::CountdownAction(CountdownChoice::Cancel));
                                    }),
                            )
                            .child(
                                Button::new("later")
                                    .secondary()
                                    .label("稍后关机 (30s)")
                                    .on_click(move |_, _, _| {
                                        let _ = tx_later
                                            .unbounded_send(AppEvent::CountdownAction(CountdownChoice::Later));
                                    }),
                            ),
                    ),
            )
    }
}

/// 打开倒计时窗口（在持有 gpui App 上下文时调用）。
/// 返回窗口根视图（Root）的句柄，用于之后关闭该窗口。
pub fn open_countdown_window(
    events: UnboundedSender<AppEvent>,
    cx: &mut App,
) -> gpui_kit::WindowHandle<Root> {
    let bounds = WindowBounds::centered(size(px(WINDOW_SIZE.0), px(WINDOW_SIZE.1)), cx);
    let options = WindowOptions {
        window_bounds: Some(bounds),
        titlebar: Some(gpui_kit::TitlebarOptions {
            title: Some(COUNTDOWN_TITLE.into()),
            appears_transparent: true,
            traffic_light_position: None,
        }),
        focus: true,
        show: true,
        kind: gpui_kit::WindowKind::PopUp,
        is_movable: true,
        is_resizable: false,
        is_minimizable: false,
        window_min_size: Some(size(px(WINDOW_SIZE.0), px(WINDOW_SIZE.1))),
        ..Default::default()
    };

    cx.open_window(options, |window, cx| {
        let view: gpui_kit::AnyView = cx.new(|cx| CountdownView::new(events, window, cx)).into();
        cx.new(|cx| Root::new(view, window, cx))
    })
    .expect("打开倒计时窗口失败")
}
