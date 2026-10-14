#pragma once
#include "radio_protocol.h"

namespace asr {
// 固定存储、协作式 SDK 回调；不分配堆内存，回调不执行密码学运算。
class RequestGuard {
 public:
  static const uint8_t CAPACITY = 8;
  static const uint8_t RECENT = 64;
  static const uint32_t REFILL_MS = 16; // 允许突发 8 次，此后每 16 ms 补充一次验证额度
  static const uint32_t REPLAY_MS = 600000;
  static const uint32_t PUBLISH_MS = 250;

  bool enqueue(const uint8_t *data, size_t len, uint32_t now) {
    if (!data || len != CHALLENGE_LEN || memcmp(data, "ASR2\x01", 5) != 0) return false;
    // 已知请求提前丢弃，不消耗队列槽位或 HMAC 验证额度。
    if (seen(data + 5, now)) return false;
    for (uint8_t i = 0; i < count_; ++i)
      if (memcmp(queue_[(head_ + i) % CAPACITY], data, CHALLENGE_LEN) == 0) return false;
    for (uint8_t i = 0; i < rejected_count_; ++i)
      if (memcmp(rejected_[i], data, CHALLENGE_LEN) == 0) return false;
    refill(now);
    if (!tokens_ || count_ == CAPACITY) return false;
    --tokens_;
    memcpy(queue_[(head_ + count_) % CAPACITY], data, CHALLENGE_LEN);
    ++count_;
    return true;
  }

  bool pop(uint8_t out[CHALLENGE_LEN]) {
    if (!count_) return false;
    memcpy(out, queue_[head_], CHALLENGE_LEN);
    head_ = (head_ + 1) % CAPACITY;
    --count_;
    return true;
  }

  void reject(const uint8_t data[CHALLENGE_LEN]) {
    memcpy(rejected_[rejected_next_], data, CHALLENGE_LEN);
    rejected_next_ = (rejected_next_ + 1) % CAPACITY;
    if (rejected_count_ < CAPACITY) ++rejected_count_;
  }

  bool can_publish(const uint8_t nonce[16], uint32_t now) const {
    return (!published_ || uint32_t(now - last_publish_) >= PUBLISH_MS) && !seen(nonce, now);
  }

  void published(const uint8_t nonce[16], uint32_t now) {
    memcpy(recent_[recent_next_].nonce, nonce, 16);
    recent_[recent_next_].at = now;
    recent_next_ = (recent_next_ + 1) % RECENT;
    if (recent_count_ < RECENT) ++recent_count_;
    published_ = true;
    last_publish_ = now;
  }

 private:
  struct Nonce { uint8_t nonce[16]; uint32_t at; };
  uint8_t queue_[CAPACITY][CHALLENGE_LEN] = {};
  uint8_t rejected_[CAPACITY][CHALLENGE_LEN] = {};
  Nonce recent_[RECENT] = {};
  uint8_t head_ = 0, count_ = 0, tokens_ = CAPACITY;
  uint8_t rejected_next_ = 0, rejected_count_ = 0, recent_next_ = 0, recent_count_ = 0;
  uint32_t refill_at_ = 0, last_publish_ = 0;
  bool published_ = false;

  void refill(uint32_t now) {
    uint32_t gained = uint32_t(now - refill_at_) / REFILL_MS;
    if (!gained) return;
    uint32_t room = CAPACITY - tokens_;
    tokens_ = uint8_t(tokens_ + (gained >= room ? room : gained));
    refill_at_ = now - uint32_t(now - refill_at_) % REFILL_MS;
  }

  bool seen(const uint8_t nonce[16], uint32_t now) const {
    for (uint8_t i = 0; i < recent_count_; ++i)
      if (uint32_t(now - recent_[i].at) < REPLAY_MS && memcmp(recent_[i].nonce, nonce, 16) == 0)
        return true;
    return false;
  }
};
} // 命名空间 asr
