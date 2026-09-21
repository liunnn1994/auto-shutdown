//! # 通信协议定义
//!
//! 本模块定义 PC 客户端与心跳服务端之间的所有常量与报文格式。
//! 服务端可以用任何 socket 方案实现（Arduino / MicroPython / 任意后端），
//! 需要实现与本文件完全一致的协议。
//!
//! ## 协议概览
//!
//! - **心跳**: PC 作为 WebSocket 客户端，每隔 [`HEARTBEAT_INTERVAL`] 向服务端的
//!   `ws://<ip>:8123/ws` 发送一条加密的 `ping` 报文；服务端立即回复 `pong`。
//!   如果连续 [`HEARTBEAT_TIMEOUT`]（默认 60s）内没有任何一次成功的 ping/pong，
//!   就认为服务端已经离线（典型部署：服务端插在市电上，离线即市电中断），
//!   触发关机倒计时。
//! - **发现**: PC 向局域网广播 UDP 地址 `255.255.255.255:8124` 发送加密的
//!   `discover` 报文；服务端收到后向来源地址单播回复加密的 `announce` 报文。
//!
//! ## 加密帧格式（WebSocket 文本帧 / UDP 数据报，均为 Base64 文本）
//!
//! ```text
//! Base64( HMAC-SHA256(k_mac, IV || CT)[32B] || IV[16B] || CT )
//! ```
//!
//! - `IV`: 每条报文随机生成的 16 字节初始向量；
//! - `CT`: AES-256-CTR(k_enc, IV, 明文 JSON)；
//! - `k_enc` / `k_mac`: 由预设口令（[`PASSPHRASE`]，两端必须一致）派生的
//!   32 字节密钥（SHA-256），分别用于加密和校验。
//!
//! HMAC 先于解密校验（encrypt-then-MAC），任何第三方在不知道口令的情况下
//! 既无法伪造报文，也无法读取/篡改内容，从而避免局域网内其他设备的干扰。
//!
//! 选择 AES-CTR + HMAC-SHA256 是因为 mbedtls（ESP32 等 MCU）与 Python cryptography 都原生支持，
//! Python 侧的 `cryptography` 库也有现成实现，两端移植都非常简单。

// 报文结构体在此项目中主要作为协议文档存在（实际收发用动态 JSON 处理，
// 以便与服务端实现保持最大兼容性）。
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

/// WebSocket 服务端口（服务端监听）
pub const WS_PORT: u16 = 8123;
/// WebSocket 路径
pub const WS_PATH: &str = "/ws";
/// UDP 发现服务端口（服务端监听）
pub const DISCOVERY_PORT: u16 = 8124;

/// 心跳发送间隔（每次心跳都会重新建立 WebSocket 连接，LAN 内开销可忽略）
pub const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);
/// 心跳超时时间：连续这么长时间没有任何成功的心跳，就认为服务已失联
pub const HEARTBEAT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// 关机倒计时时长（秒）：倒计时结束仍无人响应则自动关机
pub const COUNTDOWN_SECONDS: u64 = 60;
/// “稍后关机”的重新弹出间隔（秒）
pub const SNOOZE_LATER_SECONDS: u64 = 30;

/// 预共享口令。两端（PC 与服务端）必须配置完全一致的内容。
/// 如需更换，请同步修改 python/protocol.py 与服务端代码。
pub const PASSPHRASE: &str = "auto-shutdown-v1";

// ---------------------------------------------------------------------------
// 报文结构（明文 JSON）
// ---------------------------------------------------------------------------

/// 心跳请求：PC -> 服务端
#[derive(Serialize, Deserialize, Debug)]
pub struct Ping {
    /// 报文类型: "ping"
    #[serde(rename = "type")]
    pub typ: String,
    /// 当前时间戳（毫秒），服务端可忽略
    pub ts: u64,
    /// 随机数，服务端需要在 pong 中原样带回
    pub nonce: String,
}

/// 心跳响应：服务端 -> PC
#[derive(Serialize, Deserialize, Debug)]
pub struct Pong {
    /// 报文类型: "pong"
    #[serde(rename = "type")]
    pub typ: String,
    /// 原样返回 ping 中的 nonce
    pub nonce: String,
    /// 服务端已运行的秒数（可选，用于界面展示）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uptime_s: Option<u64>,
    /// 设备身份（ESP 用芯片 ID 的十六进制）。PC 端用它在一台设备
    /// 换了 IP 之后仍能认出是同一台，避免误把别的设备当成目标
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// 发现请求：PC 广播
#[derive(Serialize, Deserialize, Debug)]
pub struct Discover {
    /// 报文类型: "discover"
    #[serde(rename = "type")]
    pub typ: String,
    /// 随机数
    pub nonce: String,
}

/// 发现响应：服务端单播回复
#[derive(Serialize, Deserialize, Debug)]
pub struct Announce {
    /// 报文类型: "announce"
    #[serde(rename = "type")]
    pub typ: String,
    /// 设备名称（展示用）
    pub name: String,
    /// 设备身份（与 pong 中的 id 一致）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// WebSocket 服务端口
    pub ws_port: u16,
    /// 服务端已运行的秒数（可选）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uptime_s: Option<u64>,
}
