//! 无线挑战监控；只有当前轮次认证成功的应答才刷新在线时间。
use crate::events::{AppEvent, Device, MonitorCommand};
use crate::config::Settings;
use std::time::Duration;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use std::time::Instant;
use std::{collections::VecDeque, future::Future};

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
    fn failure(&mut self, now: Instant, timeout: Duration) -> Option<FailureChange> {
        if !self.reported_lost
            && self
                .last_ok
                .is_some_and(|t| now.duration_since(t) >= timeout)
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

pub fn spawn(mut cmds: UnboundedReceiver<MonitorCommand>, events: UnboundedSender<AppEvent>, settings: Settings) {
    std::thread::Builder::new()
        .name("radio-monitor".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("无法创建监控 runtime");
            rt.block_on(run(&mut cmds, &events, settings));
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

/// 无线认证结束后，先应用期间保存的全部阈值更新，再交给失联判定。
/// 其他命令按原顺序留给主循环，避免吞掉扫描、测试或目标切换。
async fn heartbeat_with_timeout_updates(
    authentication: impl Future<Output = Result<Duration, String>>,
    cmds: &mut UnboundedReceiver<MonitorCommand>,
    pending: &mut VecDeque<MonitorCommand>,
    timeout: &mut Duration,
) -> Result<Duration, String> {
    let result = authentication.await;
    while let Ok(command) = cmds.try_recv() {
        match command {
            MonitorCommand::SetTimeout(seconds) => {
                *timeout = Duration::from_secs(seconds.into());
            }
            other => pending.push_back(other),
        }
    }
    result
}

async fn run(cmds: &mut UnboundedReceiver<MonitorCommand>, events: &UnboundedSender<AppEvent>, settings: Settings) {
    let mut timeout = Duration::from_secs(settings.heartbeat_timeout_seconds.into());
    let mut pending = VecDeque::new();
    let mut target: Option<String> = None;
    let mut liveness = Liveness::default();
    let mut failures = 0u32;
    let mut discovering = true;
    let mut last_discovery: Option<Instant> = None;
    let mut tick = Box::pin(tokio::time::sleep(crate::radio::INTERVAL));
    loop {
        tokio::select! {
            biased;
            cmd = async {
                match pending.pop_front() {
                    Some(command) => Ok(command),
                    None => cmds.recv().await,
                }
            } => match cmd {
                Ok(MonitorCommand::SetTimeout(seconds)) => {
                    timeout = Duration::from_secs(seconds.into());
                }
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
                    match heartbeat_with_timeout_updates(heartbeat(t), cmds, &mut pending, &mut timeout).await {
                        Ok(rtt) => {
                            crate::log_info!("广播认证正常 rtt={}ms target={t} previous_failures={failures}", rtt.as_millis());
                            failures = 0;
                            if liveness.success(Instant::now()) { let _ = events.unbounded_send(AppEvent::HeartbeatOk); }
                        }
                        Err(error) => {
                            failures = failures.saturating_add(1);
                            crate::log_warn!("广播认证失败（连续第 {failures} 次）target={t}: {error}");
                            match liveness.failure(Instant::now(), timeout) {
                                Some(FailureChange::Lost) => {
                                    let detail = format!("超过 {} 秒未收到开发板的有效无线挑战应答，按失联关机策略处理（最后一次错误: {error}）", timeout.as_secs());
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
    const TIMEOUT: Duration = Duration::from_secs(60);
    #[test]
    fn shutdown_is_armed_only_after_authentication_and_reported_once() {
        let now = Instant::now();
        let mut state = Liveness::default();
        assert_eq!(state.failure(now + Duration::from_secs(1000), TIMEOUT), None);
        assert!(state.success(now));
        assert_eq!(
            state.failure(now + Duration::from_secs(59), TIMEOUT),
            Some(FailureChange::Unconfirmed)
        );
        assert_eq!(
            state.failure(now + Duration::from_secs(60), TIMEOUT),
            Some(FailureChange::Lost)
        );
        assert_eq!(state.failure(now + Duration::from_secs(240), TIMEOUT), None);
        assert!(state.success(now + Duration::from_secs(241)));
        assert_eq!(
            state.failure(now + Duration::from_secs(300), TIMEOUT),
            Some(FailureChange::Unconfirmed)
        );
        assert_eq!(
            state.failure(now + Duration::from_secs(301), TIMEOUT),
            Some(FailureChange::Lost)
        );
    }
    #[test]
    fn first_failure_updates_ui_and_recovery_before_threshold_restores_it() {
        let now = Instant::now();
        let mut state = Liveness::default();
        assert!(state.success(now));
        assert_eq!(
            state.failure(now + Duration::from_secs(20), TIMEOUT),
            Some(FailureChange::Unconfirmed)
        );
        assert_eq!(state.failure(now + Duration::from_secs(40), TIMEOUT), None);
        assert!(state.success(now + Duration::from_secs(50)));
        assert!(!state.success(now + Duration::from_secs(70)));
        assert_eq!(
            state.failure(now + Duration::from_secs(90), TIMEOUT),
            Some(FailureChange::Unconfirmed)
        );
    }

    #[test]
    fn changing_timeout_preserves_last_success_and_does_not_repeat_loss() {
        let now = Instant::now();
        let mut state = Liveness::default();
        state.success(now);
        assert_eq!(state.failure(now + Duration::from_secs(40), TIMEOUT), Some(FailureChange::Unconfirmed));
        // 增大阈值延后判定，缩短阈值则仍从同一次成功认证起计算。
        assert_eq!(state.failure(now + Duration::from_secs(70), Duration::from_secs(120)), None);
        assert_eq!(state.failure(now + Duration::from_secs(71), Duration::from_secs(30)), Some(FailureChange::Lost));
        assert_eq!(state.failure(now + Duration::from_secs(72), Duration::from_secs(1)), None);
    }

    #[test]
    fn saved_timeouts_during_authentication_apply_before_failure() {
        futures::executor::block_on(async {
            let (tx, mut rx) = futures::channel::mpsc::unbounded();
            let (started_tx, started_rx) = futures::channel::oneshot::channel();
            let (finish_tx, finish_rx) = futures::channel::oneshot::channel();
            let mut pending = VecDeque::new();
            let mut timeout = TIMEOUT;
            let now = Instant::now();
            let mut state = Liveness::default();
            state.success(now);

            let authentication = async {
                started_tx.send(()).unwrap();
                finish_rx.await.unwrap();
                Err("模拟认证失败".to_string())
            };
            let save_while_authenticating = async {
                started_rx.await.unwrap();
                // 阈值更新前后夹有普通命令，最后一次保存应生效。
                tx.unbounded_send(MonitorCommand::Scan).unwrap();
                tx.unbounded_send(MonitorCommand::SetTimeout(90)).unwrap();
                tx.unbounded_send(MonitorCommand::Test("radio://d86071".into())).unwrap();
                tx.unbounded_send(MonitorCommand::SetTimeout(120)).unwrap();
                tx.unbounded_send(MonitorCommand::SetTarget(None)).unwrap();
                finish_tx.send(()).unwrap();
            };
            let (result, ()) = futures::join!(
                heartbeat_with_timeout_updates(authentication, &mut rx, &mut pending, &mut timeout),
                save_while_authenticating,
            );
            assert!(result.is_err());
            assert_eq!(timeout, Duration::from_secs(120));
            // 61 秒超过旧阈值，但本轮失败不能触发失联。
            assert_eq!(state.failure(now + Duration::from_secs(61), timeout), Some(FailureChange::Unconfirmed));
            assert_eq!(state.failure(now + Duration::from_secs(119), timeout), None);
            assert_eq!(state.failure(now + Duration::from_secs(120), timeout), Some(FailureChange::Lost));
            assert!(matches!(pending.pop_front(), Some(MonitorCommand::Scan)));
            assert!(matches!(pending.pop_front(), Some(MonitorCommand::Test(target)) if target == "radio://d86071"));
            assert!(matches!(pending.pop_front(), Some(MonitorCommand::SetTarget(None))));
            assert!(pending.is_empty());
        });
    }

    #[test]
    fn shortened_timeout_during_authentication_applies_to_current_failure() {
        futures::executor::block_on(async {
            let (tx, mut rx) = futures::channel::mpsc::unbounded();
            let mut pending = VecDeque::new();
            let mut timeout = TIMEOUT;
            let authentication = async {
                tx.unbounded_send(MonitorCommand::SetTimeout(30)).unwrap();
                Err("模拟认证失败".to_string())
            };
            assert!(heartbeat_with_timeout_updates(authentication, &mut rx, &mut pending, &mut timeout).await.is_err());
            let now = Instant::now();
            let mut state = Liveness::default();
            state.success(now);
            assert_eq!(state.failure(now + Duration::from_secs(31), timeout), Some(FailureChange::Lost));
        });
    }
}
