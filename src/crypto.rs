//! # 报文加解密
//!
//! 实现 protocol.rs 模块注释中描述的加密帧：
//!
//! ```text
//! Base64( HMAC-SHA256(k_mac, IV || CT) || IV || AES-256-CTR(k_enc, IV, 明文) )
//! ```
//!
//! 服务端移植提示（以 mbedtls 为例）：
//! - 加解密: `mbedtls_cipher` AES-256-CTR，计数器为 16 字节大端整块递增
//!   （与 mbedtls / Python cryptography 的默认行为一致）；
//! - 校验: `mbedtls_md_hmac` HMAC-SHA256；
//! - 密钥派生: SHA-256(`PASSPHRASE:enc`) / SHA-256(`PASSPHRASE:mac`)。

use aes::Aes256;
use base64::Engine as _;
use ctr::cipher::{KeyIvInit, StreamCipher};
use ctr::Ctr128BE;
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use std::sync::LazyLock;

use crate::protocol::PASSPHRASE;

const IV_LEN: usize = 16;
const MAC_LEN: usize = 32;
/// 派生密钥长度（AES-256 / HMAC-SHA256 均为 32 字节）
const KEY_LEN: usize = 32;

type AesCtr = Ctr128BE<Aes256>;
type HmacSha256 = Hmac<Sha256>;

/// 由口令派生出的加/校验密钥对
struct Keys {
    /// 加密密钥（AES-256-CTR）
    enc: [u8; KEY_LEN],
    /// 校验密钥（HMAC-SHA256）
    mac: [u8; KEY_LEN],
}

static KEYS: LazyLock<Keys> = LazyLock::new(|| Keys {
    enc: sha256(format!("{PASSPHRASE}:enc").as_bytes()),
    mac: sha256(format!("{PASSPHRASE}:mac").as_bytes()),
});

fn sha256(data: &[u8]) -> [u8; KEY_LEN] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().into()
}

/// 用 AES-256-CTR 加解密（CTR 模式加解密为同一操作）
fn ctr_crypt(key: &[u8; KEY_LEN], iv: &[u8; IV_LEN], data: &mut [u8]) {
    let mut cipher =
        AesCtr::new_from_slices(key, iv).expect("密钥/IV 长度固定且合法");
    cipher.apply_keystream(data);
}

/// 计算帧 HMAC（覆盖 IV 与密文）
fn frame_mac(iv: &[u8], ct: &[u8]) -> [u8; MAC_LEN] {
    let mut mac = <HmacSha256 as KeyInit>::new_from_slice(&KEYS.mac)
        .expect("HMAC 接受任意长度密钥");
    mac.update(iv);
    mac.update(ct);
    mac.finalize().into_bytes().into()
}

/// 加密一条 JSON 明文，返回可直接发送的 Base64 文本帧
pub fn seal_json(value: &serde_json::Value) -> String {
    let plaintext = serde_json::to_vec(value).expect("JSON 序列化不会失败");
    let mut iv = [0u8; IV_LEN];
    rand::Rng::fill_bytes(&mut rand::rng(), &mut iv);

    let mut ct = plaintext;
    ctr_crypt(&KEYS.enc, &iv, &mut ct);
    let mac = frame_mac(&iv, &ct);

    // 帧 = MAC || IV || CT
    let mut frame = Vec::with_capacity(MAC_LEN + IV_LEN + ct.len());
    frame.extend_from_slice(&mac);
    frame.extend_from_slice(&iv);
    frame.extend_from_slice(&ct);
    base64::engine::general_purpose::STANDARD.encode(frame)
}

/// 解密一条 Base64 文本帧，返回其中的 JSON 明文。
///
/// HMAC 校验失败会直接报错（说明发送方不是持有口令的合法设备）。
pub fn open_frame(frame: &str) -> anyhow::Result<serde_json::Value> {
    let data = base64::engine::general_purpose::STANDARD
        .decode(frame.trim())
        .map_err(|e| anyhow::anyhow!("报文不是合法的 Base64: {e}"))?;
    anyhow::ensure!(
        data.len() > MAC_LEN + IV_LEN,
        "报文长度不足（{} 字节），不是合法的加密帧",
        data.len()
    );

    let (mac, rest) = data.split_at(MAC_LEN);
    let (iv, ct) = rest.split_at(IV_LEN);

    // 先校验 HMAC，再解密（encrypt-then-MAC）
    anyhow::ensure!(
        mac_eq(mac, &frame_mac(iv, ct)),
        "HMAC 校验失败：报文来源不是合法设备（口令不一致或数据被篡改）"
    );

    let mut iv_buf = [0u8; IV_LEN];
    iv_buf.copy_from_slice(iv);
    let mut plaintext = ct.to_vec();
    ctr_crypt(&KEYS.enc, &iv_buf, &mut plaintext);

    serde_json::from_slice(&plaintext).map_err(|e| anyhow::anyhow!("解密后的报文不是合法 JSON: {e}"))
}

/// 常数时间比较，避免时序侧信道（局域网内威胁模型下属于锦上添花）
fn mac_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 加解密往返
    #[test]
    fn seal_open_roundtrip() {
        let msg = serde_json::json!({"type": "ping", "ts": 1, "nonce": "abc"});
        let frame = seal_json(&msg);
        assert_eq!(open_frame(&frame).unwrap(), msg);
    }

    /// 篡改密文应导致 HMAC 校验失败
    #[test]
    fn tampered_frame_rejected() {
        let msg = serde_json::json!({"type": "ping"});
        let mut frame = seal_json(&msg);
        // 翻转最后一个字符（密文末尾）
        let last = frame.pop().unwrap();
        frame.push(if last == 'A' { 'B' } else { 'A' });
        assert!(open_frame(&frame).is_err());
    }

    /// 与 Python 端（python/protocol.py，固定 IV=00..0f）的互验：
    /// 该帧由 python/protocol.py 运行 self-test 生成，本测试保证两端
    /// 密钥派生 / AES-CTR / HMAC 实现完全一致。
    #[test]
    fn python_frame_opens_in_rust() {
        let frame = "HoOyT2EiuDzhoa8ymy6n1GxoIFh8UrTcj+iLw7+2dLwAAQIDBAUGBwgJCgsMDQ4PRsWPDkRICbkgZw1sUBVjl8FQNlwWAWcZ+zE+GNCMckjJ8miE4qCq2yIUUw7BhKj7051CCYiMe28fBvI9tA==";
        let msg = open_frame(frame).unwrap();
        assert_eq!(msg["type"], "ping");
        assert_eq!(msg["ts"], 1_700_000_000_000u64);
        assert_eq!(msg["nonce"], "0011223344556677");
    }
}

