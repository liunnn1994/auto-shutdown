# ESP8266 心跳服务端（NodeMCU 实机实现）

`ESP8266.ino` 是 `python/heartbeat_server.py` 的 ESP8266 移植版，加密协议与
`python/protocol.py` / `src/crypto.rs` 完全一致。烧录后它就是 README 架构图里
插在市电插座上的那台"心跳设备"：市电一断它失联，PC 端就会触发关机倒计时。

## 硬件

只需一块 **NodeMCU ESP8266 开发板**（ESP-12E 模组 + CH340 USB 转串口 +
Micro-USB 线 + 5V/1A USB 电源适配器），无其他配件。板载 LED 用作状态指示。

## 功能

| 端口 | 服务 | 说明 |
|---|---|---|
| 80 | Web 管理界面（htmx） | 管理员登录后配置 WiFi / 修改密码 / 查看状态 |
| 8123 | WebSocket 心跳 | 收到加密 `ping` → 回加密 `pong` |
| 8124 | UDP 发现 | 收到加密 `discover` → 单播加密 `announce` |

- **首次使用零工具配置**：没有 WiFi 配置时，设备自动开启配置热点
  `auto-shutdown-xxxxxx`（无密码）。手机/电脑连上后会弹出"此 WiFi 需要登录"
  提示（Captive Portal），点击即打开配置页；没有弹窗时浏览器访问
  `http://192.168.4.1` 也可以。用默认管理员 **admin / admin** 登录；
- **WiFi 扫描列表**：登录后点"扫描附近 WiFi"，从按信号强度排序的列表里
  直接点选网络（显示信号强度和加密/开放标识），输入密码即可，不用手打
  SSID；隐藏网络仍可手动输入；
- WiFi 配置保存在 EEPROM，掉电不丢；管理员密码只存 SHA-256，不存明文；
- WiFi 连不上（如改了路由器密码）超过 30 秒，设备会重新开启配置热点兜底；
- 配置热点在 WiFi 连接稳定 10 秒后自动关闭；
- 已配置的 WiFi 会无限自动重试：启用底层自动重连，未联网每 30 秒主动
  重新发起连接，每 120 秒临时停用并重新启用 STA 连接接口。不会清除
  账号密码、不会重启开发板；开启配置热点也不会停止重试。只有在网页
  保存新的 WiFi 配置时才更换连接目标。30 秒热点等待时间从本次掉线开始计算；
- 板载 LED：未联网时闪烁，联网后每次心跳亮一下；
- **日志上报**：启动（含复位原因）、WiFi 掉线/重连、配置热点开关、配置变更、
  非法报文（限流）、5 分钟一条的存活统计等事件记入内存环形缓冲（最近 24 条），
  随下一次心跳连接推送给 PC，由 PC 端写入 `~/.auto-shutdown/logs/esp-日期.log`
  （与 PC 自身日志 `app-日期.log` 分开存放，均保留 30 天）。
  WiFi 诊断日志还包含断开原因码 `reason`、连接状态 `status`、重试/接口
  重置次数、持续离线秒数和可用堆内存；恢复时上报本次离线汇总。
  日志仅在内存中缓存最近 24 条，长时间离线会覆盖旧条目，拔插断电会丢失
  尚未上报的日志。下次故障如能等待自动恢复，可保留更多诊断信息；
- `wifi-recovery-2` 固件对低内存时的心跳连接使用 `abort()` 释放 TCP 资源，
  对端断开后立即结束读取等待，并限制握手/会话等待时间；各网络模式切换后
  都禁用 WiFi 省电。日志新增 RSSI、信道、AP 的 BSSID、首次掉线原因、
  累计找不到 AP/信标超时次数，以及最小堆内存、最大空闲块、碎片率和
  TCP 强制释放次数。重复 WiFi 事件最多每 30 秒汇总一次，避免冲掉关键日志。
  异常重启时上报异常原因和指令地址；定位地址需要保留烧录版本对应的 `.elf`。
  这些改动需要实机长时间运行验证，不能据编译成功认定无线掉线或崩溃已解决。

## 文件说明

```
ESP8266/                    <-- Arduino IDE 需要打开的是这个文件夹
├── ESP8266.ino             <-- 固件主程序（必须上传）
├── htmx_min.h              <-- Web 界面数据（必须上传，随固件一起编译进板子）
├── README.md               本文档（不需要上传）
└── web/                    Vite 项目，仅修改界面时才需要，上传用不到：
    ├── index.html          界面唯一源码（Vite 入口，Tailwind v4 + daisyUI 5）
    ├── vite.config.mjs     Vite 配置（单文件内联 + 生成 ../htmx_min.h）
    ├── src/
    │   ├── main.js         JS 入口（引入 htmx、样式、页面行为）
    │   └── style.css       样式入口（Tailwind/daisyUI + 固件片段类安全清单）
    ├── package.json        依赖清单（vite / tailwindcss / daisyui / htmx.org）
    └── dist/               构建中间产物（自动清理，gitignore）
```

要点：

- 上传的是**整个 `ESP8266/` 文件夹**（Arduino IDE 要求文件夹名与 `.ino`
  文件名一致，不要单独拷贝 `.ino` 文件，否则会缺少 `htmx_min.h` 导致
  编译失败"htmx_min.h: No such file or directory"）；
- `ESP8266.ino` 和 `htmx_min.h` 两个文件**一起编译**后生成一个固件烧进
  板子， Arduino IDE 打开 sketch 时会自动以标签页形式同时加载它们；
- `web/` 目录只在你需要修改管理界面外观时才用到（见下文"修改 Web
  界面"），日常上传与它无关；
- 仓库拉取后 `htmx_min.h` 已存在，可直接上传；只有改过 `web/index.html`
  才需要重新 `pnpm build` 生成它。

## 环境准备

1. Arduino IDE 2.x；
2. 开发板管理器安装 **esp8266** 核心包（3.x，由 Espressif Systems 提供）。

无其他依赖：加密用核心自带的 BearSSL（SHA-256 / HMAC-SHA256 / AES-256），
Web 界面用核心自带的 ESP8266WebServer，htmx 运行时已嵌入固件，全部开箱即用。

### 修改 Web 界面（可选）

Web 界面用 **Vite 8** 构建（[`web/`](web/) 目录）：Tailwind v4 + daisyUI 5
样式、htmx 均为 npm 依赖，构建时全部 tree-shaking / 按需生成，最终内联成
**单文件 HTML**，再由 `vite.config.mjs` 里的自定义插件按 `<!-- @段落 -->
` 标记（head / login / main / tail）切成片段、生成固件引用的
`../htmx_min.h`。只有页面和固件片段实际用到的类会进固件（约 100KB；
全量包 1.4MB 是塞不进 ESP8266 的，不要用 CDN 版的 daisyui.min.css /
tailwind 运行时 JS）。

```bash
cd ESP8266/web
pnpm install
pnpm build      # Vite 构建：内联单文件 HTML -> 生成 ../htmx_min.h
pnpm dev        # 本地预览界面布局（无固件后端，数据请求会失败）
```

生成后重新用 Arduino IDE 上传固件即可生效；`htmx_min.h` 为自动生成文件，
请勿手改。要点：

- 界面只改 `web/index.html`（分段标记 `<!-- @head -->` 等勿删，切分靠它们，
  缺失时构建会直接报错）；页面行为在 `web/src/main.js`，样式入口在
  `web/src/style.css`；
- 固件 .ino 里的 htmx 片段（状态卡片 / 扫描列表 / 提示）所用的 daisyUI 类
  列在 `web/src/style.css` 的 `@source inline("...")` 安全清单里——在固件
  片段里新增样式类时记得同步加进去，否则会被 Tailwind 按需构建裁掉；
- htmx 升级：`pnpm install htmx.org@最新` 后重新 build，无需替换任何文件。

## 烧录步骤

1. USB 线连接开发板；
2. Arduino IDE 打开 `ESP8266/ESP8266.ino`（整个文件夹，会同时看到
   `htmx_min.h` 标签页）；
3. 工具菜单确认三项：
   - 开发板：`NodeMCU 1.0 (ESP-12E Module)`
   - 端口：板子插上后出现的 COM 口（如 `COM3`）
   - Erase Flash：保持默认 `Only Sketch`（选 `All Flash Contents` 会清空
     已保存的 WiFi 和管理员密码，用于重置）
4. 点左上角"上传"（→），IDE 自动完成编译 + 烧录（会覆盖板子上原有固件，
   这是正常流程；全程约 1-2 分钟）；
5. 上传失败（`Timed out waiting for packet header`）时：按住板上 **FLASH**
   键，按一下 **RST** 复位，松开 FLASH，再点上传；
6. 上传完成，板子自动重启，按下一节验证。

无需修改任何代码：WiFi 账号密码和管理员密码都不写在代码里，烧录后按
"首次使用"流程在网页上配置。

## 验证

1. 上传后板载 LED 闪烁（未配置 WiFi），手机/电脑连接热点
   `auto-shutdown-xxxxxx`，浏览器打开 `http://192.168.4.1`；
2. 用 admin / admin 登录，填入 WiFi 账号密码，点"保存并连接"；状态卡片
   显示"已连接"和设备 IP 后热点自动关闭；
3. 启动 PC 端（`cargo run`）：自动发现应能扫到该设备（或手动输入
   `设备IP:8123` 后点"测试"）；
4. 心跳正常时：串口监视器（115200）每 3 秒打印一条 `[hb] N from 192.168.x.x`，
   Web 页面心跳计数持续增长；
5. 测试整条链路：直接拔掉设备电源，PC 端应在 180 秒后弹出关机倒计时；
   重新上电后 PC 端自动恢复正常。

## 协议对照（移植说明）

| python/protocol.py | ESP8266.ino |
|---|---|
| `hashlib.sha256(PASSPHRASE + ":enc"/":mac")` | `sha256_buf()`（br_sha256） |
| `_ctr_xor`（AES-256-CTR） | `aes256_ctr_xor()`（br_aes_big 单块加密 + 16 字节大端计数器递增） |
| `hmac.new(...)` / `compare_digest` | `hmac_sha256()`（br_hmac）+ `ct_eq()` 常数时间比对 |
| `seal` / `open_frame`（encrypt-then-MAC） | 同名逻辑（先验 HMAC 再解密，失败丢弃） |
| `websockets.serve(8123)` | `WiFiServer(8123)` + 手写 WS 握手（SHA-1 + Base64）/帧解析 |
| UDP 8124 discover → announce | `WiFiUDP(8124)` |

IV 使用硬件随机数 `RANDOM_REG32`；时间参数（心跳 3 秒 / 丢失 180 秒）由 PC 端
决定，设备端无需关心。

## 常见问题

- **配置热点为什么是 192.168.4.1？** 这是 ESP8266 软热点的出厂默认地址
  （每次开启热点都固定是这个）。正常情况下不需要记它——连接热点后系统会
  自动弹出"此 WiFi 需要登录"，点击即进入配置页（原理：设备在热点模式下
  劫持 DNS，把系统的联网探测域名 302 重定向到配置页）。连接上正常 WiFi 后
  管理界面走设备的局域网 IP，不再与 192.168.4.1 有关。
- **弹窗没出现？** 部分系统关闭了 Captive Portal 检测，或弹窗被网络选择
  窗口挡住；直接在浏览器输入 `192.168.4.1` 即可。
- **自动弹出的配网窗口里页面不能滚动？** 这是部分系统版本（如 vivo
  OriginOS）配网窗口自身的缺陷——它会在原生层拦截滚动手势（下拉还会
  触发整页刷新）。页面已经做了内滚容器与手势反制的兼容处理，若仍遇到，
  不用配网窗口：手机保持连接设备热点，**用浏览器（Chrome/Edge 等）打开
  `http://192.168.4.1`** 即可，浏览器里一切正常。
- **PC 端扫描不到设备？** 确认设备与 PC 在同一网段、路由器无 AP 隔离；也可
  手动输入设备 IP 后用"测试"按钮验证。
- **PC 端报"HMAC 校验失败"？** 两端口令不一致：同步修改 `src/protocol.rs`
  （Rust）、`python/protocol.py`（Python）和 `ESP8266.ino` 中的 `PASSPHRASE`。
- **忘了管理员密码 / 想重置 WiFi？** 重新烧录一次，并在"工具 → Erase
  Flash"中选择 `All Flash Contents`，即可恢复默认配置（admin / admin，
  WiFi 未配置）。
- **想改热点名 / 加热点密码？** 修改 `start_ap()` 中的 `WiFi.softAP()` 调用。
