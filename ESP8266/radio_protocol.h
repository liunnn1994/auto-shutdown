#pragma once
#include <stdint.h>
#include <stddef.h>
#include <string.h>

// 固件使用的定长认证报文编解码。
namespace asr {
static const uint8_t OUI[3] = {0x02, 0x53, 0x44};
static const size_t CHALLENGE_LEN = 37;
static const size_t RESPONSE_LEN = 91;
typedef void (*MacFn)(const uint8_t[32], const void *, size_t, uint8_t[32]);

inline bool challenge_valid(const uint8_t *data, size_t len, const uint8_t key[32], MacFn mac) {
  if (len != CHALLENGE_LEN || memcmp(data, "ASR2\x01", 5) != 0) return false;
  uint8_t expected[32];
  mac(key, data, 21, expected);
  uint8_t diff = 0;
  for (size_t i = 0; i < 16; ++i) diff |= expected[i] ^ data[21 + i];
  return diff == 0;
}

inline void put32(uint8_t *out, uint32_t value) {
  for (int i = 3; i >= 0; --i) { out[i] = (uint8_t)value; value >>= 8; }
}
struct Telemetry {
  uint32_t id, uptime;
  uint64_t boot;
  uint32_t sequence;
  uint32_t heap, min_heap, max_block;
  uint8_t fragmentation, reset_reason;
  uint32_t crash_cause, crash_pc, crash_address, received, invalid;
};
inline void response(const uint8_t nonce[16], const Telemetry &t,
                     const uint8_t key[32], MacFn mac, uint8_t out[RESPONSE_LEN]) {
  memcpy(out, "ASR2\x02", 5);
  memcpy(out + 5, nonce, 16);
  put32(out + 21, t.id);
  put32(out + 25, t.uptime);
  put32(out + 29, (uint32_t)(t.boot >> 32));
  put32(out + 33, (uint32_t)t.boot);
  put32(out + 37, t.sequence);
  put32(out + 41, t.heap);
  put32(out + 45, t.min_heap);
  put32(out + 49, t.max_block);
  out[53] = t.fragmentation;
  out[54] = t.reset_reason;
  put32(out + 55, t.crash_cause);
  put32(out + 59, t.crash_pc);
  put32(out + 63, t.crash_address);
  put32(out + 67, t.received);
  put32(out + 71, t.invalid);
  uint8_t digest[32];
  mac(key, out, 75, digest);
  memcpy(out + 75, digest, 16);
}
} // 命名空间 asr
