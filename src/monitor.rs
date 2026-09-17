//! # 心跳监控线程
//!
//! 独立线程内运行一个 tokio runtime，负责：
//!
//! 1. **心跳**：按 [`protocol::HEARTBEAT_INTERVAL`] 周期向心跳服务端发送加密 ping，
//!    等待 pong。如果“最后一次成功心跳”距今超过 [`protocol::HEARTBEAT_TIMEOUT`]，
//!    即认为服务端已失联，通过事件通道发出 [`AppEvent::HeartbeatLost`]。
//! 2. **发现**：收到 [`MonitorCommand::Scan`] 后 UDP 广播 discover，
//!    收集 3 秒内的 announce 应答。
//! 3. **测试**：收到 [`MonitorCommand::Test`] 后对指定地址做一次完整的
//!    连接 + ping/pong，把成功信息或完整错误链返回给界面。
//!
//! ## “戒备”机制（重要）
//!
//! 只有**首次成功连通**之后才会开始判定丢失（armed 状态）。也就是说：
//! 如果用户配置了地址但服务端从未在线（例如尚未部署完成），
//! 软件不会因此触发关机；只有先确认过设备在线、随后心跳丢失超过 60 秒，
//! 才认定是服务失联。

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use futures::{SinkExt, StreamExt};
use serde_json::json;
use tokio::net::UdpSocket;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::crypto;
use crate::events::{AppEvent, Device, MonitorCommand};
use crate::protocol::{self, DISCOVERY_PORT, HEARTBEAT_INTERVAL, HEARTBEAT_TIMEOUT};

/// 单次心跳中，建立连接 / 等待 pong 的超时时间
const STEP_TIMEOUT: Duration = Duration::from_secs(4);
/// 局域网扫描的总时长
const SCAN_DURATION: Duration = Duration::from_secs(3);
/// 局域网扫描时每轮广播的间隔
const SCAN_BROADCAST_INTERVAL: Duration = Duration::from_millis(400);

/// 在独立线程中启动 tokio runtime 并运行监控循环。
///
/// 使用独立线程 + 自带 runtime 的原因：gpui 的主线程有自己的事件循环，
/// 心跳属于纯 IO 任务，放后台线程互不干扰；两线程之间只用无锁的
/// 无界 mpsc 通道通信（事件上行 / 命令下行）。
pub fn spawn(mut cmds: UnboundedReceiver<MonitorCommand>, events: UnboundedSender<AppEvent>) {
    std::thread::Builder::new()
        .name("heartbeat-monitor".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("无法创建 tokio runtime");
            rt.block_on(run(&mut cmds, &events));
        })
        .expect("无法创建心跳监控线程");
}

/// 把用户输入的地址规范化为 WebSocket URL。
///
/// 支持 "192.168.1.100"、"192.168.1.100:8123"、"ws://192.168.1.100:8123" 三种写法。
pub fn to_ws_url(addr: &str) -> String {
    let a = addr.trim().trim_end_matches('/');
    if a.starts_with("ws://") || a.starts_with("wss://") {
        format!("{a}{}", protocol::WS_PATH)
    } else if a.contains(':') {
        format!("ws://{a}{}", protocol::WS_PATH)
    } else {
        format!("ws://{a}:{}{}", protocol::WS_PORT, protocol::WS_PATH)
    }
}

async fn run(cmds: &mut UnboundedReceiver<MonitorCommand>, events: &UnboundedSender<AppEvent>) {
    // 当前监控目标（"ip:port"），None 表示未配置
    let mut target: Option<String> = None;
    // 最后一次成功心跳的时间
    let mut last_ok: Option<Instant> = None;
    // 是否已确认过设备在线（见模块注释“戒备机制”）
    let mut armed = false;
    // 本次丢失周期内是否已经上报过 HeartbeatLost（避免重复弹窗）
    let mut reported_lost = false;

    let mut tick = Box::pin(tokio::time::sleep(HEARTBEAT_INTERVAL));

    loop {
        tokio::select! {
            // 下行命令（UI -> 监控线程）；Err 表示通道已关闭（UI 已退出）
            cmd = cmds.recv() => match cmd {
                Ok(MonitorCommand::SetTarget(t)) => {
                    target = t;
                    // 换了目标，一切从零开始（重新等待首次连通）
                    last_ok = None;
                    armed = false;
                    reported_lost = false;
                }
                Ok(MonitorCommand::Scan) => {
                    let result = scan().await;
                    let _ = events.unbounded_send(AppEvent::ScanFinished(result));
                }
                Ok(MonitorCommand::Test(addr)) => {
                    let result = test(&addr).await;
                    let _ = events.unbounded_send(AppEvent::TestFinished(result));
                }
                Err(_) => return,
            },
            // 心跳节拍
            _ = &mut tick => {
                if let Some(addr) = target.clone() {
                    match heartbeat_once(&addr).await {
                        Ok(rtt) => {
                            last_ok = Some(Instant::now());
                            if !armed {
                                // 首次连通：进入戒备状态
                                armed = true;
                                reported_lost = false;
                                let _ = events.unbounded_send(AppEvent::HeartbeatOk);
                            } else if reported_lost {
                                // 心跳恢复（服务回来了）
                                reported_lost = false;
                                let _ = events.unbounded_send(AppEvent::HeartbeatOk);
                            }
                            tracing_rtt(rtt);
                        }
                        Err(err) => {
                            // 只有“确认过在线”且“距上次成功心跳超过阈值”才触发
                            if armed
                                && !reported_lost
                                && last_ok.is_some_and(|t| t.elapsed() >= HEARTBEAT_TIMEOUT)
                            {
                                reported_lost = true;
                                let _ = events.unbounded_send(AppEvent::HeartbeatLost { detail: err });
                            }
                        }
                    }
                }
                tick = Box::pin(tokio::time::sleep(HEARTBEAT_INTERVAL));
            }
        }
    }
}

/// 打印往返耗时（无正式日志框架，直接走 stderr 即可满足排查需求）
fn tracing_rtt(rtt: Duration) {
    eprintln!("[heartbeat] ok, rtt = {}ms", rtt.as_millis());
}

/// 当前 Unix 时间戳（毫秒）
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 生成 16 字符的十六进制随机数
fn random_nonce() -> String {
    use rand::Rng;
    let mut buf = [0u8; 8];
    rand::rng().fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// 执行一次完整心跳：建连 -> 发送加密 ping -> 等待匹配的 pong。
/// 成功返回往返耗时，失败返回错误描述。
async fn heartbeat_once(addr: &str) -> Result<Duration, String> {
    let started = Instant::now();
    let url = to_ws_url(addr);

    // 1. 建立 WebSocket 连接
    let request = url
        .clone()
        .into_client_request()
        .map_err(|e| format!("构造请求失败: {e}"))?;
    let (ws, _) = tokio::time::timeout(STEP_TIMEOUT, tokio_tungstenite::connect_async(request))
        .await
        .map_err(|_| "连接超时".to_string())?
        .map_err(|e| format!("连接失败 ({url}): {e}"))?;

    let (mut tx, mut rx) = ws.split();

    // 2. 发送加密 ping
    let nonce = random_nonce();
    let ping = json!({ "type": "ping", "ts": now_ms(), "nonce": nonce });
    tx.send(Message::text(crypto::seal_json(&ping)))
        .await
        .map_err(|e| format!("发送心跳失败: {e}"))?;

    // 3. 等待带相同 nonce 的 pong（跳过无关报文）
    loop {
        let msg = tokio::time::timeout(STEP_TIMEOUT, rx.next())
            .await
            .map_err(|_| "等待心跳应答超时".to_string())?;

        let Some(Ok(Message::Text(text))) = msg else {
            return Err(match msg {
                Some(Ok(other)) => format!("收到非文本报文: {other:?}"),
                Some(Err(e)) => format!("读取心跳应答失败: {e}"),
                None => "连接被对端关闭".to_string(),
            });
        };

        let pong = crypto::open_frame(&text).map_err(|e| format!("心跳应答解密失败: {e}"))?;
        if pong["type"] == "pong" && pong["nonce"] == nonce {
            return Ok(started.elapsed());
        }
        // 其它报文（例如设备主动推送的状态）直接忽略
    }
}

/// 手动测试：与心跳相同流程，但把成功/完整错误链返回给界面
async fn test(addr: &str) -> Result<String, String> {
    match heartbeat_once(addr).await {
        Ok(rtt) => Ok(format!(
            "连接成功！ping/pong 往返耗时 {} ms。",
            rtt.as_millis()
        )),
        Err(e) => Err(e),
    }
}

/// UDP 广播扫描局域网内的心跳服务端。
///
/// 广播 discover 报文到 255.255.255.255:8124，持有合法口令的设备会
/// 单播回复 announce；无法解密的杂音报文一律忽略。
async fn scan() -> Result<Vec<Device>, String> {
    let sock = UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|e| format!("绑定 UDP 端口失败: {e}"))?;
    sock.set_broadcast(true)
        .map_err(|e| format!("开启广播失败: {e}"))?;

    let discover = json!({ "type": "discover", "nonce": random_nonce() });
    let frame = crypto::seal_json(&discover);
    let deadline = Instant::now() + SCAN_DURATION;
    // 按来源 IP 去重
    let mut found: HashMap<String, Device> = HashMap::new();
    let mut buf = [0u8; 2048];

    while Instant::now() < deadline {
        let _ = sock
            .send_to(frame.as_bytes(), ("255.255.255.255", DISCOVERY_PORT))
            .await;

        // 在广播间隔窗口内收集应答
        while let Ok(res) = tokio::time::timeout(SCAN_BROADCAST_INTERVAL, sock.recv_from(&mut buf)).await {
            let (n, peer) = res.map_err(|e| format!("接收扫描应答失败: {e}"))?;
            let Ok(reply) = crypto::open_frame(std::str::from_utf8(&buf[..n]).map_err(|e| e.to_string())?)
            else {
                // 无法解密的报文：不是我们的设备，忽略
                continue;
            };
            if reply["type"] == "announce" {
                let name = reply["name"].as_str().unwrap_or("heartbeat-server").to_string();
                let ws_port = reply["ws_port"].as_u64().unwrap_or(protocol::WS_PORT as u64) as u16;
                let ip = peer.ip().to_string();
                found.insert(
                    ip.clone(),
                    Device {
                        name,
                        addr: format!("{ip}:{ws_port}"),
                    },
                );
            }
        }
    }

    Ok(found.into_values().collect())
}
