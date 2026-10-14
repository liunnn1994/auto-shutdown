//! # 事件与命令定义
//!
//! - [`AppEvent`]: 后台线程（心跳监控 / 托盘菜单 / 倒计时窗口）发给 UI 的**事件**；
//! - [`MonitorCommand`]: UI 发给心跳监控线程的**命令**。

use futures::channel::mpsc::UnboundedSender;

/// 心跳的整体状态，用于主界面展示
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeartStatus {
    /// 尚未选择广播设备
    NotConfigured,
    /// 已选择设备，但从未认证成功（成功后才进入戒备状态）
    Waiting,
    /// 心跳服务在线
    Connected,
    /// 首次认证失败，尚未达到关机阈值
    Unconfirmed,
    /// 心跳丢失，服务可能已离线
    Lost,
}

/// 通过无线广播认证发现的设备
#[derive(Clone, Debug)]
pub struct Device {
    /// 设备名称和信号强度
    pub name: String,
    /// 广播设备标识（radio://芯片ID）
    pub addr: String,
}

/// 倒计时弹窗上的用户选择
#[derive(Clone, Copy, Debug)]
pub enum CountdownChoice {
    /// 取消本次关机，认证恢复前不再提醒
    Cancel,
    /// 稍后关机（30 秒后再次弹出倒计时）
    Later,
}

/// 发往 UI 主循环的事件
#[derive(Debug)]
pub enum AppEvent {
    /// 收到本轮挑战的有效认证应答
    HeartbeatOk,
    /// 最新一轮认证失败，但尚未达到失联关机阈值
    HeartbeatUnconfirmed { detail: String },
    /// 超过阈值未收到认证应答，按失联策略处理
    HeartbeatLost { detail: String },
    /// 无线广播扫描完成
    ScanFinished(Result<Vec<Device>, String>),
    /// 手动“测试”完成
    TestFinished(Result<String, String>),
    /// 托盘菜单：显示主窗口
    TrayShow,
    /// 托盘菜单：退出程序
    Quit,
    /// 开机启动已切换（托盘菜单切换后通知主界面同步开关状态）
    AutostartChanged(bool),
    /// 主窗口已隐藏到托盘（最小化或关闭按钮触发），UI 可借此清理临时状态
    WindowHidden,
    /// 倒计时结束 / 用户点击“立即关机”
    ShutdownNow,
    /// 用户在倒计时弹窗上做出的选择
    CountdownAction(CountdownChoice),
}

/// 发往心跳监控线程的命令
#[derive(Debug)]
pub enum MonitorCommand {
    /// 设置监控目标；None 表示停止监控（例如用户尚未配置）
    SetTarget(Option<String>),
    /// 扫描并认证开发板广播
    Scan,
    /// 手动测试某个广播设备的认证应答
    Test(String),
}

/// 事件发送端的别名，方便在各模块间传递
pub type EventTx = UnboundedSender<AppEvent>;
