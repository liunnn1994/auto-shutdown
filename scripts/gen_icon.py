# 生成 assets/app.ico：琥珀色圆角方块 + 白色闪电（与 src/tray.rs 中的托盘图标同款设计）。
# 4 倍超采样做抗锯齿，输出 16/24/32/48/64/128/256 七档尺寸。
# 仅在需要更换图标时手动运行：python scripts/gen_icon.py

from PIL import Image, ImageDraw

BG = (245, 158, 11, 255)   # amber-500
FG = (255, 255, 255, 255)

# 闪电多边形（256px 坐标系）
BOLT = [(152, 16), (64, 144), (120, 144), (96, 240), (192, 104), (132, 104)]


def render(size: int) -> Image.Image:
    ss = size * 4  # 4 倍超采样
    k = ss / 256.0
    img = Image.new("RGBA", (ss, ss), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    d.rounded_rectangle([0, 0, ss - 1, ss - 1], radius=56 * k, fill=BG)
    d.polygon([(x * k, y * k) for x, y in BOLT], fill=FG)
    return img.resize((size, size), Image.LANCZOS)


def main():
    sizes = [16, 24, 32, 48, 64, 128, 256]
    imgs = [render(s) for s in sizes]
    imgs[-1].save(
        "assets/app.ico",
        format="ICO",
        sizes=[(s, s) for s in sizes],
    )
    print("assets/app.ico 已生成")


if __name__ == "__main__":
    main()
