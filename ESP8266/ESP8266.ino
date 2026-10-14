/* NodeMCU ESP8266 无线挑战认证固件。
 * 不连接路由器，不提供 IP 心跳、网页服务、EEPROM 配置或管理员账号。
 * SoftAP 仅用于发送 Beacon 与 Probe Response 中的信息元素。
 */
#include <ESP8266WiFi.h>
#include <esp8266_peri.h>
#include <bearssl/bearssl_hash.h>
#include <bearssl/bearssl_hmac.h>
#include "radio_protocol.h"
#include "radio_guard.h"
#include "radio_key.local.h" // 由 scripts/build-firmware.ps1 生成，已被 Git 忽略

static const char FIRMWARE_REV[] = "radio-only-3";
static asr::RequestGuard g_guard;
static bool g_has_reply = false;
static uint64_t g_boot = 0;
static uint32_t g_sequence = 0, g_received = 0, g_bad = 0;
static uint32_t g_dropped = 0;
static uint8_t g_last_len = 0;
static uint32_t g_min_heap = UINT32_MAX;
static uint32_t g_last_reply_ms = 0, g_last_stats_ms = 0;
static rst_info g_reset = {};

static void hmac_sha256(const uint8_t key[32], const void *data, size_t len, uint8_t out[32]) {
  br_hmac_key_context kc;
  br_hmac_context hc;
  br_hmac_key_init(&kc, &br_sha256_vtable, key, 32);
  br_hmac_init(&hc, &kc, 0);
  br_hmac_update(&hc, data, len);
  br_hmac_out(&hc, out);
}

// SDK 回调与主循环协作执行；队列不会覆盖尚未处理的请求。
static void radio_received(uint8_t type, const uint8_t *, const uint8_t oui[3],
                           uint8_t *data, uint8_t len, int) {
  if (type != USER_IE_PROBE_REQ || !oui || !data || memcmp(oui, asr::OUI, 3) != 0) return;
  ++g_received;
  g_last_len = len;
  if (len != asr::CHALLENGE_LEN || memcmp(data, "ASR2\x01", 5) != 0) { ++g_bad; return; }
  if (!g_guard.enqueue(data, len, millis())) ++g_dropped;
}

static void radio_tick() {
  uint32_t heap = ESP.getFreeHeap();
  if (heap < g_min_heap) g_min_heap = heap;
  uint8_t request[asr::CHALLENGE_LEN];
  // 每轮最多验证一次 HMAC；末尾的 delay(1) 始终给 SDK 留出执行时间。
  if (!g_guard.pop(request)) return;
  if (!asr::challenge_valid(request, sizeof(request), RADIO_KEY, hmac_sha256)) {
    g_guard.reject(request);
    ++g_bad;
    return;
  }
  if (!g_guard.can_publish(request + 5, millis())) { ++g_dropped; return; }
  asr::Telemetry telemetry = {
    ESP.getChipId(), millis() / 1000, g_boot, ++g_sequence,
    heap, g_min_heap, ESP.getMaxFreeBlockSize(), ESP.getHeapFragmentation(),
    (uint8_t)g_reset.reason, g_reset.exccause, g_reset.epc1, g_reset.excvaddr,
    g_received, g_bad
  };
  static uint8_t reply[asr::RESPONSE_LEN];
  asr::response(request + 5, telemetry, RADIO_KEY, hmac_sha256, reply);
  uint8_t oui[3];
  memcpy(oui, asr::OUI, sizeof(oui));
  // NONOS SDK 要求先禁用已有用户 IE，再写入新的应答。
  wifi_set_user_ie(false, NULL, USER_IE_BEACON, NULL, 0);
  bool beacon = wifi_set_user_ie(true, oui, USER_IE_BEACON, reply, sizeof(reply));
  wifi_set_user_ie(false, NULL, USER_IE_PROBE_RESP, NULL, 0);
  bool probe = wifi_set_user_ie(true, oui, USER_IE_PROBE_RESP, reply, sizeof(reply));
  g_has_reply = beacon && probe;
  if (g_has_reply) {
    g_guard.published(request + 5, millis());
    g_last_reply_ms = millis();
    digitalWrite(LED_BUILTIN, LOW);
  } else {
    Serial.printf("radio publish failed: beacon=%d probe=%d\n", beacon, probe);
  }
}

void setup() {
  Serial.begin(115200);
  pinMode(LED_BUILTIN, OUTPUT);
  digitalWrite(LED_BUILTIN, HIGH);
  const rst_info *reset = ESP.getResetInfoPtr();
  if (reset) g_reset = *reset;
  if (g_reset.reason != REASON_EXCEPTION_RST && g_reset.reason != REASON_SOFT_WDT_RST) {
    g_reset.exccause = g_reset.epc1 = g_reset.excvaddr = 0;
  }
  Serial.printf("\nfirmware=%s built=%s %s reset=%s\n", FIRMWARE_REV, __DATE__, __TIME__,
                ESP.getResetReason().c_str());
  g_boot = ((uint64_t)RANDOM_REG32 << 32) | RANDOM_REG32;

  WiFi.persistent(false);
  WiFi.setAutoReconnect(false);
  WiFi.mode(WIFI_AP); // 不启动客户端接口，也不读取已保存的连接凭据
  WiFi.setSleepMode(WIFI_NONE_SLEEP);
  char ssid[32], pass[33];
  snprintf(ssid, sizeof(ssid), "auto-shutdown-%06X", ESP.getChipId());
  // 每次启动生成不公开的 AP 密码，防止客户端加入；扫描不需要该密码。
  for (size_t i = 0; i < 4; ++i) snprintf(pass + i * 8, 9, "%08lX", (unsigned long)RANDOM_REG32);
  bool ap = WiFi.softAP(ssid, pass, 6, false, 1);
  wifi_softap_dhcps_stop(); // 只需要管理帧广播，关闭 DHCP 服务
  int callback = wifi_register_user_ie_manufacturer_recv_cb(radio_received);
  Serial.printf("radio AP=%d callback=%d target=radio://%06x\n", ap, callback, (unsigned)ESP.getChipId());
}

void loop() {
  radio_tick();
  uint32_t now = millis();
  if (now - g_last_stats_ms >= 300000) {
    g_last_stats_ms = now;
    Serial.printf("radio stats: rx=%lu replies=%lu bad=%lu dropped=%lu len=%u heap=%u min=%lu\n",
                  (unsigned long)g_received, (unsigned long)g_sequence,
                  (unsigned long)g_bad, (unsigned long)g_dropped, g_last_len, (unsigned)ESP.getFreeHeap(),
                  (unsigned long)g_min_heap);
  }
  static uint32_t last_led_ms = 0;
  bool active = g_has_reply && now - g_last_reply_ms < 45000;
  if (!active && now - last_led_ms >= 500) {
    last_led_ms = now;
    digitalWrite(LED_BUILTIN, !digitalRead(LED_BUILTIN));
  } else if (active && now - g_last_reply_ms > 120) {
    digitalWrite(LED_BUILTIN, HIGH);
  }
  delay(1);
}
