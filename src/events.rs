//! # 事件与命令定义
//!
//! - [`AppEvent`]: 后台线程（心跳监控 / 托盘菜单 / 倒计时窗口）发给 UI 的**事件**；
//! - [`MonitorCommand`]: UI 发给心跳监控线程的**命令**。

use futures::channel::mpsc::UnboundedSender;
use serde::{Deserialize, Serialize};

/// 心跳的整体状态，用于主界面展示
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeartStatus {
    /// 尚未配置服务地址
    NotConfigured,
    /// 已配置地址，但从未连通过（首次连通后才会进入“戒备”状态）
    Waiting,
    /// 心跳服务在线
    Connected,
    /// 心跳丢失，服务可能已离线
    Lost,
}

/// 局域网内发现的心跳服务设备
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Device {
    /// 设备名称（来自 announce 报文）
    pub name: String,
    /// WebSocket 地址（"ip:port" 形式，不含 scheme）
    pub addr: String,
}

/// 倒计时弹窗上的用户选择
#[derive(Clone, Copy, Debug)]
pub enum CountdownChoice {
    /// 取消关机（60 秒后若心跳仍未恢复，再次弹出）
    Cancel,
    /// 稍后关机（30 秒后再次弹出倒计时）
    Later,
}

/// 发往 UI 主循环的事件
#[derive(Debug)]
pub enum AppEvent {
    /// 心跳恢复（收到一次成功的 pong）
    HeartbeatOk,
    /// 心跳丢失超过阈值，服务可能已离线
    HeartbeatLost { detail: String },
    /// 局域网扫描完成
    ScanFinished(Result<Vec<Device>, String>),
    /// 手动“测试”完成
    TestFinished(Result<String, String>),
    /// 托盘菜单：显示主窗口
    TrayShow,
    /// 托盘菜单：退出程序
    Quit,
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
    /// 扫描局域网内的心跳服务
    Scan,
    /// 手动测试某个地址的连通性
    Test(String),
}

/// 事件发送端的别名，方便在各模块间传递
pub type EventTx = UnboundedSender<AppEvent>;
