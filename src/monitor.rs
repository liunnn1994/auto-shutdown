//! 无线挑战监控；只有当前轮次认证成功的应答才刷新在线时间。
use crate::events::{AppEvent, Device, MonitorCommand};
use crate::protocol::HEARTBEAT_TIMEOUT;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use std::time::Instant;

#[derive(Default)]
struct Liveness {
    last_ok: Option<Instant>,
    reported_lost: bool,
    unconfirmed: bool,
}
#[derive(Debug, PartialEq, Eq)]
enum FailureChange {
    Unconfirmed,
    Lost,
}
impl Liveness {
    fn success(&mut self, now: Instant) -> bool {
        let recovered = self.last_ok.is_none() || self.reported_lost || self.unconfirmed;
        self.last_ok = Some(now);
        self.reported_lost = false;
        self.unconfirmed = false;
        recovered
    }
    fn failure(&mut self, now: Instant) -> Option<FailureChange> {
        if !self.reported_lost
            && self
                .last_ok
                .is_some_and(|t| now.duration_since(t) >= HEARTBEAT_TIMEOUT)
        {
            self.reported_lost = true;
            Some(FailureChange::Lost)
        } else if self.last_ok.is_some() && !self.reported_lost && !self.unconfirmed {
            self.unconfirmed = true;
            Some(FailureChange::Unconfirmed)
        } else {
            None
        }
    }
}

pub fn spawn(mut cmds: UnboundedReceiver<MonitorCommand>, events: UnboundedSender<AppEvent>) {
    std::thread::Builder::new()
        .name("radio-monitor".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("无法创建监控 runtime");
            rt.block_on(run(&mut cmds, &events));
        })
        .expect("无法创建广播监控线程");
}
async fn discover() -> Result<Vec<Device>, String> {
    tokio::task::spawn_blocking(crate::radio::discover)
        .await
        .map_err(|e| format!("无线扫描任务失败: {e}"))?
}
async fn heartbeat(target: &str) -> Result<std::time::Duration, String> {
    let target = target.to_owned();
    tokio::task::spawn_blocking(move || crate::radio::heartbeat(&target))
        .await
        .map_err(|e| format!("无线认证任务失败: {e}"))?
}
async fn run(cmds: &mut UnboundedReceiver<MonitorCommand>, events: &UnboundedSender<AppEvent>) {
    let mut target: Option<String> = None;
    let mut liveness = Liveness::default();
    let mut failures = 0u32;
    let mut discovering = true;
    let mut last_discovery: Option<Instant> = None;
    let mut tick = Box::pin(tokio::time::sleep(crate::radio::INTERVAL));
    loop {
        tokio::select! {
            cmd = cmds.recv() => match cmd {
                Ok(MonitorCommand::SetTarget(t)) => {
                    if let Some(value) = &t && let Err(error) = crate::radio_protocol::target_id(value) {
                        let _ = events.unbounded_send(AppEvent::TestFinished(Err(error)));
                        continue;
                    }
                    crate::log_info!("设置广播监控目标: {}", t.as_deref().unwrap_or("(停止监控)"));
                    target = t;
                    discovering = false;
                    liveness = Liveness::default();
                    failures = 0;
                    tick = Box::pin(tokio::time::sleep(std::time::Duration::ZERO));
                }
                Ok(MonitorCommand::Scan) => {
                    let result = discover().await;
                    last_discovery = Some(Instant::now());
                    if result.as_ref().is_ok_and(|d| !d.is_empty()) { discovering = false; }
                    let _ = events.unbounded_send(AppEvent::ScanFinished(result));
                }
                Ok(MonitorCommand::Test(t)) => {
                    let result = heartbeat(&t).await.map(|rtt| format!("广播认证成功，耗时 {} ms。", rtt.as_millis()));
                    let _ = events.unbounded_send(AppEvent::TestFinished(result));
                }
                Err(_) => return,
            },
            _ = &mut tick => {
                if target.is_none() && discovering && last_discovery.is_none_or(|t| t.elapsed() >= crate::radio::INTERVAL) {
                    let result = discover().await;
                    last_discovery = Some(Instant::now());
                    if result.as_ref().is_ok_and(|d| !d.is_empty()) { discovering = false; }
                    let _ = events.unbounded_send(AppEvent::ScanFinished(result));
                }
                if let Some(t) = &target {
                    match heartbeat(t).await {
                        Ok(rtt) => {
                            crate::log_info!("广播认证正常 rtt={}ms target={t} previous_failures={failures}", rtt.as_millis());
                            failures = 0;
                            if liveness.success(Instant::now()) { let _ = events.unbounded_send(AppEvent::HeartbeatOk); }
                        }
                        Err(error) => {
                            failures = failures.saturating_add(1);
                            crate::log_warn!("广播认证失败（连续第 {failures} 次）target={t}: {error}");
                            match liveness.failure(Instant::now()) {
                                Some(FailureChange::Lost) => {
                                    let detail = format!("超过 {} 秒未收到开发板的有效无线挑战应答，按失联关机策略处理（最后一次错误: {error}）", HEARTBEAT_TIMEOUT.as_secs());
                                    crate::log_error!("{detail}");
                                    let _ = events.unbounded_send(AppEvent::HeartbeatLost { detail });
                                }
                                Some(FailureChange::Unconfirmed) => {
                                    let _ = events.unbounded_send(AppEvent::HeartbeatUnconfirmed { detail: error });
                                }
                                None => {}
                            }
                        }
                    }
                }
                tick = Box::pin(tokio::time::sleep(crate::radio::INTERVAL));
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    #[test]
    fn shutdown_is_armed_only_after_authentication_and_reported_once() {
        let now = Instant::now();
        let mut state = Liveness::default();
        assert_eq!(state.failure(now + Duration::from_secs(1000)), None);
        assert!(state.success(now));
        assert_eq!(
            state.failure(now + Duration::from_secs(179)),
            Some(FailureChange::Unconfirmed)
        );
        assert_eq!(
            state.failure(now + Duration::from_secs(180)),
            Some(FailureChange::Lost)
        );
        assert_eq!(state.failure(now + Duration::from_secs(240)), None);
        assert!(state.success(now + Duration::from_secs(241)));
        assert_eq!(
            state.failure(now + Duration::from_secs(420)),
            Some(FailureChange::Unconfirmed)
        );
        assert_eq!(
            state.failure(now + Duration::from_secs(421)),
            Some(FailureChange::Lost)
        );
    }
    #[test]
    fn first_failure_updates_ui_and_recovery_before_threshold_restores_it() {
        let now = Instant::now();
        let mut state = Liveness::default();
        assert!(state.success(now));
        assert_eq!(
            state.failure(now + Duration::from_secs(20)),
            Some(FailureChange::Unconfirmed)
        );
        assert_eq!(state.failure(now + Duration::from_secs(40)), None);
        assert!(state.success(now + Duration::from_secs(50)));
        assert!(!state.success(now + Duration::from_secs(70)));
        assert_eq!(
            state.failure(now + Duration::from_secs(90)),
            Some(FailureChange::Unconfirmed)
        );
    }
}
