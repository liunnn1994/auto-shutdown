# -*- coding: utf-8 -*-
"""
auto-shutdown —— 通信协议实现（Python 测试服务端用，可移植到 ESP32）

本文件实现与 PC 端（Rust, src/crypto.rs + src/protocol.rs）完全一致的加密帧：

    帧 = Base64( HMAC-SHA256(k_mac, IV || CT)[32B] || IV[16B] || CT )

- IV   : 每条报文随机生成的 16 字节初始向量；
- CT   : AES-256-CTR(k_enc, IV, 明文 JSON 字节)；
- k_enc: SHA-256(PASSPHRASE + ":enc")   —— 加密密钥（32 字节）
- k_mac: SHA-256(PASSPHRASE + ":mac")   —— 校验密钥（32 字节）

注意两端必须使用相同的 PASSPHRASE（默认值见下），否则 HMAC 校验会失败。

## ESP32 移植提示（mbedtls / Arduino）

- AES-256-CTR: mbedtls_aes_context + MBEDTLS_MODE_CTR，
  CTR 计数器为整个 16 字节 IV 按大端整体递增（与 mbedtls、本实现一致）；
- HMAC-SHA256: mbedtls_md_hmac(MBEDTLS_MD_SHA256, k_mac, 32, ...)；
- 密钥派生: mbedtls_sha256 对字符串 "PASSPHRASE:enc" / "PASSPHRASE:mac" 求摘要；
- 随机 IV: esp_fill_random()。

## 报文类型（明文 JSON）

- {"type": "ping",     "ts": <毫秒时间戳>, "nonce": "<hex>"}  PC -> ESP32（心跳）
- {"type": "pong",     "nonce": "<同 ping>", "uptime_s": <秒>} ESP32 -> PC
- {"type": "discover", "nonce": "<hex>"}                     PC 广播（UDP 发现）
- {"type": "announce", "name": "heartbeat-server", "ws_port": 8123}  服务端单播应答（UDP）
"""

import base64
import hashlib
import hmac as hmac_mod
import json
import os

# 与 PC 端 src/protocol.rs 中的 PASSPHRASE 必须一致！
PASSPHRASE = "auto-shutdown-v1"

# 加密帧各部分长度
IV_LEN = 16   # AES 块大小
MAC_LEN = 32  # HMAC-SHA256 输出长度
KEY_LEN = 32  # AES-256 密钥长度

# 由口令派生密钥（模块加载时计算一次）
_KEY_ENC = hashlib.sha256((PASSPHRASE + ":enc").encode("utf-8")).digest()
_KEY_MAC = hashlib.sha256((PASSPHRASE + ":mac").encode("utf-8")).digest()


def _ctr_xor(key: bytes, iv: bytes, data: bytes) -> bytes:
    """AES-256-CTR 加解密（流模式加解密同函数）。

    使用 `cryptography` 库的 CTR 模式：16 字节 IV 作为计数器初值，
    整块按大端递增 —— 与 mbedtls / Rust 端行为一致。
    """
    from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes

    cipher = Cipher(algorithms.AES(key), modes.CTR(iv))
    enc = cipher.encryptor()
    return enc.update(data) + enc.finalize()


def _frame_mac(iv: bytes, ct: bytes) -> bytes:
    """计算帧 HMAC（覆盖 IV 与密文）"""
    return hmac_mod.new(_KEY_MAC, iv + ct, hashlib.sha256).digest()


def seal(obj: dict, iv: bytes | None = None) -> str:
    """加密一条 JSON 报文，返回 Base64 文本帧。

    iv 参数仅供测试（固定 IV 得到确定性输出）；正常运行时使用随机 IV。
    """
    plaintext = json.dumps(obj, separators=(",", ":")).encode("utf-8")
    iv = iv or os.urandom(IV_LEN)
    if len(iv) != IV_LEN:
        raise ValueError("IV 必须为 16 字节")
    ct = _ctr_xor(_KEY_ENC, iv, plaintext)
    mac = _frame_mac(iv, ct)
    return base64.b64encode(mac + iv + ct).decode("ascii")


def open_frame(frame: str) -> dict:
    """解密一条 Base64 文本帧，返回其中的 JSON 对象。

    任何解密/校验失败都会抛出 ValueError —— 对调用方而言这意味
    着报文来源不是持有口令的合法设备，直接丢弃即可。
    """
    try:
        data = base64.b64decode(frame.strip())
    except Exception as e:
        raise ValueError(f"报文不是合法的 Base64: {e}") from e
    if len(data) <= MAC_LEN + IV_LEN:
        raise ValueError(f"报文长度不足（{len(data)} 字节）")
    mac, rest = data[:MAC_LEN], data[MAC_LEN:]
    iv, ct = rest[:IV_LEN], rest[IV_LEN:]

    # 先校验 HMAC，再解密（encrypt-then-MAC）
    if not hmac_mod.compare_digest(mac, _frame_mac(iv, ct)):
        raise ValueError("HMAC 校验失败：报文来源不是合法设备")

    plaintext = _ctr_xor(_KEY_ENC, iv, ct)
    try:
        return json.loads(plaintext.decode("utf-8"))
    except Exception as e:
        raise ValueError(f"解密后的报文不是合法 JSON: {e}") from e


def random_nonce(n: int = 8) -> str:
    """生成 2n 位十六进制随机数（用作报文 nonce）"""
    return os.urandom(n).hex()


if __name__ == "__main__":
    # 简单自测：加解密往返 + 固定 IV 的确定性输出（用于与 Rust 端互验）
    msg = {"type": "ping", "ts": 1700000000000, "nonce": "0011223344556677"}
    sealed = seal(msg, iv=bytes(range(16)))
    print("固定 IV 加密帧（供与 Rust 端互验）:")
    print(" ", sealed)
    assert open_frame(sealed) == msg
    print("自测通过：seal/open 往返一致")
