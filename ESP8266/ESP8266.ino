/*
 * auto-shutdown —— ESP8266 心跳服务端（NodeMCU / ESP-12E）
 *
 * 硬件：NodeMCU ESP8266 开发板（ESP-12E + CH340 USB 转串口），无其他配件。
 *
 * 行为：
 *   1. WebSocket 服务端（8123）：收到加密 ping -> 回加密 pong（原样带回 nonce）；
 *   2. UDP 服务端（8124）：收到加密 discover -> 向来源单播加密 announce；
 *   3. Web 管理界面（80）：htmx 单页界面，管理员登录后可配置 WiFi、
 *      修改管理员密码、查看运行状态。配置保存在 EEPROM，掉电不丢。
 *   4. 日志上报：启动 / WiFi 掉线重连 / 配置变更 / 非法报文 / 周期统计等
 *      事件记入内存环形缓冲，随下一次心跳连接以加密 "log" 报文推送给 PC，
 *      由 PC 端写入 ~/.auto-shutdown/logs/esp-日期.log（与 PC 自身日志分开）。
 *
 * 首次使用：没有 WiFi 配置时设备自动开启配置热点 `auto-shutdown-xxxxxx`
 * （无密码），电脑/手机连上后浏览器访问 http://192.168.4.1 ，用默认管理员
 * admin/admin 登录，填入 WiFi 账号密码；连接成功后热点自动关闭，之后通过
 * 设备的局域网 IP 即可访问管理界面（PC 端心跳地址为 设备IP:8123）。
 *
 * 加密帧（与 python/protocol.py / src/crypto.rs 完全一致）：
 *
 *     帧 = Base64( HMAC-SHA256(k_mac, IV || CT)[32B] || IV[16B] || CT )
 *
 *   - CT = AES-256-CTR(k_enc, IV, 明文 JSON)，16 字节计数器大端整块递增；
 *   - k_enc = SHA-256(PASSPHRASE ":enc")，k_mac = SHA-256(PASSPHRASE ":mac")；
 *   - 校验顺序 encrypt-then-MAC：先比对 HMAC（常数时间）再解密，失败直接丢弃。
 *
 * 加密使用 ESP8266 Arduino 核心自带的 BearSSL（无需额外安装加密库）：
 *   - SHA-256 / SHA-1 : bearssl_hash.h（br_sha256_* / br_sha1_*，SHA-1 供 WS 握手）
 *   - HMAC-SHA256     : bearssl_hmac.h（br_hmac_*）
 *   - AES-256         : bearssl_block.h（br_aes_big_*；CTR 由本文件按协议手工
 *                       实现：对计数器块做单块加密得到密钥流后异或，与
 *                       mbedtls / Python cryptography 的行为一致）
 *
 * 需要 ESP8266 Arduino 核心 3.x（在开发板管理器中安装 "esp8266"）。
 */

#include <ESP8266WiFi.h>
#include <ESP8266WebServer.h>
#include <DNSServer.h>
#include <WiFiUdp.h>
#include <EEPROM.h>
#include <esp8266_peri.h>          // RANDOM_REG32：硬件随机数（报文 IV / 会话令牌）
#include <bearssl/bearssl_hash.h>  // br_sha256_* / br_sha1_*
#include <bearssl/bearssl_hmac.h>  // br_hmac_*
#include <bearssl/bearssl_block.h> // br_aes_big_*
#include <ctype.h>
#include <stdarg.h>                // 日志格式化（vsnprintf）
#include "htmx_min.h"              // 自动生成：单文件 HTML 片段（web/ 目录 Vite 构建）

// ---------------------------------------------------------------------------
// 用户配置（务必与 PC 端一致：口令必须等于 src/protocol.rs 的 PASSPHRASE）
// ---------------------------------------------------------------------------

static const char PASSPHRASE[]  = "auto-shutdown-v1"; // 与 src/protocol.rs 保持一致！
static const char DEV_NAME[]    = "heartbeat-server"; // announce 中展示的设备名
static const uint16_t WEB_PORT  = 80;                 // Web 管理界面端口
static const uint16_t WS_PORT   = 8123;               // WebSocket 心跳端口
static const uint16_t UDP_PORT  = 8124;               // UDP 发现端口

// ---------------------------------------------------------------------------
// 协议常量与缓冲区
// ---------------------------------------------------------------------------

#define PLAIN_MAX 220                                   // 明文 JSON 最大长度
#define MAC_LEN   32                                    // HMAC-SHA256 输出长度
#define IV_LEN    16                                    // AES 块大小
#define FRAME_MAX (MAC_LEN + IV_LEN + PLAIN_MAX)        // 加密帧（解码后）最大长度
#define B64_MAX   (((FRAME_MAX + 2) / 3) * 4 + 1)       // Base64 文本最大长度 + 1

static const char WS_GUID[] = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

// ---------------------------------------------------------------------------
// 全局状态
// ---------------------------------------------------------------------------

static br_aes_big_cbcenc_keys g_aes;   // AES-256 密钥表（k_enc）
static uint8_t g_k_enc[32];
static uint8_t g_k_mac[32];

static WiFiServer g_ws_server(WS_PORT);
static WiFiUDP g_udp;
static ESP8266WebServer g_web(WEB_PORT);
static DNSServer g_dns;                // 配置热点模式的 Captive Portal DNS（任意域名 -> 本机）

static uint32_t g_hb_count   = 0;      // 成功应答的心跳次数
static uint32_t g_bad_count  = 0;      // 丢弃的非法报文数
static uint32_t g_last_hb_ms = 0;
static char     g_last_client[16] = "-";

static char    g_session[17] = "";     // 当前管理员会话令牌（空 = 未登录）
static bool    g_ap_active  = false;
static uint32_t g_sta_since  = 0;      // 开始尝试连接 STA 的时刻
static uint32_t g_sta_ok_ms  = 0;      // 最近一次 STA 连上时刻（热点延迟关闭用）

// 单线程事件循环使用的工作缓冲区（避免占用栈空间）
static uint8_t g_ws_payload[B64_MAX + 2]; // WS 帧载荷（即 Base64 加密帧文本）
static char    g_json[PLAIN_MAX + 1];  // 解密后的明文 JSON
static char    g_pong[PLAIN_MAX + 1];  // 待发送的明文 JSON
static char    g_b64[B64_MAX];         // 待发送的 Base64 加密帧
static char    g_nonce[72];            // 从 ping 中提取的 nonce

static inline unsigned long uptime_s() { return millis() / 1000; }

// ---------------------------------------------------------------------------
// 日志上报：环形缓冲最近 LOG_CAP 条日志，随下一次心跳连接推送给 PC 落盘
// （ESP 自身不写文件，只留串口输出；PC 端收到后写入 ~/.auto-shutdown/logs/
//   下的 esp-日期.log，文件名“日期+类型”，与 PC 自身日志分开存放）
// ---------------------------------------------------------------------------

#define LOG_CAP 24                     // 缓冲条数（写满丢最旧）
struct LogEntry {
  uint32_t up;                         // 发生时刻（开机秒数）
  char     lv[6];                      // 级别: info / warn / error
  char     msg[96];                    // 内容（已做 JSON 转义）
};
static LogEntry  g_logbuf[LOG_CAP];
static uint8_t   g_log_head      = 0; // 最旧条目下标
static uint8_t   g_log_cnt       = 0; // 当前条数
static uint32_t  g_last_stat_ms  = 0; // 上次周期统计日志时刻
static uint32_t  g_last_badlog_ms = 0; // 上次非法报文日志时刻（限流，10 秒一条）

// 记录一条日志（lv: "info"/"warn"/"error"），同时输出到串口。
// 内容会做 JSON 转义，PC 端按 {"type":"log","up":..,"lv":..,"msg":..} 解析。
static void log_evt(const char *lv, const char *fmt, ...) {
  static char tmp[112];
  va_list ap;
  va_start(ap, fmt);
  vsnprintf(tmp, sizeof(tmp), fmt, ap);
  va_end(ap);

  LogEntry *e;
  if (g_log_cnt < LOG_CAP) {
    e = &g_logbuf[(g_log_head + g_log_cnt) % LOG_CAP];
    g_log_cnt++;
  } else {
    e = &g_logbuf[g_log_head];         // 缓冲已满：覆盖最旧一条
    g_log_head = (g_log_head + 1) % LOG_CAP;
  }
  e->up = uptime_s();
  snprintf(e->lv, sizeof(e->lv), "%s", lv);
  size_t o = 0;
  for (const char *p = tmp; *p && o < sizeof(e->msg) - 2; p++) {
    if (*p == '"' || *p == '\\') e->msg[o++] = '\\'; // JSON 转义
    e->msg[o++] = *p;
  }
  e->msg[o] = 0;
  Serial.printf("[%7lu][%-5s] %s\n", (unsigned long)e->up, lv, e->msg);
}


// 前向声明（.ino 不依赖 Arduino IDE 的自动原型生成，保证直接可编译）
static size_t b64_encode(const uint8_t *in, size_t n, char *out);
static bool   b64_decode(const char *in, size_t n, uint8_t *out, size_t cap, size_t *outlen);
static size_t seal(const char *json, char *out);
static void   udp_tick();
static void   sha256_buf(const void *data, size_t len, uint8_t out[32]);
static bool   authed();
static void   save_config();
static void   log_evt(const char *lv, const char *fmt, ...);
static void   flush_logs_to(WiFiClient &c);

// ---------------------------------------------------------------------------
// 持久化配置（EEPROM）：WiFi 账号密码 + 管理员账号
// ---------------------------------------------------------------------------

#define CFG_MAGIC 0x41534431u // "ASD1"

struct Config {
  uint32_t magic;
  char    ssid[33];            // STA SSID（空 = 未配置）
  char    pass[65];            // STA 密码（开放网络为空）
  char    admin_user[33];      // 管理员用户名
  uint8_t admin_pass_hash[32]; // 管理员密码的 SHA-256（不存明文）
};

static Config g_cfg;

static void load_config() {
  EEPROM.begin(sizeof(Config));
  EEPROM.get(0, g_cfg);
  if (g_cfg.magic != CFG_MAGIC) {
    // 首次启动：写入默认配置（管理员 admin / admin，登录后可在界面修改）
    memset(&g_cfg, 0, sizeof(g_cfg));
    g_cfg.magic = CFG_MAGIC;
    strcpy(g_cfg.admin_user, "admin");
    sha256_buf("admin", 5, g_cfg.admin_pass_hash);
    save_config();
  }
}

static void save_config() {
  EEPROM.put(0, g_cfg);
  EEPROM.commit();
}

// ---------------------------------------------------------------------------
// 加密原语（BearSSL）
// ---------------------------------------------------------------------------

static void sha256_buf(const void *data, size_t len, uint8_t out[32]) {
  br_sha256_context c;
  br_sha256_init(&c);
  br_sha256_update(&c, data, len);
  br_sha256_out(&c, out);
}

static void hmac_sha256(const uint8_t key[32], const void *data, size_t len, uint8_t out[32]) {
  br_hmac_key_context kc;
  br_hmac_context hc;
  br_hmac_key_init(&kc, &br_sha256_vtable, key, 32);
  br_hmac_init(&hc, &kc, 0);
  br_hmac_update(&hc, data, len);
  br_hmac_out(&hc, out);
}

// 常数时间比较（防时序侧信道）
static bool ct_eq(const uint8_t *a, const uint8_t *b, size_t n) {
  uint8_t diff = 0;
  for (size_t i = 0; i < n; i++) diff |= a[i] ^ b[i];
  return diff == 0;
}

// AES-256 单块加密：借用 CBC 加密器、IV 置零，等价于 ECB(E)
static void aes256_encrypt_block(const uint8_t in[16], uint8_t out[16]) {
  uint8_t iv[16] = {0};
  memcpy(out, in, 16);
  br_aes_big_cbcenc_run(&g_aes, iv, out, 16);
}

// AES-256-CTR：16 字节计数器大端整块递增（与 mbedtls / Python cryptography 一致）
static void aes256_ctr_xor(const uint8_t iv[16], const uint8_t *in, size_t len, uint8_t *out) {
  uint8_t ctr[16], ks[16];
  memcpy(ctr, iv, 16);
  for (size_t off = 0; off < len; off += 16) {
    size_t n = (len - off < 16) ? (len - off) : 16;
    aes256_encrypt_block(ctr, ks);
    for (size_t i = 0; i < n; i++) out[off + i] = in[off + i] ^ ks[i];
    for (int i = 15; i >= 0; i--) if (++ctr[i]) break; // 大端整块递增
  }
}

// ---------------------------------------------------------------------------
// 加密帧 seal / open（与 python/protocol.py 的 seal / open_frame 一致）
// ---------------------------------------------------------------------------

// 加密一条明文 JSON，把 Base64 帧写入 out（容量 >= B64_MAX），返回帧长度
static size_t seal(const char *json, char *out) {
  static uint8_t buf[FRAME_MAX]; // [0..32) MAC | [32..48) IV | [48..) CT
  size_t plen = strlen(json);
  if (plen > PLAIN_MAX) return 0;
  for (int i = 0; i < IV_LEN; i += 4) *(uint32_t *)(buf + MAC_LEN + i) = RANDOM_REG32;
  aes256_ctr_xor(buf + MAC_LEN, (const uint8_t *)json, plen, buf + MAC_LEN + IV_LEN);
  hmac_sha256(g_k_mac, buf + MAC_LEN, IV_LEN + plen, buf); // HMAC 覆盖 IV||CT
  return b64_encode(buf, MAC_LEN + IV_LEN + plen, out);
}

// 解开一条 Base64 帧，明文 JSON 写入 out_json（容量 >= PLAIN_MAX+1）。
// 任何校验/解密失败都返回 false，调用方直接丢弃即可。
static bool open_frame(const char *b64, char *out_json) {
  static uint8_t frame[FRAME_MAX];
  size_t flen;
  if (!b64_decode(b64, strlen(b64), frame, sizeof(frame), &flen)) return false;
  if (flen <= MAC_LEN + IV_LEN || flen > FRAME_MAX) return false;
  size_t ctlen = flen - MAC_LEN - IV_LEN;
  if (ctlen > PLAIN_MAX) return false;

  // encrypt-then-MAC：先比对 HMAC（常数时间），再解密
  static uint8_t mac[32];
  hmac_sha256(g_k_mac, frame + MAC_LEN, IV_LEN + ctlen, mac);
  if (!ct_eq(frame, mac, MAC_LEN)) return false;

  aes256_ctr_xor(frame + MAC_LEN, frame + MAC_LEN + IV_LEN, ctlen, (uint8_t *)out_json);
  out_json[ctlen] = 0;
  return true;
}

// ---------------------------------------------------------------------------
// 明文 JSON 的轻量解析（报文结构固定，无需引入 JSON 库）
// ---------------------------------------------------------------------------

static bool json_eq_type(const char *json, const char *type) {
  char pat[32];
  snprintf(pat, sizeof(pat), "\"type\":\"%s\"", type);
  return strstr(json, pat) != NULL;
}

static bool json_get_nonce(const char *json, char *out, size_t outlen) {
  const char *p = strstr(json, "\"nonce\":\"");
  if (!p) return false;
  p += 9;
  size_t i = 0;
  while (*p && *p != '"' && i + 1 < outlen) out[i++] = *p++;
  out[i] = 0;
  return i > 0;
}

// ---------------------------------------------------------------------------
// Base64 编解码（自实现，避免依赖差异）
// ---------------------------------------------------------------------------

static const char B64C[] = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

static size_t b64_encode(const uint8_t *in, size_t n, char *out) {
  size_t o = 0;
  for (size_t i = 0; i < n; i += 3) {
    uint32_t v = (uint32_t)in[i] << 16;
    if (i + 1 < n) v |= (uint32_t)in[i + 1] << 8;
    if (i + 2 < n) v |= in[i + 2];
    out[o++] = B64C[(v >> 18) & 63];
    out[o++] = B64C[(v >> 12) & 63];
    out[o++] = (i + 1 < n) ? B64C[(v >> 6) & 63] : '=';
    out[o++] = (i + 2 < n) ? B64C[v & 63] : '=';
  }
  out[o] = 0;
  return o;
}

static int8_t b64_val(char c) {
  if (c >= 'A' && c <= 'Z') return c - 'A';
  if (c >= 'a' && c <= 'z') return c - 'a' + 26;
  if (c >= '0' && c <= '9') return c - '0' + 52;
  if (c == '+') return 62;
  if (c == '/') return 63;
  return -1;
}

static bool b64_decode(const char *in, size_t n, uint8_t *out, size_t cap, size_t *outlen) {
  size_t o = 0;
  uint32_t v = 0;
  int bits = 0;
  for (size_t i = 0; i < n; i++) {
    char c = in[i];
    if (c == '\r' || c == '\n' || c == ' ' || c == '\t') continue;
    if (c == '=') break;
    int8_t x = b64_val(c);
    if (x < 0) return false;
    v = (v << 6) | (uint32_t)x;
    bits += 6;
    if (bits >= 8) {
      bits -= 8;
      if (o >= cap) return false;
      out[o++] = (uint8_t)(v >> bits);
    }
  }
  *outlen = o;
  return o > 0;
}

// ---------------------------------------------------------------------------
// WebSocket 服务端（8123）
// PC 端是短连接：连上 -> ping -> pong -> 关闭，约 3 秒一次
// ---------------------------------------------------------------------------

static const char *ci_strstr(const char *hay, const char *needle) {
  size_t nl = strlen(needle);
  for (; *hay; hay++) {
    size_t i = 0;
    while (i < nl && tolower((unsigned char)hay[i]) == tolower((unsigned char)needle[i])) i++;
    if (i == nl) return hay;
  }
  return NULL;
}

// Sec-WebSocket-Accept = Base64( SHA-1(key + GUID) )
static void ws_accept(const char *key, char *out /*>= 32*/) {
  char buf[104];
  snprintf(buf, sizeof(buf), "%s%s", key, WS_GUID);
  br_sha1_context c;
  uint8_t d[20];
  br_sha1_init(&c);
  br_sha1_update(&c, buf, strlen(buf));
  br_sha1_out(&c, d);
  b64_encode(d, 20, out);
}

// 精确读取 n 字节；等待期间顺带处理 UDP 发现报文，保持发现服务可用
static bool read_exact(WiFiClient &c, uint8_t *dst, size_t n, uint32_t timeout_ms) {
  size_t got = 0;
  uint32_t t0 = millis();
  while (got < n) {
    if (millis() - t0 > timeout_ms) return false;
    int v = c.read();
    if (v >= 0) dst[got++] = (uint8_t)v;
    else { udp_tick(); yield(); }
  }
  return true;
}

static bool send_ws_frame(WiFiClient &c, uint8_t opcode, const char *data, size_t len) {
  uint8_t h[4];
  h[0] = 0x80 | opcode; // FIN + opcode
  size_t hn;
  if (len < 126) { h[1] = (uint8_t)len; hn = 2; }
  else { h[1] = 126; h[2] = (uint8_t)(len >> 8); h[3] = (uint8_t)len; hn = 4; }
  if (c.write(h, hn) != hn) return false;
  return c.write((const uint8_t *)data, len) == len;
}

// 把积压日志逐条作为加密 WS 文本帧推给 PC（PC 在等到 pong 前会一直读取并落盘）。
// 发送失败则立即返回，剩余日志留在缓冲里等下一次连接。
static void flush_logs_to(WiFiClient &c) {
  static char ljson[PLAIN_MAX + 1];
  static char lb64[B64_MAX];
  while (g_log_cnt) {
    LogEntry *e = &g_logbuf[g_log_head];
    snprintf(ljson, sizeof(ljson),
             "{\"type\":\"log\",\"up\":%lu,\"lv\":\"%s\",\"msg\":\"%s\"}",
             (unsigned long)e->up, e->lv, e->msg);
    size_t n = seal(ljson, lb64);
    if (n == 0 || !send_ws_frame(c, 0x1, lb64, n)) return;
    g_log_head = (g_log_head + 1) % LOG_CAP;
    g_log_cnt--;
  }
}

// 读取一个客户端帧（PC 端发来的帧带掩码，这里解掉）
static bool read_ws_frame(WiFiClient &c, uint8_t *payload, size_t cap,
                          size_t *plen, uint8_t *opcode) {
  uint8_t h[2], mask[4];
  if (!read_exact(c, h, 2, 5000)) return false;
  *opcode = h[0] & 0x0F;
  bool masked = h[1] & 0x80;
  size_t len = h[1] & 0x7F;
  if (len == 126) {
    uint8_t e[2];
    if (!read_exact(c, e, 2, 1000)) return false;
    len = ((size_t)e[0] << 8) | e[1];
  } else if (len == 127) {
    return false; // 心跳报文很小，拒绝大帧
  }
  if (len > cap) return false;
  if (masked && !read_exact(c, mask, 4, 1000)) return false;
  if (len > 0 && !read_exact(c, payload, len, 2000)) return false;
  if (masked) for (size_t i = 0; i < len; i++) payload[i] ^= mask[i & 3];
  *plen = len;
  return true;
}

static void serve_ws_client(WiFiClient &c) {
  // ---- 1. 读 HTTP 升级握手请求（直到 \r\n\r\n） ----
  static char req[768];
  size_t rl = 0;
  uint32_t t0 = millis();
  for (;;) {
    int v = c.read();
    if (v >= 0) {
      if (rl < sizeof(req) - 1) req[rl++] = (char)v;
      t0 = millis();
    } else if (millis() - t0 > 5000) {
      c.stop(); return;
    }
    if (rl >= 4 && memcmp(req + rl - 4, "\r\n\r\n", 4) == 0) break;
    if (!c.connected() && !c.available()) { c.stop(); return; }
    yield();
  }
  req[rl] = 0;

  // ---- 2. 计算 Sec-WebSocket-Accept 并回复 101 ----
  const char *kp = ci_strstr(req, "sec-websocket-key:");
  if (!kp) { c.stop(); return; }
  kp += strlen("sec-websocket-key:");
  while (*kp == ' ' || *kp == '\t') kp++;
  char key[64];
  size_t ki = 0;
  while (*kp && *kp != '\r' && *kp != '\n' && ki + 1 < sizeof(key)) key[ki++] = *kp++;
  key[ki] = 0;
  if (ki == 0) { c.stop(); return; }

  char accept[32];
  ws_accept(key, accept);
  char resp[256];
  snprintf(resp, sizeof(resp),
           "HTTP/1.1 101 Switching Protocols\r\n"
           "Upgrade: websocket\r\n"
           "Connection: Upgrade\r\n"
           "Sec-WebSocket-Accept: %s\r\n\r\n",
           accept);
  c.print(resp);

  // ---- 3. 帧循环：解密 ping -> 回加密 pong ----
  for (;;) {
    size_t plen;
    uint8_t opcode;
    if (!read_ws_frame(c, g_ws_payload, sizeof(g_ws_payload) - 1, &plen, &opcode)) break;

    if (opcode == 0x8) { // close
      uint8_t rsp[2] = {0x88, 0x00};
      c.write(rsp, 2);
      break;
    }
    if (opcode == 0x9) { send_ws_frame(c, 0xA, (const char *)g_ws_payload, plen); continue; } // WS ping -> WS pong
    if (opcode != 0x1 || plen == 0) continue; // 只处理文本帧

    g_ws_payload[plen] = 0;
    if (!open_frame((const char *)g_ws_payload, g_json)) {
      g_bad_count++;
      // 非法报文限流记录（可能是干扰源，也可能是密钥不一致）
      if (millis() - g_last_badlog_ms > 10000) {
        g_last_badlog_ms = millis();
        log_evt("warn", "ws bad frame dropped (total=%lu)", (unsigned long)g_bad_count);
      }
      continue;
    }
    if (!json_eq_type(g_json, "ping")) continue;
    if (!json_get_nonce(g_json, g_nonce, sizeof(g_nonce))) continue;
    snprintf(g_pong, sizeof(g_pong),
             "{\"type\":\"pong\",\"nonce\":\"%s\",\"uptime_s\":%lu}",
             g_nonce, uptime_s());
    // 先推积压日志、再回 pong：PC 收到匹配的 pong 就会断开，
    // pong 之后的报文会来不及送达
    flush_logs_to(c);
    seal(g_pong, g_b64);
    if (send_ws_frame(c, 0x1, g_b64, strlen(g_b64))) {
      g_hb_count++;
      g_last_hb_ms = millis();
      snprintf(g_last_client, sizeof(g_last_client), "%s", c.remoteIP().toString().c_str());
      digitalWrite(LED_BUILTIN, LOW); // 板载 LED（低有效）：心跳亮一下
      Serial.printf("[hb] %lu  from %s\n", g_hb_count, g_last_client);
    }
  }
  c.stop();
}

// ---------------------------------------------------------------------------
// UDP 发现服务（8124）：收到 discover -> 向来源单播 announce
// ---------------------------------------------------------------------------

void udp_tick() {
  int n = g_udp.parsePacket();
  if (n <= 0) return;
  static char buf[400];
  int len = g_udp.read(buf, sizeof(buf) - 1);
  if (len <= 0) return;
  buf[len] = 0;

  if (!open_frame(buf, g_json)) return;      // 非本协议设备（解不开），静默忽略
  if (!json_eq_type(g_json, "discover")) return;
  snprintf(g_pong, sizeof(g_pong),
           "{\"type\":\"announce\",\"name\":\"%s\",\"ws_port\":%u,\"uptime_s\":%lu}",
           DEV_NAME, (unsigned)WS_PORT, uptime_s());
  seal(g_pong, g_b64);

  g_udp.beginPacket(g_udp.remoteIP(), g_udp.remotePort());
  g_udp.write((const uint8_t *)g_b64, strlen(g_b64));
  g_udp.endPacket();
}

// ---------------------------------------------------------------------------
// Web 管理界面（80）：htmx 单页界面
// 管理员登录 -> WiFi 配置 / 修改管理员密码 / 运行状态
// ---------------------------------------------------------------------------

// HTML 转义（SSID 等用户输入回显时使用）
static String html_escape(const char *s) {
  String o;
  for (; *s; s++) {
    switch (*s) {
      case '&':  o += "&amp;";  break;
      case '<':  o += "&lt;";   break;
      case '>':  o += "&gt;";   break;
      case '"':  o += "&quot;"; break;
      case '\'': o += "&#39;";  break;
      default:   o += *s;
    }
  }
  return o;
}

// 界面 HTML 与 htmx 运行时由 web/build.mjs 从 web/index.html 生成（htmx.min.h），
// 修改界面请编辑 web/index.html 后在 web/ 目录执行 npm run build。
// 页面骨架：PAGE_HEAD +（PAGE_LOGIN 或 PAGE_MAIN）+ PAGE_TAIL；
// PAGE_MAIN 中的 __SSID__ 占位符替换为当前 WiFi 名称。

// 整页 ~80KB（内联了全部样式与脚本），超出 ESP8266 的堆容量，
// 不能拼成 String 发送——用 chunked 响应模式从 PROGMEM 流式输出
static void send_page_open() {
  g_web.chunkedResponseModeStart_P(200, PSTR("text/html; charset=utf-8"));
  g_web.sendContent_P(PAGE_HEAD);
}

static void send_fragment(const String &html) {
  g_web.send(200, "text/html; charset=utf-8", html);
}

static void send_err_fragment(const char *msg) {
  send_fragment(String("<div class=\"alert alert-error py-2 text-sm\">⚠️ ") + msg + "</div>");
}

static void send_ok_fragment(const char *msg) {
  send_fragment(String("<div class=\"alert alert-success py-2 text-sm\">✅ ") + msg + "</div>");
}

// 会话校验：Cookie 中的令牌与登录时发放的一致
static bool authed() {
  if (g_session[0] == 0) return false;
  String cookie = g_web.header("Cookie");
  return cookie.indexOf(String("as_session=") + g_session) >= 0;
}

static void handle_root() {
  send_page_open();
  if (!authed()) {
    g_web.sendContent_P(PAGE_LOGIN);
    if (g_web.arg("err").toInt()) {
      g_web.sendContent_P(PSTR(
          "<div class=\"alert alert-error py-2 text-sm\">⚠️ 用户名或密码错误</div>"));
    }
  } else {
    // PAGE_MAIN 很小（~2KB），替换占位符后正常发送
    String body = FPSTR(PAGE_MAIN);
    body.replace("__SSID__", html_escape(g_cfg.ssid));
    body.replace("__USER__", html_escape(g_cfg.admin_user));
    g_web.sendContent(body);
  }
  g_web.sendContent_P(PAGE_TAIL);
  g_web.chunkedResponseFinalize();
}

static void gen_token(char out[17]) {
  for (int i = 0; i < 2; i++) {
    snprintf(out + i * 8, 9, "%08lx", (unsigned long)RANDOM_REG32);
  }
  out[16] = 0;
}

static void handle_login() {
  String user = g_web.arg("user");
  String pass = g_web.arg("pass");
  uint8_t h[32];
  sha256_buf(pass.c_str(), pass.length(), h);
  bool ok = (user.length() < sizeof(g_cfg.admin_user)) &&
            (user == g_cfg.admin_user) &&
            ct_eq(h, g_cfg.admin_pass_hash, 32);
  if (!ok) {
    delay(300); // 简单的暴力破解抑制
    log_evt("warn", "login failed, user=%s", user.c_str());
    g_web.sendHeader("Location", "/?err=1");
    g_web.send(303, "text/plain", "");
    return;
  }
  gen_token(g_session);
  g_web.sendHeader("Set-Cookie", String("as_session=") + g_session + "; Path=/; HttpOnly");
  // 带 postlogin=1 跳转：main.js 检测到后再加载一次干净的 "/"，
  // 绕开"登录跳转直接到达的页面在配网窗口里无法滚动"的问题
  g_web.sendHeader("Location", "/?postlogin=1");
  g_web.send(303, "text/plain", "");
}

static void handle_logout() {
  g_session[0] = 0;
  g_web.sendHeader("Set-Cookie", "as_session=; Path=/; Max-Age=0");
  g_web.sendHeader("Location", "/");
  g_web.send(303, "text/plain", "");
}

static void handle_status() {
  if (!authed()) { send_err_fragment("请先登录"); return; }
  bool conn = WiFi.status() == WL_CONNECTED;
  String s;
  if (!g_cfg.ssid[0]) {
    s += "<div class=\"alert alert-warning py-2 text-sm\">WiFi 未配置——请在下方扫描选择或手动输入网络。</div>";
  } else if (conn) {
    s += "<div class=\"flex items-center gap-2\"><span class=\"badge badge-success badge-sm\">已连接</span>"
         "<b class=\"text-sm\">" + html_escape(g_cfg.ssid) + "</b></div>";
    s += "<p class=\"text-sm\">设备 IP：<code class=\"bg-base-200 px-1 rounded\">" + WiFi.localIP().toString() +
         "</code><span class=\"opacity-60\">（PC 端心跳地址＝该 IP，端口 8123）</span></p>";
  } else {
    s += "<div class=\"flex items-center gap-2\"><span class=\"badge badge-error badge-sm\">连接中…</span>"
         "<span class=\"text-sm\">" + html_escape(g_cfg.ssid) + "（密码错误或信号太弱？请重新扫描）</span></div>";
  }
  if (g_ap_active) {
    s += "<div class=\"alert alert-info py-2 text-sm\">📡 配置热点开启中：手机/电脑连接热点后会自动弹出配置页"
         "（或访问 http://192.168.4.1）</div>";
  }
  s += "<p class=\"text-xs opacity-60\">❤️ 心跳 " + String(g_hb_count) + " 次 · 丢弃非法报文 " + String(g_bad_count) +
       " 条 · 最近来源 " + g_last_client + " · 已运行 " + String(uptime_s()) + " 秒</p>";
  send_fragment(s);
}

static void handle_wifi() {
  if (!authed()) { send_err_fragment("请先登录"); return; }
  String ssid = g_web.arg("ssid");
  String pass = g_web.arg("pass");
  if (ssid.length() == 0 || ssid.length() >= sizeof(g_cfg.ssid) || pass.length() >= sizeof(g_cfg.pass)) {
    send_err_fragment("WiFi 名称或密码长度不合法");
    return;
  }
  memset(g_cfg.ssid, 0, sizeof(g_cfg.ssid));
  memset(g_cfg.pass, 0, sizeof(g_cfg.pass));
  ssid.toCharArray(g_cfg.ssid, sizeof(g_cfg.ssid));
  pass.toCharArray(g_cfg.pass, sizeof(g_cfg.pass));
  save_config();
  log_evt("info", "wifi config saved, ssid=%s", g_cfg.ssid);

  // 立即尝试连接（保持当前 STA/AP 模式组合）
  WiFi.mode(g_ap_active ? WIFI_AP_STA : WIFI_STA);
  if (pass.length()) WiFi.begin(g_cfg.ssid, g_cfg.pass);
  else               WiFi.begin(g_cfg.ssid);
  g_sta_since = millis();

  String s = "<div class=\"alert alert-success py-2 text-sm\">已保存，正在连接 ";
  s += html_escape(g_cfg.ssid);
  s += " …</div><div class=\"alert py-2 text-sm\">连接成功后配置热点会自动关闭，之后用<b>设备的新 IP</b>访问本页面；"
       "PC 端心跳地址＝设备 IP:8123。</div>";
  send_fragment(s);
}

static void handle_password() {
  if (!authed()) { send_err_fragment("请先登录"); return; }
  String oldp = g_web.arg("old");
  String newp = g_web.arg("pass");
  uint8_t h[32];
  sha256_buf(oldp.c_str(), oldp.length(), h);
  if (!ct_eq(h, g_cfg.admin_pass_hash, 32)) { send_err_fragment("当前密码不正确"); return; }
  if (newp.length() < 8) { send_err_fragment("新密码至少 8 位"); return; }
  sha256_buf(newp.c_str(), newp.length(), g_cfg.admin_pass_hash);
  save_config();
  log_evt("info", "admin password changed");
  g_session[0] = 0; // 修改成功后自动登出
  g_web.sendHeader("HX-Redirect", "/"); // htmx 收到后整页跳转回登录页
  send_ok_fragment("密码已修改，正在返回登录页…");
}

// ---------------------------------------------------------------------------
// WiFi 扫描（供管理界面选择网络，免手动输入 SSID）
// ---------------------------------------------------------------------------

static void handle_scan() {
  if (!authed()) { send_err_fragment("请先登录"); return; }
  if (WiFi.getMode() & WIFI_AP && !(WiFi.getMode() & WIFI_STA)) {
    WiFi.mode(WIFI_AP_STA); // 仅热点模式时借 STA 通道扫描
  }
  Serial.printf("[scan] start, mode=%d\n", WiFi.getMode());

  // 同步扫描（约 2-3 秒，官方 ScanNetworks 示例的推荐做法）。
  // 模式刚切换时 SDK 可能暂时拒绝扫描，失败则稍候重试一次。
  int n = WiFi.scanNetworks(false, true);
  if (n == WIFI_SCAN_FAILED) {
    delay(250);
    n = WiFi.scanNetworks(false, true);
  }
  Serial.printf("[scan] done, found %d\n", n);

  if (n < 0) {
    send_fragment("<div class=\"alert alert-error py-2 text-sm\">扫描启动失败（错误码 " + String(n) +
                  "），请重试</div>"
                  "<button type=\"button\" class=\"btn btn-outline btn-sm\" "
                  "hx-get=\"/scan\" hx-target=\"#scan-area\" hx-swap=\"innerHTML\">重试</button>");
    return;
  }
  if (n == 0) {
    send_fragment("<div class=\"alert alert-warning py-2 text-sm\">没有扫描到任何网络，请检查现场 WiFi 环境</div>"
                  "<button type=\"button\" class=\"btn btn-outline btn-sm\" "
                  "hx-get=\"/scan\" hx-target=\"#scan-area\" hx-swap=\"innerHTML\">🔄 重新扫描</button>");
    WiFi.scanDelete();
    return;
  }

  // 结果渲染为下拉列表（结果已按信号强度排序）；隐藏网络不可选，提示用手动输入
  String s = "<select id=\"wifi-select\" class=\"select w-full\">"
             "<option value=\"\" disabled selected>发现 " + String(n) + " 个网络，点选一个…</option>";
  for (int i = 0; i < n && i < 30; i++) {
    String ssid = WiFi.SSID(i);
    int rssi = WiFi.RSSI(i);
    bool open = WiFi.encryptionType(i) == ENC_TYPE_NONE;
    if (ssid.length() == 0) {
      s += "<option value=\"\" disabled>（隐藏网络）· " + String(rssi) + " dBm · 请切到\"手动输入\"</option>";
    } else {
      s += "<option value=\"" + html_escape(ssid.c_str()) + "\">" + html_escape(ssid.c_str()) +
           " · " + String(rssi) + " dBm · " + (open ? "开放" : "加密") + "</option>";
    }
  }
  s += "</select>"
       "<button type=\"button\" class=\"btn btn-outline btn-sm mt-2\" "
       "hx-get=\"/scan\" hx-target=\"#scan-area\" hx-swap=\"innerHTML\">🔄 重新扫描</button>";
  WiFi.scanDelete(); // 释放扫描结果占用的内存
  send_fragment(s);
}

static void web_setup() {
  g_web.collectHeaders("Cookie");
  g_web.on("/", HTTP_GET, handle_root);
  g_web.on("/login", HTTP_POST, handle_login);
  g_web.on("/logout", HTTP_POST, handle_logout);
  g_web.on("/status", HTTP_GET, handle_status);
  g_web.on("/wifi", HTTP_POST, handle_wifi);
  g_web.on("/password", HTTP_POST, handle_password);
  g_web.on("/scan", HTTP_GET, handle_scan);
  // 界面为单文件 HTML（CSS/JS 全部内联），仅需返回 "/" 与 htmx 片段
  // 未知路径全部重定向：热点模式下即 Captive Portal——手机/电脑连上热点后，
  // 系统的联网探测请求（如 connectivitycheck.gstatic.com、captive.apple.com、
  // msftconnecttest.com）会被 DNS 劫持到这里，返回 302 后系统就会弹出
  // "此 WiFi 需要登录 / 登录到此网络" 的提示，点击直接打开配置页。
  g_web.onNotFound([]() {
    if (g_ap_active) {
      g_web.sendHeader("Location", String("http://") + WiFi.softAPIP().toString() + "/", true);
      g_web.send(302, "text/plain", "");
      return;
    }
    g_web.sendHeader("Location", "/", true);
    g_web.send(303, "text/plain", "");
  });
  g_web.begin(WEB_PORT);
}

// ---------------------------------------------------------------------------
// 网络模式管理
// ---------------------------------------------------------------------------

static void start_ap() {
  if (g_ap_active) return;
  char ap_ssid[32];
  snprintf(ap_ssid, sizeof(ap_ssid), "auto-shutdown-%06X", ESP.getChipId());
  WiFi.mode(g_cfg.ssid[0] ? WIFI_AP_STA : WIFI_AP);
  WiFi.softAP(ap_ssid); // 开放网络：配置操作由管理员登录保护
  g_dns.start(53, "*", WiFi.softAPIP()); // 劫持所有域名解析 -> Captive Portal
  g_ap_active = true;
  log_evt("warn", "config AP started: %s", ap_ssid);
  Serial.printf("[ap] %s -> http://%s\n", ap_ssid, WiFi.softAPIP().toString().c_str());
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

void setup() {
  Serial.begin(115200);
  Serial.println();
  Serial.println("auto-shutdown heartbeat server (ESP8266)");
  // 复位原因随首条日志上报：PC 端可据此判断设备是否发生过意外重启
  log_evt("info", "boot, reset reason: %s", ESP.getResetReason().c_str());

  pinMode(LED_BUILTIN, OUTPUT);
  digitalWrite(LED_BUILTIN, HIGH); // 板载 LED 低有效，先熄灭

  load_config();

  // 派生密钥：k_enc = SHA256(PASSPHRASE ":enc")，k_mac = SHA256(PASSPHRASE ":mac")
  char tmp[80];
  snprintf(tmp, sizeof(tmp), "%s:enc", PASSPHRASE);
  sha256_buf(tmp, strlen(tmp), g_k_enc);
  snprintf(tmp, sizeof(tmp), "%s:mac", PASSPHRASE);
  sha256_buf(tmp, strlen(tmp), g_k_mac);
  br_aes_big_cbcenc_init(&g_aes, g_k_enc, 32); // 32 字节密钥 -> AES-256

  // 连 WiFi（未配置时由 start_ap 开启配置热点；连不上 30 秒后也会开热点兜底）
  if (g_cfg.ssid[0]) {
    WiFi.mode(WIFI_STA);
    WiFi.setSleepMode(WIFI_NONE_SLEEP); // 服务端响应更快
    if (g_cfg.pass[0]) WiFi.begin(g_cfg.ssid, g_cfg.pass);
    else               WiFi.begin(g_cfg.ssid);
    g_sta_since = millis();
    Serial.print("WiFi connecting");
  } else {
    Serial.println("WiFi not configured");
    start_ap();
  }

  g_ws_server.begin();
  g_ws_server.setNoDelay(true);
  g_udp.begin(UDP_PORT);
  web_setup();

  Serial.printf("Web %u, WS %u, UDP %u, name \"%s\"\n",
                (unsigned)WEB_PORT, (unsigned)WS_PORT, (unsigned)UDP_PORT, DEV_NAME);
}

void loop() {
  g_web.handleClient();
  if (g_ap_active) g_dns.processNextRequest();

  bool conn = WiFi.status() == WL_CONNECTED;
  static bool was_conn = false;
  if (conn && !was_conn) {
    g_sta_ok_ms = millis();
    log_evt("info", "wifi connected, ip=%s", WiFi.localIP().toString().c_str());
  }
  if (!conn && was_conn) {
    log_evt("warn", "wifi disconnected");
  }
  was_conn = conn;

  if (g_cfg.ssid[0]) {
    if (conn) {
      // 连接稳定 10 秒后关掉配置热点
      if (g_ap_active && millis() - g_sta_ok_ms > 10000) {
        g_dns.stop();
        WiFi.softAPdisconnect(true);
        WiFi.mode(WIFI_STA);
        g_ap_active = false;
        log_evt("info", "config AP closed");
        Serial.println("[ap] closed");
      }
      WiFiClient c = g_ws_server.available();
      if (c) serve_ws_client(c);
    } else if (!g_ap_active && millis() - g_sta_since > 30000) {
      start_ap(); // 连不上（如改了路由器密码）：开热点兜底供重新配置
    }
  }

  udp_tick();

  // 周期性运行统计（5 分钟一条）：让 PC 端日志里有设备的存活轨迹
  if (conn && millis() - g_last_stat_ms > 300000) {
    g_last_stat_ms = millis();
    log_evt("info", "stats: hb=%lu bad=%lu heap=%u", (unsigned long)g_hb_count,
            (unsigned long)g_bad_count, (unsigned)ESP.getFreeHeap());
  }

  // 板载 LED：未联网闪烁；联网后每次心跳亮一下
  static uint32_t last_led_ms = 0;
  if (!conn && millis() - last_led_ms > 500) {
    last_led_ms = millis();
    digitalWrite(LED_BUILTIN, !digitalRead(LED_BUILTIN));
  } else if (conn && millis() - g_last_hb_ms > 120) {
    digitalWrite(LED_BUILTIN, HIGH);
  }
}
