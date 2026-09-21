# -*- coding: utf-8 -*-
"""
auto-shutdown —— 心跳服务端模拟（以插在市电上的 ESP32 为例）

用途：ESP32 还没到手之前，用它在任意一台电脑上模拟 ESP32 的行为，
配合 PC 端软件联调 / 验证整条链路。后续把这里的逻辑原样搬到 ESP32 上即可。

## 行为

1. WebSocket 心跳服务端：监听 ws://0.0.0.0:8123（任意路径均可），
   收到加密 ping 立即回加密 pong（原样带回 nonce，并附上运行秒数）；
2. UDP 发现服务：监听 0.0.0.0:8124，收到加密 discover 后向来源单播
   加密 announce（name / ws_port / uptime_s）；
3. 断电模拟：--outage N 秒后"假装断电"（60 秒内不回复任何报文，
   模拟市电中断、设备失联），随后自动恢复，方便测试 PC 端的
   关机倒计时 / 自动恢复流程。

## 依赖

    pip install -r requirements.txt   # websockets + cryptography

## 用法示例

    python ups_server.py                    # 正常运行
    python ups_server.py --outage 30        # 启动 30 秒后模拟断电 60 秒
    python ups_server.py --ws-port 8123 --udp-port 8124 --name heartbeat-server
"""

import argparse
import asyncio
import time

import protocol

# ---------------------------------------------------------------------------
# 全局状态
# ---------------------------------------------------------------------------

START_TIME = time.time()   # 进程启动时间（计算 uptime_s）
OUTAGE_AT = None           # 模拟断电的触发时刻（time.monotonic()），None 表示不断电
OUTAGE_DURATION = 60.0     # 断电持续时间（秒），期间不回复任何报文
DEVICE_ID = "python-sim"   # 设备身份（ESP 用芯片 ID），PC 端靠它识别“同一台设备”


def log(msg: str) -> None:
    """带时间戳的日志输出"""
    print(f"[{time.strftime('%H:%M:%S')}] {msg}", flush=True)


def uptime() -> int:
    """进程已运行秒数"""
    return int(time.time() - START_TIME)


def in_outage() -> bool:
    """当前是否处于模拟断电窗口（对外的表现就是设备失联）"""
    if OUTAGE_AT is None:
        return False
    elapsed = time.monotonic() - OUTAGE_AT
    return elapsed >= 0 and elapsed < OUTAGE_DURATION


# ---------------------------------------------------------------------------
# WebSocket 心跳服务端
# ---------------------------------------------------------------------------

async def handle_ws(ws) -> None:
    """一个 PC 客户端连接的生命周期。

    心跳是“连上 -> ping -> pong -> 正常关闭”的短连接（约 3 秒一次），
    因此这里不做连接级别的日志，只记录真正的异常与非法报文。
    """
    try:
        async for raw in ws:
            # ---- 解密 ----
            try:
                msg = protocol.open_frame(raw if isinstance(raw, str) else raw.decode("utf-8"))
            except ValueError as e:
                log(f"非法报文（已丢弃）: {e}")
                continue

            mtype = msg.get("type")
            if mtype == "ping" and not in_outage():
                # 心跳：原样带回 nonce
                pong = {"type": "pong", "nonce": msg.get("nonce"), "uptime_s": uptime(),
                        "id": DEVICE_ID}
                await ws.send(protocol.seal(pong))
            elif mtype == "ping" and in_outage():
                log("[模拟断电中] 收到 ping，假装不在……")
            else:
                log(f"收到未知类型报文: {mtype}")
    except Exception as e:
        # 客户端正常关闭时 async for 会自然结束，走到这里说明是真的异常
        log(f"WebSocket 连接异常: {type(e).__name__}: {e}")


async def run_ws_server(port: int) -> None:
    import websockets

    async with websockets.serve(handle_ws, "0.0.0.0", port):
        log(f"WebSocket 心跳服务已启动: ws://0.0.0.0:{port} (路径不限制)")
        await asyncio.Future()  # 永久运行


# ---------------------------------------------------------------------------
# UDP 发现服务
# ---------------------------------------------------------------------------

class DiscoveryProtocol(asyncio.DatagramProtocol):
    """UDP 发现服务：收到 discover -> 单播 announce"""

    def __init__(self, name: str, ws_port: int):
        self.name = name
        self.ws_port = ws_port
        self._last_log: dict = {}  # 按来源地址去重日志（一次扫描会广播多次）

    def connection_made(self, transport) -> None:
        self.transport = transport

    def datagram_received(self, data: bytes, addr) -> None:
        if in_outage():
            return  # 模拟断电：对发现广播也保持沉默
        try:
            msg = protocol.open_frame(data.decode("utf-8"))
        except (ValueError, UnicodeDecodeError):
            return  # 不是我们的设备（解不开），静默忽略
        if msg.get("type") != "discover":
            return
        announce = {
            "type": "announce",
            "name": self.name,
            "id": DEVICE_ID,
            "ws_port": self.ws_port,
            "uptime_s": uptime(),
        }
        self.transport.sendto(protocol.seal(announce).encode("ascii"), addr)
        now = time.monotonic()
        if now - self._last_log.get(addr, 0) > 1.0:
            self._last_log[addr] = now
            log(f"UDP 发现应答 -> {addr[0]}:{addr[1]} (name={self.name})")


async def run_udp_server(port: int, name: str, ws_port: int) -> None:
    loop = asyncio.get_running_loop()
    transport, _ = await loop.create_datagram_endpoint(
        # 注意：announce 里必须带 WebSocket 端口，而不是 UDP 端口
        lambda: DiscoveryProtocol(name, ws_port), local_addr=("0.0.0.0", port)
    )
    log(f"UDP 发现服务已启动: 0.0.0.0:{port} (设备名: {name})")
    await asyncio.Future()  # 永久运行


# ---------------------------------------------------------------------------
# 入口
# ---------------------------------------------------------------------------

async def main() -> None:
    parser = argparse.ArgumentParser(description="auto-shutdown 心跳服务端模拟（ESP32 移植参考）")
    parser.add_argument("--ws-port", type=int, default=8123, help="WebSocket 端口（默认 8123）")
    parser.add_argument("--udp-port", type=int, default=8124, help="UDP 发现端口（默认 8124）")
    parser.add_argument("--name", default="heartbeat-server", help="设备名称（发现结果里展示）")
    parser.add_argument(
        "--outage", type=int, default=0, metavar="N",
        help="启动 N 秒后模拟断电 60 秒（期间不回复任何报文），之后自动恢复",
    )
    args = parser.parse_args()

    global OUTAGE_AT
    if args.outage > 0:
        OUTAGE_AT = time.monotonic() + args.outage
        log(f"将在 {args.outage} 秒后模拟断电 {OUTAGE_DURATION:.0f} 秒")

    log(f"ESP32 模拟服务端启动 (name={args.name}, uptime 从现在起算)")
    tasks = [
        asyncio.create_task(run_ws_server(args.ws_port)),
        asyncio.create_task(run_udp_server(args.udp_port, args.name, args.ws_port)),
    ]
    # Ctrl+C 由最外层 except KeyboardInterrupt 处理
    await asyncio.Future()  # 永久运行


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
