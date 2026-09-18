//! # 主窗口（主界面 + 事件主控）
//!
//! [`AppModel`] 同时承担两个角色：
//!
//! 1. **主界面视图**：状态展示、服务地址配置（扫描 / 测试 / 保存）；
//! 2. **事件主控**：接收托盘、心跳监控线程、倒计时弹窗发来的
//!    [`AppEvent`]，驱动整个应用的状态机（弹倒计时、关机、托盘显隐等）。

use std::time::{Duration, Instant};

use futures::channel::mpsc::UnboundedSender;
use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants as _};
use gpui_kit::component::collapsible::Collapsible;
use gpui_kit::component::group_box::GroupBox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::label::Label;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::separator::Separator;
use gpui_kit::component::{ActiveTheme, Disableable as _, Icon, IconName, Root, Sizable as _, TitleBar};
use gpui_kit::{AppContext as _, Context, Entity, InteractiveElement as _, StatefulInteractiveElement as _, FontWeight, ParentElement as _, Render, Styled as _, Window, div, px};
use gpui_kit::prelude::FluentBuilder as _;

use crate::countdown;
use crate::events::{AppEvent, CountdownChoice, EventTx, HeartStatus, MonitorCommand};
use crate::protocol::{SNOOZE_LATER_SECONDS, WS_PORT};
use crate::win32;

/// 主窗口标题（同时用于 Win32 按标题查找窗口，必须全局唯一）
pub const MAIN_WINDOW_TITLE: &str = "自动关机守护 - 心跳失联自动关机";

// ---------------------------------------------------------------------------
// 本软件不持久化任何数据：每次启动都重新扫描局域网。
// 下面仅保留旧版本遗留配置文件的清理逻辑。
// ---------------------------------------------------------------------------

/// 旧版本配置文件路径（本软件已不再读写，仅用于启动时清理遗留文件）
fn legacy_config_path() -> Option<std::path::PathBuf> {
    std::env::var("APPDATA")
        .ok()
        .map(|base| std::path::PathBuf::from(base).join("auto-shutdown").join("config.json"))
}

// ---------------------------------------------------------------------------
// 主视图 + 主控
// ---------------------------------------------------------------------------

pub struct AppModel {
    /// 心跳状态（界面展示用）
    status: HeartStatus,
    /// 心跳丢失时的错误详情（展示用）
    lost_detail: Option<String>,
    /// 服务地址输入框
    input: Entity<InputState>,
    /// 已保存（并生效中）的服务地址
    saved_server: Option<String>,
    /// 是否正在扫描局域网
    scanning: bool,
    /// 「服务配置」折叠面板是否展开（默认收起）
    config_open: bool,
    /// 最近一次“测试”的结果
    test_result: Option<Result<String, String>>,
    /// 界面提示信息（Some(文本, 是否为错误)）
    hint: Option<(String, bool)>,
    /// 当前打开的倒计时弹窗（None 表示未打开）
    countdown: Option<gpui_kit::WindowHandle<Root>>,
    /// 冷却期：在此之前不要再次弹出倒计时（用户点了取消/稍后）
    snooze_until: Option<Instant>,
    /// 冷却任务代数：防止多次点击“取消”叠加出多个定时重弹任务
    snooze_gen: u64,
    /// 上行事件通道（转发给倒计时窗口等）
    events: EventTx,
    /// 下行命令通道（发给心跳监控线程）
    cmds: UnboundedSender<MonitorCommand>,
}

impl AppModel {
    pub fn new(
        events: EventTx,
        cmds: UnboundedSender<MonitorCommand>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // 拦截关闭按钮：不销毁窗口，而是隐藏到托盘
        window.on_window_should_close(cx, |_, _| {
            win32::hide_window(MAIN_WINDOW_TITLE);
            win32::notify_hidden();
            false
        });
        // 拦截最小化按钮：同样是隐藏到托盘（Win32 子类化实现）
        win32::hook_minimize_to_tray(MAIN_WINDOW_TITLE);

        // 不持久化任何数据：清理旧版本遗留的配置文件，然后每次启动都重新扫描
        if let Some(path) = legacy_config_path() {
            let _ = std::fs::remove_file(path);
        }
        let status = HeartStatus::NotConfigured;
        let scanning = true;
        let hint = Some((
            "正在扫描局域网（UDP 广播，发现端口 8124），请稍候…".to_string(),
            false,
        ));
        let _ = cmds.unbounded_send(MonitorCommand::Scan);

        Self {
            status,
            lost_detail: None,
            input: cx.new(|cx| {
                InputState::new(window, cx).placeholder(format!("例如: 192.168.1.100（默认端口 {WS_PORT}）"))
            }),
            saved_server: None,
            scanning,
            config_open: false,
            test_result: None,
            hint,
            countdown: None,
            snooze_until: None,
            snooze_gen: 0,
            events,
            cmds,
        }
    }

    // -----------------------------------------------------------------
    // 事件分发（UI 事件主循环在 main.rs，逐条调用到这里）
    // -----------------------------------------------------------------

    pub fn handle_event(&mut self, event: AppEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            AppEvent::HeartbeatOk => self.on_heartbeat_ok(cx),
            AppEvent::HeartbeatLost { detail } => self.on_heartbeat_lost(detail, cx),
            AppEvent::ScanFinished(result) => self.on_scan_finished(result, window, cx),
            AppEvent::TestFinished(result) => {
                self.test_result = Some(result);
                cx.notify();
            }
            AppEvent::TrayShow => win32::show_window(MAIN_WINDOW_TITLE),
            AppEvent::Quit => cx.quit(),
            // 窗口已收进托盘：清掉临时的测试结果，下次打开时界面是干净的
            AppEvent::WindowHidden => {
                if self.test_result.take().is_some() {
                    cx.notify();
                }
            }
            AppEvent::ShutdownNow => self.shutdown(cx),
            AppEvent::CountdownAction(choice) => self.on_countdown_choice(choice, cx),
        }
    }

    /// 心跳恢复：更新状态、关闭倒计时弹窗、清除冷却期
    fn on_heartbeat_ok(&mut self, cx: &mut Context<Self>) {
        if self.status != HeartStatus::Connected {
            self.status = HeartStatus::Connected;
            self.lost_detail = None;
            // 状态徽章已经表达“服务在线”，提示行清空避免重复
            self.hint = None;
        }
        // 若倒计时还开着（服务在倒计时期间恢复了），直接关掉它
        self.close_countdown(cx);
        self.snooze_until = None;
        self.snooze_gen += 1;
        cx.notify();
    }

    /// 心跳丢失超过阈值：置状态，按冷却期决定是否立即弹窗
    fn on_heartbeat_lost(&mut self, detail: String, cx: &mut Context<Self>) {
        self.status = HeartStatus::Lost;
        self.lost_detail = Some(detail);
        self.try_open_countdown(cx);
        cx.notify();
    }

    /// 扫描完成：找到设备则自动填入地址并直接启用监控；
    /// 没找到则提示用户手动输入。
    fn on_scan_finished(
        &mut self,
        result: Result<Vec<crate::events::Device>, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.scanning = false;
        match result {
            Ok(devices) if devices.is_empty() => {
                self.hint = Some((
                    "未在局域网内发现心跳服务。请确认服务端已开机，或手动输入服务地址。".into(),
                    false,
                ));
            }
            Ok(devices) => {
                let first = devices[0].clone();
                // 把找到的地址填入输入框
                self.input.update(cx, |state, cx| {
                    state.set_value(first.addr.clone(), window, cx)
                });
                // 扫描应答经过了加密握手校验，可以直接信任并启用监控
                self.apply_server(first.addr.clone());
                self.hint = Some((
                    format!(
                        "发现 {} 台设备：{}（{}），已自动连接并开始监控。",
                        devices.len(),
                        first.name,
                        first.addr
                    ),
                    false,
                ));
            }
            Err(err) => {
                self.hint = Some((format!("扫描失败: {err}"), true));
            }
        }
        cx.notify();
    }

    /// 切换监控目标（仅内存生效，不落盘；本软件不持久化任何数据）
    fn apply_server(&mut self, addr: String) {
        self.saved_server = Some(addr.clone());
        self.status = HeartStatus::Waiting;
        self.lost_detail = None;
        let _ = self.cmds.unbounded_send(MonitorCommand::SetTarget(Some(addr)));
    }

    /// 用户在倒计时弹窗上做出了选择
    fn on_countdown_choice(&mut self, choice: CountdownChoice, cx: &mut Context<Self>) {
        match choice {
            CountdownChoice::Cancel => {
                // 取消关机：本次彻底不提醒了。心跳监控线程在“恢复 -> 再次丢失”
                // 之前不会重复上报丢失事件，因此这里只需关窗，无需定时重弹；
                // 下次心跳恢复（HeartbeatOk）后自动复位，重新进入 60 秒丢失监测。
                self.close_countdown(cx);
                self.hint = Some((
                    "已取消本次关机。心跳恢复后会重新开始监测，若再次失联超过 60 秒将再次提醒。".into(),
                    false,
                ));
            }
            CountdownChoice::Later => {
                // 稍后关机：关闭弹窗并安排 30 秒后重新弹出完整倒计时
                self.close_countdown(cx);
                self.hint = Some((
                    format!("将在 {SNOOZE_LATER_SECONDS} 秒后再次弹出关机倒计时。"),
                    false,
                ));
                self.snooze_until = Some(Instant::now() + Duration::from_secs(SNOOZE_LATER_SECONDS));
                self.snooze_gen += 1;
                let snooze_gen = self.snooze_gen;
                cx.spawn(async move |weak, cx| {
                    cx.background_executor()
                        .timer(Duration::from_secs(SNOOZE_LATER_SECONDS))
                        .await;
                    // 冷却期到点：如果期间没有新的选择（代数没变）且服务仍未恢复，重新弹窗
                    let _ = weak.update(cx, |this, cx| {
                        if this.snooze_gen == snooze_gen {
                            this.try_open_countdown(cx);
                        }
                    });
                })
                .detach();
            }
        }
        cx.notify();
    }

    /// 若当前处于“心跳丢失”状态且没有弹窗在展示，打开倒计时弹窗
    fn try_open_countdown(&mut self, cx: &mut Context<Self>) {
        if self.status != HeartStatus::Lost || self.countdown.is_some() {
            return;
        }
        if let Some(until) = self.snooze_until
            && Instant::now() < until {
                return;
            }
        self.snooze_until = None;
        let handle = countdown::open_countdown_window(self.events.clone(), cx);
        self.countdown = Some(handle);
        // 弹窗时同步主界面的提示
        self.hint = Some(("检测到心跳服务失联，已弹出关机倒计时！".into(), true));
    }

    /// 关闭倒计时弹窗（如果开着）
    fn close_countdown(&mut self, cx: &mut Context<Self>) {
        if let Some(handle) = self.countdown.take() {
            let _ = handle.update(cx, |_, window, _| window.remove_window());
        }
    }

    /// 执行 Windows 关机命令
    fn shutdown(&mut self, cx: &mut Context<Self>) {
        self.close_countdown(cx);
        self.hint = Some(("正在执行关机…".into(), false));
        cx.notify();
        eprintln!("[shutdown] 触发系统关机");
        let result = std::process::Command::new("shutdown")
            .args(["/s", "/t", "0", "/c", "自动关机守护：心跳失联自动关机"])
            .spawn();
        if let Err(e) = result {
            eprintln!("[shutdown] 执行失败: {e}");
            self.hint = Some((format!("关机命令执行失败: {e}"), true));
            cx.notify();
        }
    }

    // -----------------------------------------------------------------
    // 界面按钮回调
    // -----------------------------------------------------------------

    /// “扫描局域网”按钮
    fn start_scan(&mut self, cx: &mut Context<Self>) {
        self.scanning = true;
        self.hint = Some(("正在扫描局域网，请稍候…".into(), false));
        let _ = self.cmds.unbounded_send(MonitorCommand::Scan);
        cx.notify();
    }

    /// “测试”按钮：对输入框中的地址做一次完整的心跳握手
    fn start_test(&mut self, cx: &mut Context<Self>) {
        let addr = self.input.read(cx).value().trim().to_string();
        if addr.is_empty() {
            self.test_result = Some(Err("请先输入服务地址".into()));
            cx.notify();
            return;
        }
        self.test_result = Some(Err("测试中…".into()));
        let _ = self.cmds.unbounded_send(MonitorCommand::Test(addr));
        cx.notify();
    }

    /// “保存并启动监控”按钮：写配置文件并切换监控目标
    fn save_and_start(&mut self, cx: &mut Context<Self>) {
        let addr = self.input.read(cx).value().trim().to_string();
        if addr.is_empty() {
            self.hint = Some(("请先输入服务地址".into(), true));
            cx.notify();
            return;
        }
        self.apply_server(addr);
        self.test_result = None;
        self.hint = Some((
            "已开始监控（地址仅本次运行生效，重启后重新扫描或输入）。首次连通前不会触发关机（防误报）。".into(),
            false,
        ));
        cx.notify();
    }
}

impl Render for AppModel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl gpui_kit::IntoElement {
        let theme = cx.theme();

        // ---- 状态（图标 + tooltip 文案 + 着色）----
        let (status_text, status_color, status_icon) = match self.status {
            HeartStatus::NotConfigured => ("尚未配置服务地址", theme.muted_foreground, IconName::Info),
            HeartStatus::Waiting => ("等待首次连通心跳服务", theme.warning, IconName::BatteryCharging),
            HeartStatus::Connected => ("服务在线，心跳正常", theme.success, IconName::CircleCheck),
            HeartStatus::Lost => ("心跳丢失！服务可能已离线", theme.danger, IconName::TriangleAlert),
        };

        // ---- 提示信息（Alert 组件，按严重程度着色）----
        let hint_el = self.hint.as_ref().map(|(text, is_err)| {
            if *is_err {
                Alert::error("hint", text.clone())
            } else {
                Alert::info("hint", text.clone())
            }
            .small()
        });

        // ---- 测试结果（Alert 组件）----
        let test_el = self.test_result.as_ref().map(|r| match r {
            Ok(msg) => Alert::success("test-result", msg.clone()).small(),
            Err(msg) => Alert::error("test-result", msg.clone()).small(),
        });

        // ---- 丢失原因 ----
        let lost_el = self.lost_detail.as_ref().map(|d| {
            Alert::error("lost-detail", format!("丢失原因: {d}")).small()
        });

        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            // ---- 自绘标题栏（拖拽区 + 最小化/关闭按钮）----
            .child(TitleBar::new().child(div().ml_3().text_size(px(13.)).child("自动关机守护")))
            .child(
                v_flex()
                    .flex_1()
                    .p_4()
                    .gap_4()
                    // 内容超出窗口高度时出现滚动条，不用手动拉大窗口
                    .overflow_y_scrollbar()
                    // ---- 顶部：监控目标卡片（左：目标地址；右：状态图标）----
                    .child(
                        div()
                            .border_1()
                            .border_color(theme.border)
                            .rounded(px(10.))
                            .p_4()
                            .child(
                                h_flex()
                                    .justify_between()
                                    .items_center()
                                    .child(
                                        v_flex()
                                            .gap_0p5()
                                            .child(
                                                Label::new("监控目标")
                                                    .text_color(theme.muted_foreground)
                                                    .text_size(px(12.)),
                                            )
                                            .child(
                                                Label::new(
                                                    self.saved_server
                                                        .as_deref()
                                                        .unwrap_or("尚未配置"),
                                                )
                                                .text_size(px(18.))
                                                .font_weight(FontWeight::BOLD),
                                            ),
                                    )
                                    .child(
                                        // 图标按钮承载状态：颜色随状态变化，悬停 tooltip 显示具体文案
                                        Button::new("status")
                                            .custom(
                                                ButtonCustomVariant::new(cx)
                                                    .foreground(status_color)
                                                    .hover(theme.transparent),
                                            )
                                            .icon(status_icon)
                                            .tooltip(status_text),
                                    ),
                            ),
                    )
                    .when_some(lost_el, |el, a| el.child(a))
                    // ---- 服务配置（Collapsible，默认收起，点击触发行展开/收起）----
                    .child(
                        Collapsible::new()
                            .open(self.config_open)
                            .border_1()
                            .border_color(theme.border)
                            .rounded(px(10.))
                            .overflow_hidden()
                            // 触发行
                            .child(
                                div()
                                    .id("config-trigger")
                                    .cursor_pointer()
                                    .p_3()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.config_open = !this.config_open;
                                        cx.notify();
                                    }))
                                    .child(
                                        h_flex()
                                            .justify_between()
                                            .items_center()
                                            .child(
                                                Label::new("服务配置")
                                                    .text_size(px(14.))
                                                    .font_weight(FontWeight::MEDIUM),
                                            )
                                            .child(
                                                Icon::new(if self.config_open {
                                                    IconName::ChevronUp
                                                } else {
                                                    IconName::ChevronDown
                                                })
                                                .text_color(theme.muted_foreground),
                                            ),
                                    ),
                            )
                            // 展开内容
                            .content(
                                div()
                                    .px_3()
                                    .pb_3()
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .items_center()
                                            .child(
                                                div().flex_1().child(Input::new(&self.input).small()),
                                            )
                                            .child(
                                                Button::new("scan")
                                                    .small()
                                                    .label(if self.scanning {
                                                        "扫描中…"
                                                    } else {
                                                        "扫描局域网"
                                                    })
                                                    .disabled(self.scanning)
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.start_scan(cx)
                                                    })),
                                            )
                                            .child(
                                                Button::new("test")
                                                    .small()
                                                    .label("测试")
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.start_test(cx)
                                                    })),
                                            )
                                            .child(
                                                Button::new("save")
                                                    .small()
                                                    .primary()
                                                    .label("保存并启动监控")
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.save_and_start(cx)
                                                    })),
                                            ),
                                    ),
                            ),
                    )
                    .when_some(hint_el, |el, a| el.child(a))
                    .when_some(test_el, |el, a| el.child(a))
                    // ---- 使用说明 ----
                    .child(
                        GroupBox::new().title("使用说明").child(
                            v_flex()
                                .gap_1()
                                .child(
                                    Label::new("1. 一台常驻的 WebSocket 心跳服务端（默认端口 8123），任何 socket 服务均可；")
                                        .text_color(theme.muted_foreground)
                                        .text_size(px(12.)),
                                )
                                .child(
                                    Label::new("2. 本软件每 3 秒向它发送一次加密心跳，只要能收到应答，就说明服务在线；")
                                        .text_color(theme.muted_foreground)
                                        .text_size(px(12.)),
                                )
                                .child(
                                    Label::new("3. 如果连续 60 秒收不到任何应答，判定服务离线，弹出置顶的 60 秒关机倒计时；")
                                        .text_color(theme.muted_foreground)
                                        .text_size(px(12.)),
                                )
                                .child(
                                    Label::new("4. 倒计时无人干预会自动关机；也可选择立即关机 / 取消 / 稍后（30 秒后再次提醒）。")
                                        .text_color(theme.muted_foreground)
                                        .text_size(px(12.)),
                                )
                                .child(
                                    Separator::horizontal(),
                                )
                                .child(
                                    Label::new("提示: 关闭或最小化窗口都会收到托盘；右键托盘图标可显示窗口或退出。")
                                        .text_color(theme.muted_foreground)
                                        .text_size(px(12.)),
                                ),
                        ),
                    ),
            )
    }
}
