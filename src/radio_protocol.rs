//! 带认证的厂商 IE 协议；无需 IP 连接或双方时钟同步。
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

pub const OUI: [u8; 3] = [0x02, 0x53, 0x44]; // 本地管理的私有命名空间
// ESP8266 NONOS 的用户 IE 接口将厂商标记封装在乐鑫 IE 中：
// 外层 OUI 为 18:fe:34，子类型 dd，版本 01，随后为本协议的三字节厂商标记。
const ESP_HEADER: [u8; 5] = [0x18, 0xfe, 0x34, 0xdd, 0x01];
pub const NONCE_LEN: usize = 16;
pub const RESPONSE_LEN: usize = 91;
const TAG_LEN: usize = 16;
#[cfg(test)]
fn tag(data: &[u8]) -> [u8; TAG_LEN] {
    tag_with_key(data, &crate::protocol::RADIO_KEY)
}

fn tag_with_key(data: &[u8], key: &[u8; 32]) -> [u8; TAG_LEN] {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key).unwrap();
    mac.update(data);
    mac.finalize().into_bytes()[..TAG_LEN].try_into().unwrap()
}

pub fn challenge(nonce: &[u8; NONCE_LEN]) -> Vec<u8> {
    challenge_with_key(nonce, &crate::protocol::RADIO_KEY)
}

fn challenge_with_key(nonce: &[u8; NONCE_LEN], key: &[u8; 32]) -> Vec<u8> {
    let mut data = b"ASR2\x01".to_vec();
    data.extend_from_slice(nonce);
    data.extend_from_slice(&tag_with_key(&data, key));
    data
}

pub fn vendor_ie(data: &[u8]) -> Vec<u8> {
    assert!(data.len() <= 247);
    let mut ie = vec![221, (ESP_HEADER.len() + OUI.len() + data.len()) as u8];
    ie.extend_from_slice(&ESP_HEADER);
    ie.extend_from_slice(&OUI);
    ie.extend_from_slice(data);
    ie
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    pub id: u32,
    pub uptime: u32,
    pub boot: u64,
    pub sequence: u32,
    pub heap: u32,
    pub min_heap: u32,
    pub max_block: u32,
    pub fragmentation: u8,
    pub reset_reason: u8,
    pub crash_cause: u32,
    pub crash_pc: u32,
    pub crash_address: u32,
    pub received: u32,
    pub invalid: u32,
}

fn u32_at(data: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap())
}

pub fn response(data: &[u8], nonce: &[u8; NONCE_LEN]) -> Option<Response> {
    response_with_key(data, nonce, &crate::protocol::RADIO_KEY)
}

fn response_with_key(data: &[u8], nonce: &[u8; NONCE_LEN], key: &[u8; 32]) -> Option<Response> {
    if data.len() != RESPONSE_LEN || &data[..5] != b"ASR2\x02" || &data[5..21] != nonce {
        return None;
    }
    let expected = tag_with_key(&data[..75], key);
    if expected
        .iter()
        .zip(&data[75..])
        .fold(0u8, |a, (x, y)| a | (x ^ y))
        != 0
    {
        return None;
    }
    if u32_at(data, 21) > 0x00ff_ffff {
        return None;
    }
    Some(Response {
        id: u32_at(data, 21),
        uptime: u32_at(data, 25),
        boot: u64::from_be_bytes(data[29..37].try_into().unwrap()),
        sequence: u32_at(data, 37),
        heap: u32_at(data, 41),
        min_heap: u32_at(data, 45),
        max_block: u32_at(data, 49),
        fragmentation: data[53],
        reset_reason: data[54],
        crash_cause: u32_at(data, 55),
        crash_pc: u32_at(data, 59),
        crash_address: u32_at(data, 63),
        received: u32_at(data, 67),
        invalid: u32_at(data, 71),
    })
}

/// 拒绝长度异常的 IE；只按元素边界解析，不在任意字节中搜索协议标记。
pub fn responses(ies: &[u8], nonce: &[u8; NONCE_LEN]) -> Vec<Response> {
    let mut rest = ies;
    let mut found = Vec::new();
    while rest.len() >= 2 {
        let len = rest[1] as usize;
        if rest.len() < len + 2 {
            break;
        }
        let data = &rest[2..len + 2];
        if rest[0] == 221
            && data.starts_with(&ESP_HEADER)
            && data.get(5..8) == Some(&OUI)
            && let Some(reply) = response(&data[8..], nonce)
        {
            found.push(reply);
        }
        rest = &rest[len + 2..];
    }
    found
}

pub fn target_id(target: &str) -> Result<u32, String> {
    let id = target
        .strip_prefix("radio://")
        .ok_or("广播目标必须以 radio:// 开头")?;
    if id.len() != 6 || !id.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err("广播设备标识应为 6 位十六进制，例如 radio://d86071".into());
    }
    u32::from_str_radix(id, 16).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample(nonce: &[u8; 16]) -> Vec<u8> {
        let mut data = b"ASR2\x02".to_vec();
        data.extend_from_slice(nonce);
        data.extend_from_slice(&0xd86071u32.to_be_bytes());
        data.extend_from_slice(&300u32.to_be_bytes());
        data.extend_from_slice(&0x0102030405060708u64.to_be_bytes());
        data.extend_from_slice(&42u32.to_be_bytes());
        for value in [36000u32, 32000, 30000] {
            data.extend_from_slice(&value.to_be_bytes());
        }
        data.extend_from_slice(&[5, 2]);
        for value in [28u32, 0x40201000, 0x1234, 43, 1] {
            data.extend_from_slice(&value.to_be_bytes());
        }
        data.extend_from_slice(&tag(&data));
        data
    }
    #[test]
    fn rejects_replay_tampering_and_reflection() {
        let nonce = [0x11; 16];
        let mut data = sample(&nonce);
        assert_eq!(response(&data, &nonce).unwrap().id, 0xd86071);
        assert!(response(&data, &[0x12; 16]).is_none());
        data[25] ^= 1;
        assert!(response(&data, &nonce).is_none());
        assert!(response(&challenge(&nonce), &nonce).is_none());
    }
    #[test]
    fn parses_only_complete_vendor_elements() {
        let nonce = [0x11; 16];
        let valid = vendor_ie(&sample(&nonce));
        let mut ies = vec![0, 3, b'a', b'b', b'c'];
        ies.extend_from_slice(&valid);
        assert_eq!(responses(&ies, &nonce).len(), 1);
        for end in 0..valid.len() {
            assert!(responses(&valid[..end], &nonce).is_empty());
        }
        let mut wrong_oui = valid.clone();
        wrong_oui[2] ^= 1;
        assert!(responses(&wrong_oui, &nonce).is_empty());
        assert!(target_id("radio://d86071").is_ok());
        assert!(target_id("radio://d86071/extra").is_err());
    }
    #[test]
    fn independent_python_vector_matches_firmware_contract() {
        let vector: serde_json::Value =
            serde_json::from_str(include_str!("../tests/radio-vectors.json")).unwrap();
        let decode = |name: &str| -> Vec<u8> {
            vector[name]
                .as_str()
                .unwrap()
                .as_bytes()
                .chunks_exact(2)
                .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
                .collect()
        };
        let nonce: [u8; 16] = decode("nonce").try_into().unwrap();
        let key: [u8; 32] = decode("key").try_into().unwrap();
        assert_eq!(challenge_with_key(&nonce, &key), decode("challenge"));
        assert_eq!(
            &vendor_ie(&challenge(&nonce))[..10],
            &[0xdd, 45, 0x18, 0xfe, 0x34, 0xdd, 1, 2, 0x53, 0x44]
        );
        let reply = response_with_key(&decode("response"), &nonce, &key).unwrap();
        let mut wrong_key = key;
        wrong_key[0] ^= 1;
        assert!(response_with_key(&decode("response"), &nonce, &wrong_key).is_none());
        assert_eq!(reply.id, 0xd86071);
        assert_eq!(
            (
                reply.uptime,
                reply.sequence,
                reply.min_heap,
                reply.max_block,
                reply.heap
            ),
            (300, 42, 32000, 30000, 36000)
        );
        assert_eq!(reply.boot, 0x0102030405060708);
        assert_eq!(
            (
                reply.fragmentation,
                reply.reset_reason,
                reply.crash_cause,
                reply.crash_pc,
                reply.crash_address,
                reply.received,
                reply.invalid
            ),
            (5, 2, 28, 0x40201000, 0x1234, 43, 1)
        );
    }
}
