//! 无线认证与失联关机策略。
pub const SNOOZE_LATER_SECONDS: u64 = 30;
// 构建时从 .env.local 生成；不提供默认密钥或运行时回退。
include!(concat!(env!("OUT_DIR"), "/radio_key.rs"));
