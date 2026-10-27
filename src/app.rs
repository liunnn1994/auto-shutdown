//! # 主窗口（主界面 + 事件主控）
//!
//! [`AppModel`] 同时承担两个角色：
//!
//! 1. **主界面视图**：状态展示、广播设备配置（扫描 / 测试 / 启动）；
//! 2. **事件主控**：接收托盘、心跳监控线程、倒计时弹窗发来的
//!    [`AppEvent`]，驱动整个应用的状态机（弹倒计时、关机、托盘显隐等）。

use std::time::{Duration, Instant};

use futures::channel::mpsc::UnboundedSender;
use gpui_kit::base::{h_flex, v_flex};
use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::collapsible::Collapsible;
use gpui_kit::component::form::{Field, Form};
use gpui_kit::component::group_box::{GroupBox, GroupBoxVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::label::Label;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::separator::Separator;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme, Disableable as _, Icon, IconName, Root, Sizable as _, TitleBar,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, Context, Entity, FontWeight, ParentElement as _, Render, Styled as _, Window,
    px,
};

use crate::config::{Settings, positive_seconds};
use crate::countdown;
use crate::events::{AppEvent, CountdownChoice, EventTx, HeartStatus, MonitorCommand};
use crate::protocol::SNOOZE_LATER_SECONDS;
use crate::win32;

/// 主窗口标题（同时用于 Win32 按标题查找窗口，必须全局唯一）
pub const MAIN_WINDOW_TITLE: &str = "自动关机守护 - 心跳失联自动关机";

// ---------------------------------------------------------------------------
// 广播设备每次启动重新发现；关机策略独立持久化。
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// 主视图 + 主控
// ---------------------------------------------------------------------------

pub struct AppModel {
    /// 心跳状态（界面展示用）
    status: HeartStatus,
    /// 心跳丢失时的错误详情（展示用）
    lost_detail: Option<String>,
    /// 广播设备标识输入框
    input: Entity<InputState>,
    timeout_input: Entity<InputState>,
    countdown_input: Entity<InputState>,
    settings: Settings,
    settings_hint: Option<(String, bool)>,
    /// 生效中的广播设备标识
    saved_server: Option<String>,
    /// 是否正在扫描广播
    scanning: bool,
    /// 「配置」折叠面板是否展开（默认收起）
    config_open: bool,
    /// 最近一次“测试”的结果
    test_result: Option<Result<String, String>>,
    /// 开机启动（任务计划程序中的计划任务是否存在）
    autostart: bool,
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
        settings: Settings,
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

        let status = HeartStatus::NotConfigured;
        let scanning = true;
        let hint = Some((
            "正在扫描开发板无线广播并验证随机挑战，请稍候…".to_string(),
            false,
        ));
        crate::log_info!("已发起开发板无线广播认证扫描（无需连接开发板网络）");
        let _ = cmds.unbounded_send(MonitorCommand::Scan);

        Self {
            status,
            lost_detail: None,
            input: cx.new(|cx| InputState::new(window, cx).placeholder("例如: radio://d86071")),
            timeout_input: cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(settings.heartbeat_timeout_seconds.to_string())
                    .validate(|value, _| value.is_empty() || positive_seconds(value).is_some())
            }),
            countdown_input: cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(settings.countdown_seconds.to_string())
                    .validate(|value, _| value.is_empty() || positive_seconds(value).is_some())
            }),
            settings,
            settings_hint: None,
            saved_server: None,
            scanning,
            config_open: false,
            test_result: None,
            autostart: crate::autostart::is_enabled(),
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
            AppEvent::HeartbeatUnconfirmed { detail } => {
                crate::log_warn!("最新广播认证未确认，界面切换为等待确认: {detail}");
                self.status = HeartStatus::Unconfirmed;
                self.lost_detail = Some(detail);
                self.hint = Some((
                    format!(
                        "未收到最新认证应答，正在确认设备状态；持续失联 {} 秒后才会启动关机倒计时。",
                        self.settings.heartbeat_timeout_seconds
                    ),
                    false,
                ));
                cx.notify();
            }
            AppEvent::HeartbeatLost { detail } => self.on_heartbeat_lost(detail, cx),
            AppEvent::ScanFinished(result) => self.on_scan_finished(result, window, cx),
            AppEvent::TestFinished(result) => {
                match &result {
                    Ok(msg) => crate::log_info!("手动测试成功: {msg}"),
                    Err(msg) => crate::log_warn!("手动测试失败: {msg}"),
                }
                self.test_result = Some(result);
                cx.notify();
            }
            AppEvent::TrayShow => {
                crate::log_info!("托盘：显示主窗口");
                win32::show_window(MAIN_WINDOW_TITLE);
            }
            AppEvent::Quit => cx.quit(),
            AppEvent::AutostartChanged(enabled) => {
                // 托盘菜单切换了开机启动，同步主界面开关
                crate::log_info!("开机启动已切换为: {enabled}");
                self.autostart = enabled;
                cx.notify();
            }
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
            crate::log_info!("心跳恢复，状态置为“服务在线”");
            self.status = HeartStatus::Connected;
            self.lost_detail = None;
            // 状态徽章已经表达“服务在线”，提示行清空避免重复
            self.hint = None;
        }
        // 若倒计时还开着（服务在倒计时期间恢复了），直接关掉它
        if self.countdown.is_some() {
            crate::log_info!("服务在倒计时期间恢复，自动关闭倒计时弹窗");
        }
        self.close_countdown(cx);
        self.snooze_until = None;
        self.snooze_gen += 1;
        cx.notify();
    }

    /// 心跳丢失超过阈值：置状态，按冷却期决定是否立即弹窗
    fn on_heartbeat_lost(&mut self, detail: String, cx: &mut Context<Self>) {
        crate::log_error!("收到心跳失联事件: {detail}");
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
                crate::log_info!("扫描完成：未发现心跳服务");
                self.hint = Some((
                    "未发现通过认证的开发板广播。请确认两端均为 ASR2 版本、无线网卡开启且允许定位访问；将继续扫描。".into(),
                    false,
                ));
            }
            Ok(devices) => {
                let first = devices[0].clone();
                crate::log_info!(
                    "扫描完成：发现 {} 台设备，取第一台 {}（{}）",
                    devices.len(),
                    first.name,
                    first.addr
                );
                // 把找到的地址填入输入框
                self.input.update(cx, |state, cx| {
                    state.set_value(first.addr.clone(), window, cx)
                });
                // 扫描应答经过了加密握手校验，可以直接信任并启用监控
                self.apply_server(first.addr.clone());
                self.hint = Some((
                    format!(
                        "发现 {} 台设备：{}（{}），已选择并开始监控。",
                        devices.len(),
                        first.name,
                        first.addr
                    ),
                    false,
                ));
            }
            Err(err) => {
                crate::log_warn!("扫描失败: {err}");
                self.hint = Some((format!("扫描失败: {err}"), true));
            }
        }
        cx.notify();
    }

    /// 切换监控目标（仅内存生效，不落盘）
    fn apply_server(&mut self, addr: String) {
        crate::log_info!("监控目标设置为 {addr}（等待首次连通）");
        self.saved_server = Some(addr.clone());
        self.status = HeartStatus::Waiting;
        self.lost_detail = None;
        let _ = self
            .cmds
            .unbounded_send(MonitorCommand::SetTarget(Some(addr)));
    }

    /// 用户在倒计时弹窗上做出了选择
    fn on_countdown_choice(&mut self, choice: CountdownChoice, cx: &mut Context<Self>) {
        match choice {
            CountdownChoice::Cancel => {
                crate::log_info!("用户在倒计时弹窗上选择：取消关机（心跳恢复前不再提醒）");
                // 取消关机：本次彻底不提醒了。心跳监控线程在“恢复 -> 再次丢失”
                // 之前不会重复上报丢失事件，因此这里只需关窗，无需定时重弹；
                // 下次心跳恢复（HeartbeatOk）后自动复位，重新进入失联监测。
                self.close_countdown(cx);
                self.hint = Some((
                    format!(
                        "已取消本次关机。心跳恢复后会重新开始监测，若再次失联超过 {} 秒将再次提醒。",
                        self.settings.heartbeat_timeout_seconds
                    ),
                    false,
                ));
            }
            CountdownChoice::Later => {
                crate::log_info!(
                    "用户在倒计时弹窗上选择：稍后关机（{SNOOZE_LATER_SECONDS} 秒后重弹）"
                );
                // 稍后关机：关闭弹窗并安排 30 秒后重新弹出完整倒计时
                self.close_countdown(cx);
                self.hint = Some((
                    format!("将在 {SNOOZE_LATER_SECONDS} 秒后再次弹出关机倒计时。"),
                    false,
                ));
                self.snooze_until =
                    Some(Instant::now() + Duration::from_secs(SNOOZE_LATER_SECONDS));
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
            && Instant::now() < until
        {
            return;
        }
        self.snooze_until = None;
        let handle = countdown::open_countdown_window(self.events.clone(), self.settings, cx);
        self.countdown = Some(handle);
        crate::log_warn!(
            "已弹出关机倒计时窗口（{} 秒）",
            self.settings.countdown_seconds
        );
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
        crate::log_error!("触发系统关机（shutdown /s /t 0）");
        let result = std::process::Command::new("shutdown")
            .args(["/s", "/t", "0", "/c", "自动关机守护：心跳失联自动关机"])
            .spawn();
        if let Err(e) = result {
            crate::log_error!("关机命令执行失败: {e}");
            self.hint = Some((format!("关机命令执行失败: {e}"), true));
            cx.notify();
        }
    }

    // -----------------------------------------------------------------
    // 界面按钮回调
    // -----------------------------------------------------------------

    fn save_settings(&mut self, cx: &mut Context<Self>) {
        // 已打开的倒计时使用打开时的配置，期间不允许改策略。
        if self.countdown.is_some() {
            return;
        }
        let timeout = positive_seconds(&self.timeout_input.read(cx).value());
        let countdown = positive_seconds(&self.countdown_input.read(cx).value());
        let (Some(timeout), Some(countdown)) = (timeout, countdown) else {
            self.settings_hint = Some(("请输入大于 0 的整数（最大 4294967295 秒）。".into(), true));
            cx.notify();
            return;
        };
        let settings = Settings {
            heartbeat_timeout_seconds: timeout,
            countdown_seconds: countdown,
        };
        if settings == self.settings {
            self.settings_hint = Some(("配置未修改。".into(), false));
            cx.notify();
            return;
        }
        match settings.save() {
            Ok(()) => {
                self.settings = settings;
                let _ = self
                    .cmds
                    .unbounded_send(MonitorCommand::SetTimeout(timeout));
                crate::log_info!("已保存关机策略：失联 {timeout} 秒，倒计时 {countdown} 秒");
                if self.status == HeartStatus::Unconfirmed {
                    self.hint = Some((
                        format!(
                            "未收到最新认证应答，正在确认设备状态；持续失联 {timeout} 秒后才会启动关机倒计时。"
                        ),
                        false,
                    ));
                }
                self.settings_hint = Some(("配置已保存并生效。".into(), false));
            }
            Err(error) => {
                crate::log_error!("保存关机策略失败: {error}");
                self.settings_hint = Some((format!("保存失败，继续使用原配置：{error}"), true));
            }
        }
        cx.notify();
    }

    /// “扫描开发板”按钮
    fn start_scan(&mut self, cx: &mut Context<Self>) {
        self.scanning = true;
        self.hint = Some(("正在扫描并认证开发板无线广播，请稍候…".into(), false));
        let _ = self.cmds.unbounded_send(MonitorCommand::Scan);
        cx.notify();
    }

    /// “测试”按钮：对输入框中的地址做一次完整的心跳握手
    fn start_test(&mut self, cx: &mut Context<Self>) {
        let addr = self.input.read(cx).value().trim().to_string();
        if addr.is_empty() {
            self.test_result = Some(Err("请先输入广播设备标识".into()));
            cx.notify();
            return;
        }
        self.test_result = Some(Err("测试中…".into()));
        let _ = self.cmds.unbounded_send(MonitorCommand::Test(addr));
        cx.notify();
    }

    /// “启动监控”按钮：切换本次运行的监控目标
    fn start_monitoring(&mut self, cx: &mut Context<Self>) {
        let addr = self.input.read(cx).value().trim().to_string();
        if addr.is_empty() {
            self.hint = Some(("请先输入广播设备标识".into(), true));
            cx.notify();
            return;
        }
        if let Err(error) = crate::radio_protocol::target_id(&addr) {
            self.hint = Some((error, true));
            cx.notify();
            return;
        }
        self.apply_server(addr);
        self.test_result = None;
        self.hint = Some((
            "已开始广播监控（设备标识仅本次运行生效，重启后重新扫描）。首次认证成功前不会触发关机。".into(),
            false,
        ));
        cx.notify();
    }

    /// 切换开机启动（任务计划程序）。失败时保持开关原状并给出提示。
    fn toggle_autostart(&mut self, checked: bool, cx: &mut Context<Self>) {
        match crate::autostart::set_enabled(checked) {
            Ok(()) => {
                crate::log_info!("开机启动已设置为: {checked}");
                self.autostart = checked;
                // 托盘菜单的 √ 同步
                crate::tray::sync_autostart_checked(checked);
            }
            Err(err) => {
                crate::log_warn!("设置开机启动失败: {err}");
                self.hint = Some((format!("设置开机启动失败: {err}"), true));
            }
        }
        cx.notify();
    }
}

impl Render for AppModel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl gpui_kit::IntoElement {
        let theme = cx.theme();
        let (status_text, status_color, status_icon) = match self.status {
            HeartStatus::NotConfigured => {
                ("尚未选择监控设备", theme.muted_foreground, IconName::Info)
            }
            HeartStatus::Waiting => ("等待首次有效心跳", theme.warning, IconName::BatteryCharging),
            HeartStatus::Connected => ("服务在线，心跳正常", theme.success, IconName::CircleCheck),
            HeartStatus::Unconfirmed => (
                "未收到认证应答，正在确认设备状态",
                theme.warning,
                IconName::Info,
            ),
            HeartStatus::Lost => (
                "心跳丢失！服务可能已离线",
                theme.danger,
                IconName::TriangleAlert,
            ),
        };
        let countdown_active = self.countdown.is_some();
        let hint_el = self.hint.as_ref().map(|(text, is_err)| {
            if *is_err {
                Alert::error("hint", text.clone())
            } else {
                Alert::info("hint", text.clone())
            }
            .small()
        });
        let test_el = self.test_result.as_ref().map(|result| match result {
            Ok(message) => Alert::success("test-result", message.clone()).small(),
            Err(message) => Alert::error("test-result", message.clone()).small(),
        });
        let lost_el = self
            .lost_detail
            .as_ref()
            .map(|detail| Alert::error("lost-detail", format!("丢失原因: {detail}")).small());
        let settings_el = self.settings_hint.as_ref().map(|(message, error)| {
            if *error {
                Alert::error("settings-hint", message.clone())
            } else {
                Alert::info("settings-hint", message.clone())
            }
            .small()
        });

        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(TitleBar::new().child(Label::new("自动关机守护").ml_3().text_size(px(13.))))
            .child(
                v_flex()
                    .flex_1()
                    .p_4()
                    .gap_4()
                    .overflow_y_scrollbar()
                    .child(
                        GroupBox::new().id("monitor-target").outline().child(
                            h_flex().justify_between().items_center()
                                .child(v_flex().gap_1()
                                    .child(Label::new("监控目标").text_color(theme.muted_foreground).text_size(px(12.)))
                                    .child(Label::new(self.saved_server.as_deref().unwrap_or("尚未选择监控设备").to_string())
                                        .text_size(px(18.)).font_weight(FontWeight::BOLD)))
                                .child(Button::new("status").ghost().icon(Icon::new(status_icon).text_color(status_color)).tooltip(status_text))
                        )
                    )
                    .when_some(lost_el, |element, alert| element.child(alert))
                    .child(
                        GroupBox::new().id("autostart-group").outline().child(
                            h_flex().w_full().justify_between().items_center().gap_3()
                                .child(v_flex().flex_1().min_w_0().gap_1()
                                    .child(Label::new("开机启动"))
                                    .child(Label::new("登录 Windows 后自动在后台运行（写入任务计划程序，无需管理员权限）")
                                        .text_color(theme.muted_foreground).text_size(px(12.))))
                                .child(Switch::new("autostart-switch").checked(self.autostart)
                                    .on_click(cx.listener(|this, checked: &bool, _, cx| this.toggle_autostart(*checked, cx))))
                        )
                    )
                    .child(
                        Collapsible::new().open(self.config_open)
                            .child(Button::new("config-trigger").outline().w_full().h(px(44.)).px_4()
                                .accessibility_label("配置")
                                .child(h_flex().w_full().justify_between().items_center()
                                    .child(Label::new("配置"))
                                    .child(Icon::new(if self.config_open { IconName::ChevronUp } else { IconName::ChevronDown })
                                        .text_color(theme.muted_foreground)))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.config_open = !this.config_open;
                                    cx.notify();
                                })))
                            .content(
                                v_flex().gap_4().pt_3()
                                    .child(
                                        GroupBox::new().id("device-config").title("设备配置").outline().child(
                                            Form::vertical().small()
                                                .child(Field::new().label_fn(|_, cx| {
                                                    h_flex().items_center().gap_1()
                                                        .child(Label::new("广播设备标识"))
                                                        .child(Button::new("device-address-help").ghost().xsmall()
                                                            .icon(Icon::new(gpui_kit::assets::IconName::CircleQuestionMark)
                                                                .text_color(cx.theme().muted_foreground))
                                                            .accessibility_label("广播设备标识说明")
                                                            .tooltip("地址不会保存，重启后重新扫描，避免继续监控旧设备。"))
                                                })
                                                    .child(Input::new(&self.input).small()))
                                                .footer(h_flex().gap_2()
                                                    .child(Button::new("scan").small()
                                                        .label(if self.scanning { "扫描中…" } else { "扫描开发板" })
                                                        .disabled(self.scanning)
                                                        .on_click(cx.listener(|this, _, _, cx| this.start_scan(cx))))
                                                    .child(Button::new("test").small().label("测试")
                                                        .on_click(cx.listener(|this, _, _, cx| this.start_test(cx))))
                                                    .child(Button::new("start-monitoring").small().label("启动监控")
                                                        .on_click(cx.listener(|this, _, _, cx| this.start_monitoring(cx)))))
                                        )
                                    )
                                    .when_some(test_el, |element, alert| element.child(alert))
                                    .child(
                                        GroupBox::new().id("shutdown-settings").title("关机策略").outline().child(
                                            Form::horizontal().small()
                                                .child(Field::new().label("失联超时（秒）")
                                                    .child(Input::new(&self.timeout_input).small().disabled(countdown_active)))
                                                .child(Field::new().label("关机倒计时（秒）")
                                                    .child(Input::new(&self.countdown_input).small().disabled(countdown_active)))
                                        ).footer(if countdown_active {
                                            "倒计时进行中，暂不可修改关机策略。"
                                        } else {
                                            "请输入大于 0 的整数，点击保存后生效。"
                                        })
                                    )
                                    .when_some(settings_el, |element, alert| element.child(alert))
                                    .child(h_flex().justify_end()
                                        .child(Button::new("save-settings").small().primary().label("保存")
                                            .disabled(countdown_active)
                                            .on_click(cx.listener(|this, _, _, cx| this.save_settings(cx)))))
                            )
                    )
                    .when_some(hint_el, |element, alert| element.child(alert))
                    .child(
                        GroupBox::new().id("instructions").title("使用说明").child(
                            v_flex().gap_1()
                                .child(Label::new("1. 开发板插在普通市电上，电脑由 UPS 供电；开发板广播无需连接路由器；")
                                    .text_color(theme.muted_foreground).text_size(px(12.)))
                                .child(Label::new("2. 电脑周期扫描无线广播，通过随机挑战认证在线状态，不切换当前 Wi-Fi；")
                                    .text_color(theme.muted_foreground).text_size(px(12.)))
                                .child(Label::new(format!("3. 如果连续 {} 秒没有有效应答，按失联策略弹出 {} 秒关机倒计时；", self.settings.heartbeat_timeout_seconds, self.settings.countdown_seconds))
                                    .text_color(theme.muted_foreground).text_size(px(12.)))
                                .child(Label::new("4. 倒计时无人干预会自动关机；也可选择立即关机 / 取消 / 稍后（30 秒后再次提醒）。")
                                    .text_color(theme.muted_foreground).text_size(px(12.)))
                                .child(Separator::horizontal())
                                .child(Label::new("提示: 关闭或最小化窗口都会收进托盘；右键托盘图标可显示窗口或退出。")
                                    .text_color(theme.muted_foreground).text_size(px(12.)))
                        )
                    )
            )
    }
}
