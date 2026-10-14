//! 无线认证与失联关机策略。
pub const HEARTBEAT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);
pub const COUNTDOWN_SECONDS: u64 = 60;
pub const SNOOZE_LATER_SECONDS: u64 = 30;
// 构建时从 .env.local 生成；不提供默认密钥或运行时回退。
include!(concat!(env!("OUT_DIR"), "/radio_key.rs"));
