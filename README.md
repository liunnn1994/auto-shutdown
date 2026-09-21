# auto-shutdown —— 自动关机守护（心跳失联自动关机）

本软件解决的问题是：**当一台 PC 依赖的某个"常驻设备"离线时，自动把 PC 关机**。
最典型的场景是：你有一块插在市电插座上的 ESP32（或其他任何能跑 socket 服务的
设备），市电一断它就会失联 —— PC 发现心跳丢失，就知道市电断了，抢在硬断电
之前安全关机。任何 socket 服务（树莓派、路由器、NAS……）都可以扮演这个角色。

```
┌────────────┐                        ┌──────────────┐
│ 心跳服务端  │◄──── 有电/在线就活着 ───│  市电/网络    │
└─────┬──────┘                        └──────────────┘
      │ ws://<ip>:8123  （加密心跳，3 秒一次）
┌─────┴────────┐
│  PC 本软件    │  心跳丢失 > 60s ⇒ 弹出 60s 倒计时 ⇒ 关机
└──────────────┘
```

## 功能

- **托盘常驻**：窗口的关闭 / 最小化都会隐藏到托盘（任务栏不占位）；
  双击托盘图标或右键菜单"显示主窗口"可恢复，右键菜单"退出程序"彻底退出。
- **开机启动**：右键托盘菜单勾选"开机启动"，或在主界面打开"开机启动"开关
  （开启后托盘菜单项前有 √）。通过**任务计划程序**注册一条"用户登录时启动"
  的计划任务实现，无需管理员权限，也可在 Windows"任务计划程序"面板中查看。
- **无需管理员权限**：全程普通用户权限即可运行，启动不弹 UAC。
- **自动发现**：首次运行自动 UDP 广播扫描局域网内的心跳服务端；扫不到则手动
  输入服务地址，"测试"按钮做一次完整的心跳握手并显示结果（失败时输出完整错误）。
- **加密心跳**：所有报文经过 AES-256-CTR 加密 + HMAC-SHA256 校验，
  局域网内不持有口令的设备既无法伪造应答也无法干扰。
- **关机倒计时**：心跳丢失超过 60 秒后弹出**置顶**弹窗，60 秒倒计时实时刷新：
  - **立即关机**：马上执行关机；
  - **取消关机**：本次不再提醒，直到心跳恢复后重新开始监测（再次失联超 60 秒才会再弹）；
  - **稍后关机 (30s)**：关闭弹窗，30 秒后再次弹出完整倒计时；
  - 倒计时期间服务恢复（心跳恢复）→ 弹窗自动关闭，一切如常。

> **防误报**：只有"成功连通过至少一次"之后才会开始判定丢失。所以服务端还没
> 部署好时，软件只会安静地等待，绝不会误关机。

## 快速开始

### 1. 编译 PC 端

```bash
cargo build --release
# 产物: target/release/auto-shutdown.exe，或在主界面/托盘菜单打开"开机启动"
```

### 2. 先用 Python 模拟服务端联调（以 ESP32 为例）

```bash
pip install -r requirements.txt
python heartbeat_server.py                 # 正常运行
python heartbeat_server.py --outage 30     # 启动 30 秒后"模拟断电"60 秒，用于测试关机倒计时
```

PC 端启动后会自动扫描到本机运行的模拟服务端（同机广播即可收到）；
也可以手动输入 `127.0.0.1:8123` 后点"测试"。

### 3. 部署到真实设备

**ESP8266（NodeMCU）**：仓库已附带可直接烧录的完整实现，见
[`ESP8266/README.md`](ESP8266/README.md)（仅需一块 NodeMCU ESP8266 开发板；
首次上电自动开配置热点，浏览器登录管理员账号即可配置 WiFi，带 htmx Web
管理界面，配置掉电保存）。

**其他设备（如 ESP32）**：`python/heartbeat_server.py` 就是服务端需要实现的
全部行为的参考实现：

1. WebSocket 服务端（端口 8123）：收到加密 `ping` → 回加密 `pong`（原样带回 nonce）；
2. UDP 服务端（端口 8124）：收到加密 `discover` → 向来源单播加密 `announce`；
3. 加解密直接照抄 `python/protocol.py`，ESP32 上对应 mbedtls 的
   AES-256-CTR / HMAC-SHA256 / SHA-256，密钥派生方式见下文协议一节。

## 开发

常用命令（都在项目根目录执行）：

```bash
# 调试运行（直接启动 GUI，配合测试服务端联调）
cargo run

# 运行单元测试（含 Rust↔Python 加密互验测试）
cargo test

# 静态检查 / 自动修复
cargo clippy
cargo clippy --fix

# 发布构建（产物 target/release/auto-shutdown.exe）
cargo build --release
```

联调流程：先启动测试服务端，再 `cargo run` 启动 PC 端：

```bash
cd python
python heartbeat_server.py            # 终端 1：测试服务端（可用 --outage 30 模拟断电）
cargo run                             # 终端 2：PC 端
```

首次运行会自动扫描局域网（同机广播也能收到）；也可以在界面里手动输入
`127.0.0.1:8123` 后点"测试"。注意：**测试"取消关机"以外的关机流程时请
提前保存好工作内容**——倒计时归零会真的执行关机。

### 加密协议改动

Rust（`src/crypto.rs` + `src/protocol.rs`）与 Python（`python/protocol.py`）
两端必须保持一致。修改加密实现或 `PASSPHRASE` 后：

1. 运行 `python python/protocol.py`，它会用固定 IV 打印一条加密帧；
2. 把打印的帧更新到 `src/crypto.rs` 的 `python_frame_opens_in_rust` 测试中；
3. `cargo test` 通过即代表两端协议仍然互通。

## 协议细节（实现服务端时照此对齐）

### 加密帧（WebSocket 文本帧 / UDP 数据报，均为 Base64 文本）

```
Base64( HMAC-SHA256(k_mac, IV || CT)[32B] || IV[16B] || CT )
```

- `IV`：每条报文随机生成 16 字节；
- `CT`：AES-256-CTR(k_enc, IV, 明文 JSON)，CTR 计数器为 16 字节大端整块递增
  （mbedtls / Python cryptography 的默认行为）；
- `k_enc = SHA-256("auto-shutdown-v1:enc")`，`k_mac = SHA-256("auto-shutdown-v1:mac")`。
  两端口令必须一致：PC 端在 `src/protocol.rs` 的 `PASSPHRASE`，Python 端在
  `python/protocol.py` 的 `PASSPHRASE`（更换时两端同步修改）。

校验顺序是 encrypt-then-MAC：先比对 HMAC（常数时间比较）再解密，任何失败直接丢弃。

### 报文（明文 JSON）

| 方向 | 报文 | 说明 |
|---|---|---|
| PC → 服务端（WS） | `{"type":"ping","ts":1700000000000,"nonce":"<hex>"}` | 心跳，3 秒一次 |
| 服务端 → PC（WS） | `{"type":"pong","nonce":"<同 ping>","uptime_s":123}` | 原样带回 nonce |
| PC → 局域网（UDP 8124 广播） | `{"type":"discover","nonce":"<hex>"}` | 发现扫描 |
| 服务端 → PC（UDP 单播应答） | `{"type":"announce","name":"heartbeat-server","ws_port":8123,"uptime_s":123}` | 发现应答 |

### 时间参数

| 参数 | 值 | 位置 |
|---|---|---|
| 心跳间隔 | 3 秒 | `src/protocol.rs::HEARTBEAT_INTERVAL` |
| 心跳丢失判定 | 60 秒 | `src/protocol.rs::HEARTBEAT_TIMEOUT` |
| 关机倒计时 | 60 秒 | `src/protocol.rs::COUNTDOWN_SECONDS` |
| 稍后关机重弹 | 30 秒 | `src/protocol.rs::SNOOZE_LATER_SECONDS` |

## 数据与配置

本软件**不持久化任何数据**：不写配置文件、不留日志，每次启动都会重新扫描
局域网。地址支持三种写法：`192.168.1.100`、`192.168.1.100:8123`、
`ws://192.168.1.100:8123`（仅在本次运行内生效）。旧版本遗留的
`%APPDATA%\auto-shutdown\config.json` 会在启动时自动清理。

## 代码结构

```
src/
├── main.rs       入口：事件主循环、主窗口创建
├── app.rs        主界面 + 事件主控（扫描/测试/保存、倒计时调度、关机）
├── countdown.rs  关机倒计时弹窗（置顶 PopUp 窗口，60s 实时倒计时）
├── monitor.rs    后台心跳线程（心跳 / 扫描 / 测试，自带 tokio runtime）
├── crypto.rs     加解密（AES-256-CTR + HMAC-SHA256，含与 Python 端互验测试）
├── protocol.rs   协议常量与报文定义
├── tray.rs       系统托盘（图标、菜单、双击行为）
├── autostart.rs  开机启动开关（任务计划程序 COM 读写）
├── win32.rs      Win32 辅助（按标题找窗口、隐藏/恢复、拦截最小化）
└── events.rs     事件与命令通道定义
python/
├── protocol.py         加密协议实现（服务端移植参考）
├── heartbeat_server.py 心跳服务端模拟（支持 --outage 断电模拟）
└── requirements.txt
ESP8266/
├── ESP8266.ino         NodeMCU(ESP8266) 心跳服务端完整实现（Arduino IDE 直接上传）
├── htmx_min.h          Web 界面自动生成产物（勿手改，由 web/ Vite 构建生成）
├── web/                Web 管理界面 Vite 项目（Tailwind + daisyUI + htmx）
└── README.md           配置、烧录与验证说明
```

## 常见问题

- **要不要管理员权限？** 不需要。关机使用系统自带的 `shutdown /s /t 0`，
  开机自启动通过任务计划程序以当前用户身份注册，均为普通用户权限。
- **PC 睡眠时会误判吗？** 心跳线程随进程暂停，唤醒后第一次心跳成功即恢复
  正常状态；不会在睡眠中触发关机。
- **扫描不到设备？** 扫描会枚举本机所有 IPv4 网卡（含 Hyper-V / WSL / VMware /
  VPN / Tailscale 等虚拟网卡），逐网卡向其子网广播地址发送发现报文，本机
  `127.0.0.1` 的服务端也会被扫到。若仍未发现，先确认服务端与 PC 在同一网段
  且无 AP 隔离；也可手动输入地址后用"测试"按钮验证。Windows 防火墙一般不
  影响本应用的出站流量。
- **如何彻底退出？** 右键托盘图标 → 退出程序。
- **改了口令后报"HMAC 校验失败"？** 两端口令不一致，同步修改
  `PASSPHRASE` 后重启两端即可（这正是加密设计在起作用：第三方无法伪装设备）。

## 已验证

- Rust 端与 Python 端加密帧双向互通（见 `src/crypto.rs` 的单元测试，
  其中 `python_frame_opens_in_rust` 使用 Python 端生成的固定帧做互验）；
- PC 端实际运行：对模拟服务端 3 秒一次心跳、往返延迟约 1-4ms；
- 模拟服务端的心跳应答与 UDP 发现应答均通过加密握手验证。
