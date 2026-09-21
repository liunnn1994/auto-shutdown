//! # 心跳监控线程
//!
//! 独立线程内运行一个 tokio runtime，负责：
//!
//! 1. **心跳**：按 [`protocol::HEARTBEAT_INTERVAL`] 周期向心跳服务端发送加密 ping，
//!    等待 pong。如果“最后一次成功心跳”距今超过 [`protocol::HEARTBEAT_TIMEOUT`]，
//!    且主动探测也确认设备无应答，才认为服务端已失联，
//!    通过事件通道发出 [`AppEvent::HeartbeatLost`]。
//! 2. **失联验证（区分“物理离线”与“网络被动断连”）**：心跳失败只能说明
//!    WebSocket 这条路不通，可能是路由器/AP 抽风，也可能是设备换了 IP，
//!    并不一定代表设备断电。因此在判定失联前会主动用 UDP 发现广播探测：
//!    只要设备有电且连着网，就一定会应答 announce（这与 WS 是否可达无关）。
//!    - 探测有应答且地址未变 → 设备在线，是网络路径问题，不触发关机；
//!    - 探测有应答但地址变了（DHCP 重新分配）→ 自动切换监控目标并通知界面；
//!    - 探测无应答且距上次成功心跳超过 60 秒 → 判定设备物理离线。
//!
//!    失联上报后仍会继续周期探测，设备恢复后自动上报 [`AppEvent::HeartbeatOk`]。
//! 3. **发现**：收到 [`MonitorCommand::Scan`] 后 UDP 广播 discover，
//!    收集 3 秒内的 announce 应答。
//! 4. **测试**：收到 [`MonitorCommand::Test`] 后对指定地址做一次完整的
//!    连接 + ping/pong，把成功信息或完整错误链返回给界面。
//!
//! ## “戒备”机制（重要）
//!
//! 只有**首次成功连通**之后才会开始判定丢失（armed 状态）。也就是说：
//! 如果用户配置了地址但服务端从未在线（例如尚未部署完成），
//! 软件不会因此触发关机；只有先确认过设备在线、随后心跳丢失超过 60 秒
//! 且探测无应答，才认定是服务失联。
//!
//! ## 设备身份
//!
//! 服务端会在 pong / announce 中携带 `id`（ESP 用芯片 ID）。监控线程记录
//! 首次连通时的设备身份，此后探测发现的设备先按 `id` 认人、再按地址认人，
//! 即使 IP 被路由器重新分配也不会认错设备或误认别的设备。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use futures::{SinkExt, StreamExt};
use if_addrs::{IfAddr, Ifv4Addr};
use serde_json::json;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::crypto;
use crate::events::{AppEvent, Device, MonitorCommand};
use crate::protocol::{self, DISCOVERY_PORT, HEARTBEAT_INTERVAL, HEARTBEAT_TIMEOUT};

/// 单次心跳中，建立连接 / 等待 pong 的超时时间
const STEP_TIMEOUT: Duration = Duration::from_secs(4);
/// 心跳连续失败多久后开始主动探测（期间可能只是网络抖动，先观察）
const VERIFY_AFTER: Duration = Duration::from_secs(10);
/// 两次主动探测之间的最小间隔（每轮探测即一次完整的局域网扫描，约 3 秒）
const PROBE_INTERVAL: Duration = Duration::from_secs(12);
/// 探测确认过设备在线后，这份“在线凭证”的有效期。
/// 过期后若心跳仍未恢复、且新一轮探测也无应答，才允许判定失联。
const ALIVE_GRACE: Duration = Duration::from_secs(PROBE_INTERVAL.as_secs() * 2);
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
    // 连续失败次数（仅用于日志展示，方便和 ESP 端日志对照）
    let mut consec_fail: u32 = 0;
    // 设备身份（pong 中的 id）：首次连通时记录，之后 IP 变了也能认出同一台设备
    let mut device_id: Option<String> = None;
    // 上一次主动探测（局域网发现验证）的时刻
    let mut last_probe: Option<Instant> = None;
    // 最近一次探测确认设备仍在线的时刻（announce 应答 = 设备有电、在运行）
    let mut last_seen_alive: Option<Instant> = None;

    let mut tick = Box::pin(tokio::time::sleep(HEARTBEAT_INTERVAL));

    loop {
        tokio::select! {
            // 下行命令（UI -> 监控线程）；Err 表示通道已关闭（UI 已退出）
            cmd = cmds.recv() => match cmd {
                Ok(MonitorCommand::SetTarget(t)) => {
                    crate::log_info!("设置监控目标: {}", t.as_deref().unwrap_or("(停止监控)"));
                    target = t;
                    // 换了目标，一切从零开始（重新等待首次连通，并重新学习设备身份）
                    last_ok = None;
                    armed = false;
                    reported_lost = false;
                    consec_fail = 0;
                    device_id = None;
                    last_probe = None;
                    last_seen_alive = None;
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
                        Ok((rtt, id)) => {
                            last_ok = Some(Instant::now());
                            last_seen_alive = Some(Instant::now());
                            // 记录设备身份：此后即使设备换了 IP，探测也能认出它
                            if let Some(id) = id
                                && device_id.as_ref() != Some(&id)
                            {
                                crate::log_info!("设备身份 id={id}（target={addr}）");
                                device_id = Some(id);
                            }
                            // 每次心跳都留痕，形成完整时间线，可与 ESP 端日志逐条对照
                            crate::log_info!(
                                "心跳正常 rtt={}ms target={addr}{}",
                                rtt.as_millis(),
                                if consec_fail > 0 {
                                    format!("（此前连续失败 {consec_fail} 次，已恢复）")
                                } else {
                                    String::new()
                                }
                            );
                            consec_fail = 0;
                            if !armed {
                                // 首次连通：进入戒备状态
                                armed = true;
                                reported_lost = false;
                                crate::log_info!("首次连通，进入戒备状态（此后失联才会触发关机）");
                                let _ = events.unbounded_send(AppEvent::HeartbeatOk);
                            } else if reported_lost {
                                // 心跳恢复（服务回来了）
                                reported_lost = false;
                                crate::log_info!("服务恢复，上报 HeartbeatOk");
                                let _ = events.unbounded_send(AppEvent::HeartbeatOk);
                            }
                        }
                        Err(err) => {
                            consec_fail += 1;
                            crate::log_warn!("心跳失败（连续第 {consec_fail} 次）target={addr}: {err}");
                            if armed {
                                let since_ok = last_ok.map_or(Duration::MAX, |t| t.elapsed());
                                // 心跳失败不能区分“设备断电”和“网络路径问题”：
                                // 周期性发起 UDP 发现广播主动验证（设备只要有电就应答）。
                                let probe_due = last_probe.is_none_or(|t| t.elapsed() >= PROBE_INTERVAL);
                                if since_ok >= VERIFY_AFTER && probe_due {
                                    last_probe = Some(Instant::now());
                                    probe_adopt(
                                        &addr,
                                        &device_id,
                                        &mut target,
                                        &mut reported_lost,
                                        &mut last_seen_alive,
                                        events,
                                    )
                                    .await;
                                }
                                // 只有“确认过在线”、“距上次成功心跳超过阈值”、
                                // 且最近一次探测也没有发现设备（在线凭证已过期），
                                // 才判定设备物理离线并触发关机流程
                                let alive_recent =
                                    last_seen_alive.is_some_and(|t| t.elapsed() < ALIVE_GRACE);
                                if !reported_lost
                                    && since_ok >= HEARTBEAT_TIMEOUT
                                    && !alive_recent
                                {
                                    reported_lost = true;
                                    crate::log_error!(
                                        "判定设备物理离线（距上次成功心跳已超过 {} 秒，\
                                         期间主动探测未发现任何设备，最后一次心跳错误: {err}）",
                                        HEARTBEAT_TIMEOUT.as_secs()
                                    );
                                    let _ = events.unbounded_send(AppEvent::HeartbeatLost {
                                        detail: format!(
                                            "心跳中断超过 {} 秒，且局域网发现探测无应答，\
                                             判定设备物理离线（最后一次心跳错误: {err}）",
                                            HEARTBEAT_TIMEOUT.as_secs()
                                        ),
                                    });
                                }
                            }
                        }
                    }
                }
                tick = Box::pin(tokio::time::sleep(HEARTBEAT_INTERVAL));
            }
        }
    }
}

/// 主动探测 + 自动接管：广播 discover，确认设备是否仍在线。
///
/// - 在线且地址未变：仅确认存活（若此前已上报失联，则上报恢复）；
/// - 在线但地址变了（DHCP 重新分配）：自动切换监控目标并通知界面；
/// - 无应答：什么都不做（等待心跳阈值判定物理离线）。
///
/// 设备身份识别：已记录 `device_id` 时只认 id 匹配的设备（避免把别的
/// auto-shutdown 设备当成目标）；所有设备都不带 id（旧固件）时退化为按地址认。
async fn probe_adopt(
    addr: &str,
    device_id: &Option<String>,
    target: &mut Option<String>,
    reported_lost: &mut bool,
    last_seen_alive: &mut Option<Instant>,
    events: &UnboundedSender<AppEvent>,
) {
    let devices = match scan().await {
        Ok(d) => d,
        Err(e) => {
            crate::log_warn!("失联验证探测失败（按未发现处理）: {e}");
            return;
        }
    };

    let candidate = match device_id {
        Some(id) => devices.iter().find(|d| d.id.as_deref() == Some(id.as_str())),
        // 未学习到设备身份（首次连通前就失联，或对端是旧固件）：
        // 有 id 的设备一律不认，只在“无 id 设备”里按地址匹配（旧固件兼容）
        None => devices
            .iter()
            .filter(|d| d.id.is_none())
            .find(|d| d.addr == addr),
    };
    let Some(dev) = candidate else {
        if devices.is_empty() {
            crate::log_info!("失联验证探测：局域网内未发现任何设备");
        } else {
            crate::log_info!(
                "失联验证探测：发现 {} 台设备但均非监控目标（按 id/地址识别），视为未发现",
                devices.len()
            );
        }
        return;
    };

    *last_seen_alive = Some(Instant::now());
    if dev.addr == addr {
        crate::log_info!(
            "失联验证探测：设备 {}（{}）仍应答发现广播，判定设备在线，\
             疑似网络路径问题，暂不触发失联",
            dev.name,
            dev.addr
        );
        if *reported_lost {
            // 已弹过关机倒计时：设备确认活着，立即恢复，自动关掉倒计时
            *reported_lost = false;
            crate::log_info!("设备经探测确认恢复，上报 HeartbeatOk");
            let _ = events.unbounded_send(AppEvent::HeartbeatOk);
        }
    } else {
        // 设备活着，但 IP 变了（典型：路由器 DHCP 重新分配）。
        // 自动切换监控目标；身份 id 不变，继续沿用。
        crate::log_warn!(
            "失联验证探测：设备 {} 已改用新地址 {}（原 {addr}，IP 可能被重新分配），自动切换监控目标",
            dev.name,
            dev.addr
        );
        let old = addr.to_string();
        let new = dev.addr.clone();
        *target = Some(new.clone());
        let _ = events.unbounded_send(AppEvent::TargetAutoChanged { old, new });
        if *reported_lost {
            *reported_lost = false;
            let _ = events.unbounded_send(AppEvent::HeartbeatOk);
        }
    }
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
/// 成功返回 (往返耗时, 设备身份 id)；失败返回错误描述。
async fn heartbeat_once(addr: &str) -> Result<(Duration, Option<String>), String> {
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
        // 设备随心跳连接推送回来的运行日志：落到 esp-日期.log，继续等 pong
        if pong["type"] == "log" {
            crate::logger::write_esp(&format!(
                "uptime={}s [{}] {}",
                pong["up"],
                pong["lv"].as_str().unwrap_or("info"),
                pong["msg"].as_str().unwrap_or("")
            ));
            continue;
        }
        if pong["type"] == "pong" && pong["nonce"] == nonce {
            // 礼貌地发送 WebSocket Close 帧再断开，
            // 避免服务端把每次心跳都当作“异常断开”记录
            let _ = tx.close().await;
            let id = pong["id"].as_str().map(str::to_string);
            return Ok((started.elapsed(), id));
        }
        // 其它报文（例如设备主动推送的状态）直接忽略
    }
}

/// 手动测试：与心跳相同流程，但把成功/完整错误链返回给界面
async fn test(addr: &str) -> Result<String, String> {
    match heartbeat_once(addr).await {
        Ok((rtt, _)) => Ok(format!(
            "连接成功！ping/pong 往返耗时 {} ms。",
            rtt.as_millis()
        )),
        Err(e) => Err(e),
    }
}

/// UDP 扫描局域网内的心跳服务端（覆盖本机所有网卡，含虚拟网卡）。
///
/// 实现：枚举本机所有 IPv4 网卡（Windows 走 GetAdaptersAddresses，
/// Hyper-V / WSL / VMware / VPN / Tailscale 等虚拟网卡同样会被读到），
/// 为每块网卡单独开一个 socket **绑定到该网卡的 IP**，向该子网的
/// 定向广播地址发送 discover —— 这样每一条网卡所在网段都会被扫到，
/// 而不是像 255.255.255.255 那样只从默认路由网卡发出。
///
/// 持有合法口令的设备会单播回复 announce；无法解密的杂音报文一律忽略。
async fn scan() -> Result<Vec<Device>, String> {
    // 枚举网卡（同步系统调用，放阻塞线程执行）
    let ifaces = tokio::task::spawn_blocking(if_addrs::get_if_addrs)
        .await
        .map_err(|e| format!("枚举网卡任务失败: {e}"))?
        .map_err(|e| format!("枚举网卡失败: {e}"))?;

    let discover = json!({ "type": "discover", "nonce": random_nonce() });
    let frame = crypto::seal_json(&discover);
    let deadline = Instant::now() + SCAN_DURATION;

    // 应答汇总通道
    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel::<Device>();

    // 为每块网卡派生一个扫描任务
    let mut started = 0;
    for iface in &ifaces {
        let Ifv4Addr { ip, broadcast, .. } = match &iface.addr {
            IfAddr::V4(v4) => v4,
            IfAddr::V6(_) => continue, // 本协议只做 IPv4
        };
        // 无广播地址的网卡（如部分点对点 VPN）：退化为向该网卡 IP 所在
        // 子网的广播地址兜底，若拿不到则跳过
        let Some(broadcast) = broadcast else { continue };
        if broadcast.is_loopback() || broadcast.is_unspecified() {
            continue;
        }

        // socket 绑定到该网卡 IP，确保广播从这块网卡发出
        let Ok(sock) = UdpSocket::bind((*ip, 0)).await else {
            continue; // 个别虚拟网卡可能不允许绑定，跳过即可
        };
        if sock.set_broadcast(true).is_err() {
            continue;
        }

        let reply_tx = reply_tx.clone();
        let frame = frame.clone();
        let dst = SocketAddr::from((*broadcast, DISCOVERY_PORT));
        started += 1;
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            while Instant::now() < deadline {
                if sock.send_to(frame.as_bytes(), dst).await.is_err() {
                    return; // 这块网卡发不出去，放弃
                }
                // 每轮广播后监听一个窗口
                if let Ok(Ok((n, peer))) =
                    tokio::time::timeout(SCAN_BROADCAST_INTERVAL, sock.recv_from(&mut buf)).await
                {
                    if let Some(device) = decode_announce(&buf[..n], &peer) {
                        let _ = reply_tx.send(device);
                    }
                }
            }
        });
    }

    // 回环兜底：本机模拟服务端（127.0.0.1）也纳入扫描，方便联调
    if let Ok(sock) = UdpSocket::bind(("127.0.0.1", 0)).await {
        let reply_tx = reply_tx.clone();
        let frame = frame;
        let dst = SocketAddr::from(([127, 0, 0, 1], DISCOVERY_PORT));
        started += 1;
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            while Instant::now() < deadline {
                if sock.send_to(frame.as_bytes(), dst).await.is_err() {
                    return;
                }
                if let Ok(Ok((n, peer))) =
                    tokio::time::timeout(SCAN_BROADCAST_INTERVAL, sock.recv_from(&mut buf)).await
                {
                    if let Some(device) = decode_announce(&buf[..n], &peer) {
                        let _ = reply_tx.send(device);
                    }
                }
            }
        });
    }

    if started == 0 {
        return Err("未找到可发起扫描的网卡（没有可用的 IPv4 广播地址）".into());
    }
    drop(reply_tx);

    // 等到截止时间，汇总去重（按来源 IP）
    let mut found: HashMap<String, Device> = HashMap::new();
    while Instant::now() < deadline {
        match tokio::time::timeout(deadline.saturating_duration_since(Instant::now()), reply_rx.recv())
            .await
        {
            Ok(Some(device)) => {
                found.insert(device.addr.clone(), device);
            }
            // 所有扫描任务已结束且通道关闭，或超时
            _ => break,
        }
    }

    Ok(found.into_values().collect())
}

/// 解码一条 announce 应答；解不开（不是我们的设备）返回 None
fn decode_announce(data: &[u8], peer: &SocketAddr) -> Option<Device> {
    let reply = crypto::open_frame(std::str::from_utf8(data).ok()?).ok()?;
    if reply["type"] != "announce" {
        return None;
    }
    let name = reply["name"].as_str().unwrap_or("heartbeat-server").to_string();
    let ws_port = reply["ws_port"].as_u64().unwrap_or(protocol::WS_PORT as u64) as u16;
    let ip = peer.ip().to_string();
    Some(Device {
        name,
        addr: format!("{ip}:{ws_port}"),
        id: reply["id"].as_str().map(str::to_string),
    })
}
