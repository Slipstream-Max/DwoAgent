# -*- coding: utf-8 -*-
"""Before/after diagram: decoupling dwo-host from dwo-channels."""
import math
from PIL import Image, ImageDraw, ImageFont

S = 2
W, H = 1100, 1560

INK = "#2C3E50"
SOFT = "#7F8C8D"
RED, RED_FILL = "#E74C3C", "#FDEDEC"
GRAY, GRAY_FILL = "#95A5A6", "#F4F6F7"
BLUE, BLUE_FILL = "#2980B9", "#EAF4FB"


def load_font(size, bold=False):
    names = ("msyhbd.ttc", "msyh.ttc") if bold else ("msyh.ttc",)
    for name in names:
        try:
            return ImageFont.truetype("C:/Windows/Fonts/" + name, size * S)
        except OSError:
            pass
    return ImageFont.load_default()


F_TITLE = load_font(36, True)
F_BOX = load_font(28, True)
F_SUB = load_font(24)
F_BRACKET = load_font(26, True)
F_SMALL = load_font(21)
F_NOTE = load_font(27)

img = Image.new("RGB", (W * S, H * S), "#FFFFFF")
d = ImageDraw.Draw(img)


def text(x, y, s, font, fill=INK, anchor="mm"):
    d.text((x * S, y * S), s, font=font, fill=fill, anchor=anchor)


def box(x, y, w, h, fill, outline, radius=15, width=3):
    d.rounded_rectangle([x * S, y * S, (x + w) * S, (y + h) * S],
                        radius=radius * S, fill=fill, outline=outline, width=width * S)


def arrow_up(x, y_from, y_to, color=RED, width=4):
    d.line([x * S, y_from * S, x * S, (y_to + 10) * S], fill=color, width=width * S)
    d.polygon([(x * S, y_to * S), ((x - 12) * S, (y_to + 20) * S), ((x + 12) * S, (y_to + 20) * S)], fill=color)


def dashed(x1, y1, x2, y2, color=BLUE, width=2, dash=10, gap=7):
    total = math.hypot(x2 - x1, y2 - y1)
    if total == 0:
        return
    ux, uy = (x2 - x1) / total, (y2 - y1) / total
    pos = 0.0
    while pos < total:
        end = min(pos + dash, total)
        d.line([((x1 + ux * pos) * S, (y1 + uy * pos) * S),
                ((x1 + ux * end) * S, (y1 + uy * end) * S)], fill=color, width=width * S)
        pos = end + gap


def bracket(x, y1, y2, color=RED, width=4, tick=16):
    d.line([((x - tick) * S, y1 * S), (x * S, y1 * S)], fill=color, width=width * S)
    d.line([(x * S, y1 * S), (x * S, y2 * S)], fill=color, width=width * S)
    d.line([((x - tick) * S, y2 * S), (x * S, y2 * S)], fill=color, width=width * S)


# ---------- panel A: before ----------
text(70, 40, "现在：改一行 QQ 代码 → 7 个模块全部重编", F_TITLE, INK, anchor="lm")

rows = [
    "dwo-agent（大楼本体）",
    "dwo-cli（前台）",
    "dwo-websocket（网线）",
    "dwo-acp（调度）",
    "dwo-ipc（电话局）",
    "dwo-host（管家）",
]
bx, bw, bh, gapy = 70, 600, 78, 17
y = 110
for name in rows:
    box(bx, y, bw, bh, RED_FILL, RED)
    text(bx + bw / 2, y + bh / 2, name, F_BOX)
    y += bh + gapy

box(bx, y, bw, bh, RED_FILL, RED)
text(bx + bw / 2, y + 26, "dwo-channels（QQ 通道）", F_BOX)
text(bx + bw / 2, y + 55, "← 改这里（例子）", F_SUB, RED)
bottom_a = y + bh

bracket(690, 110, bottom_a)
text(706, 420, "全部重编", F_BOX, RED, anchor="lm")
text(706, 458, "≈ 5 分 20 秒", F_BRACKET, RED, anchor="lm")
text(370, 782, "改动从下往上连坐，楼上依次重编", F_SMALL, SOFT)

# ---------- divider ----------
d.line([70 * S, 812 * S, 1030 * S, 812 * S], fill="#D5DBDB", width=2 * S)

# ---------- panel B: after ----------
text(70, 852, "改法：从此只重编 2 个模块（≈ 2.5–3 分钟）", F_TITLE, INK, anchor="lm")

box(70, 910, 560, 84, RED_FILL, RED)
text(350, 936, "dwo-agent", F_BOX)
text(350, 968, "（大楼本体 + 总装配）", F_SUB, SOFT)

arrow_up(350, 1068, 998)

box(70, 1070, 560, 84, RED_FILL, RED)
text(350, 1096, "dwo-channels（QQ 通道）", F_BOX)
text(350, 1128, "← 改这里（例子）", F_SUB, RED)

bracket(645, 910, 1154)
text(660, 1000, "只重编", F_BRACKET, RED, anchor="lm")
text(660, 1032, "这 2 个", F_BRACKET, RED, anchor="lm")
text(660, 1068, "≈ 2.5–3 分钟", F_SUB, RED, anchor="lm")

# right column: untouched floors
text(940, 888, "其余 5 层：不用动", F_SUB, SOFT)
gray_rows = ["host（管家）", "ipc（电话局）", "acp（调度）", "websocket（网线）", "cli（前台）"]
gy = 910
for name in gray_rows:
    box(810, gy, 260, 52, GRAY_FILL, GRAY, radius=12, width=2)
    text(940, gy + 26, name, F_SUB, "#5D6D7E")
    gy += 62

# socket spec crate
dashed(940, 1212, 940, 1256)
text(926, 1234, "楼上只认它", F_SMALL, BLUE, anchor="rm")
box(810, 1258, 260, 112, BLUE_FILL, BLUE, radius=12, width=2)
text(940, 1284, "dwo-channel-api", F_SUB, "#1F618D")
text(940, 1312, "插座说明书（新增）", F_SMALL, "#1F618D")
text(940, 1340, "只规定接口 · 不认设备", F_SMALL, SOFT)

text(550, 1448, "本质：管家不再点名具体设备，设备由「总装配」插上标准插座", F_NOTE)
text(550, 1496, "行为完全不变 —— 纯搬家和换插头", F_SUB, SOFT)

img = img.resize((W, H), Image.LANCZOS)
img.save("chanarch.png", "PNG")
print("saved chanarch.png", img.size)
