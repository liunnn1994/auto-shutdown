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
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme, Disableable as _, Icon, IconName, Root, Sizable as _, TitleBar};
use gpui_kit::{AppContext as _, Context, Entity, FontWeight, ParentElement as _, Render, Styled as _, Window, div, px};
use gpui_kit::prelude::FluentBuilder as _;
use serde::{Deserialize, Serialize};

use crate::countdown;
use crate::events::{AppEvent, CountdownChoice, EventTx, HeartStatus, MonitorCommand};
use crate::protocol::{SNOOZE_CANCEL_SECONDS, SNOOZE_LATER_SECONDS, WS_PORT};
use crate::win32;

/// 主窗口标题（同时用于 Win32 按标题查找窗口，必须全局唯一）
pub const MAIN_WINDOW_TITLE: &str = "自动关机守护 - 心跳失联自动关机";

// ---------------------------------------------------------------------------
// 配置持久化（%APPDATA%\auto-shutdown\config.json）
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Default, Debug)]
struct Config {
    /// 心跳服务地址（"ip" 或 "ip:port"）
    server: Option<String>,
}

/// 配置文件路径；无法确定时返回 None（配置功能静默失效，不影响监控）
fn config_path() -> Option<std::path::PathBuf> {
    std::env::var("APPDATA")
        .ok()
        .map(|base| std::path::PathBuf::from(base).join("auto-shutdown").join("config.json"))
}

fn load_config() -> Config {
    let Some(path) = config_path() else {
        return Config::default();
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn save_config(config: &Config) {
    let Some(path) = config_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string_pretty(config) {
        let _ = std::fs::write(path, text);
    }
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
            false
        });
        // 拦截最小化按钮：同样是隐藏到托盘（Win32 子类化实现）
        win32::hook_minimize_to_tray(MAIN_WINDOW_TITLE);

        // 读取配置：已配置则直接开始监控；首次运行则自动扫描局域网
        let config = load_config();
        let mut status = HeartStatus::NotConfigured;
        let mut scanning = false;
        let mut hint = None;

        match &config.server {
            Some(server) => {
                status = HeartStatus::Waiting;
                let _ = cmds.unbounded_send(MonitorCommand::SetTarget(Some(server.clone())));
            }
            None => {
                // 第一次打开软件：自动扫描局域网内的心跳服务
                scanning = true;
                hint = Some((
                    "正在扫描局域网（UDP 广播，发现端口 8124），请稍候…".to_string(),
                    false,
                ));
                let _ = cmds.unbounded_send(MonitorCommand::Scan);
            }
        }

        Self {
            status,
            lost_detail: None,
            input: cx.new(|cx| {
                InputState::new(window, cx).placeholder(format!("例如: 192.168.1.100（默认端口 {WS_PORT}）"))
            }),
            saved_server: config.server.clone(),
            scanning,
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
            AppEvent::ShutdownNow => self.shutdown(cx),
            AppEvent::CountdownAction(choice) => self.on_countdown_choice(choice, cx),
        }
    }

    /// 心跳恢复：更新状态、关闭倒计时弹窗、清除冷却期
    fn on_heartbeat_ok(&mut self, cx: &mut Context<Self>) {
        if self.status != HeartStatus::Connected {
            self.status = HeartStatus::Connected;
            self.lost_detail = None;
            self.hint = Some(("服务在线，心跳正常。".into(), false));
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

    /// 保存配置并切换监控目标（供扫描自动连接与“保存”按钮复用）
    fn apply_server(&mut self, addr: String) {
        save_config(&Config {
            server: Some(addr.clone()),
        });
        self.saved_server = Some(addr.clone());
        self.status = HeartStatus::Waiting;
        self.lost_detail = None;
        let _ = self.cmds.unbounded_send(MonitorCommand::SetTarget(Some(addr)));
    }

    /// 用户在倒计时弹窗上做出了选择
    fn on_countdown_choice(&mut self, choice: CountdownChoice, cx: &mut Context<Self>) {
        let snooze_secs = match choice {
            CountdownChoice::Cancel => {
                self.hint = Some((
                    format!("已取消本次关机。若服务仍未恢复，约 {SNOOZE_CANCEL_SECONDS} 秒后会再次提醒。"),
                    false,
                ));
                SNOOZE_CANCEL_SECONDS
            }
            CountdownChoice::Later => {
                self.hint = Some((
                    format!("将在 {SNOOZE_LATER_SECONDS} 秒后再次弹出关机倒计时。"),
                    false,
                ));
                SNOOZE_LATER_SECONDS
            }
        };
        self.close_countdown(cx);
        // 设置冷却期，并安排一次“重弹”检查
        self.snooze_until = Some(Instant::now() + Duration::from_secs(snooze_secs));
        self.snooze_gen += 1;
        let snooze_gen = self.snooze_gen;
        cx.spawn(async move |weak, cx| {
            cx.background_executor()
                .timer(Duration::from_secs(snooze_secs))
                .await;
            // 冷却期到点：如果期间没有新的选择（代数没变）且服务仍未恢复，重新弹窗
            let _ = weak.update(cx, |this, cx| {
                if this.snooze_gen == snooze_gen {
                    this.try_open_countdown(cx);
                }
            });
        })
        .detach();
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
            "已保存配置并开始监控。首次连通前不会触发关机（防误报）。".into(),
            false,
        ));
        cx.notify();
    }
}

impl Render for AppModel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl gpui_kit::IntoElement {
        let theme = cx.theme();

        // ---- 状态行 ----
        let (status_text, status_color, status_icon) = match self.status {
            HeartStatus::NotConfigured => ("尚未配置服务地址", theme.muted_foreground, IconName::Info),
            HeartStatus::Waiting => ("等待首次连通心跳服务…", theme.warning, IconName::BatteryCharging),
            HeartStatus::Connected => ("服务在线，心跳正常", theme.success, IconName::CircleCheck),
            HeartStatus::Lost => ("心跳丢失！服务可能已离线", theme.danger, IconName::TriangleAlert),
        };

        // ---- 提示信息 ----
        let hint_el = self.hint.as_ref().map(|(text, is_err)| {
            div()
                .text_size(px(13.))
                .text_color(if *is_err { theme.danger } else { theme.muted_foreground })
                .child(text.clone())
        });

        // ---- 测试结果 ----
        let test_el = self.test_result.as_ref().map(|r| {
            let (text, ok) = match r {
                Ok(msg) => (msg.clone(), true),
                Err(msg) => (msg.clone(), false),
            };
            div()
                .text_size(px(13.))
                .text_color(if ok { theme.success } else { theme.danger })
                .child(format!("测试结果: {text}"))
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
                    .p_5()
                    .gap_4()
                    // ---- 状态卡片 ----
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(Icon::new(status_icon).text_color(status_color))
                            .child(
                                div()
                                    .text_size(px(15.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(status_color)
                                    .child(status_text),
                            ),
                    )
                    .when_some(self.lost_detail.clone(), |el, d| {
                        el.child(
                            div()
                                .text_size(px(12.))
                                .text_color(theme.muted_foreground)
                                .child(format!("丢失原因: {d}")),
                        )
                    })
                    // ---- 服务地址配置 ----
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(div().text_size(px(13.)).child("服务地址:"))
                            .child(div().w(px(240.)).child(Input::new(&self.input).small()))
                            .child(
                                Button::new("scan")
                                    .label(if self.scanning { "扫描中…" } else { "扫描局域网" })
                                    .disabled(self.scanning)
                                    .on_click(cx.listener(|this, _, _, cx| this.start_scan(cx))),
                            )
                            .child(
                                Button::new("test")
                                    .label("测试")
                                    .on_click(cx.listener(|this, _, _, cx| this.start_test(cx))),
                            )
                            .child(
                                Button::new("save")
                                    .primary()
                                    .label("保存并启动监控")
                                    .on_click(cx.listener(|this, _, _, cx| this.save_and_start(cx))),
                            ),
                    )
                    .when_some(hint_el, |el, h| el.child(h))
                    .when_some(test_el, |el, t| el.child(t))
                    // ---- 使用说明 ----
                    .child(
                        v_flex()
                            .mt_2()
                            .gap_1()
                            .p_3()
                            .rounded_md()
                            .bg(theme.secondary)
                            .text_size(px(12.))
                            .text_color(theme.muted_foreground)
                            .child("工作原理：")
                            .child("1. 一台常驻的 WebSocket 心跳服务端（默认端口 8123），任何 socket 服务均可；")
                            .child("2. 本软件每 3 秒向它发送一次加密心跳，只要能收到应答，就说明服务在线；")
                            .child("3. 如果连续 60 秒收不到任何应答，判定服务离线，弹出置顶的 60 秒关机倒计时；")
                            .child("4. 倒计时无人干预会自动关机；也可选择立即关机 / 取消 / 稍后（30 秒后再次提醒）。")
                            .child(format!(
                                "当前监控目标: {}",
                                self.saved_server.as_deref().unwrap_or("未配置")
                            ))
                            .child("提示: 关闭或最小化窗口都会收到托盘；右键托盘图标可显示窗口或退出。"),
                    ),
            )
    }
}
