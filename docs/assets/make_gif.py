#!/usr/bin/env python3
"""Render docs/assets/demo.gif: a terminal-style loop of an agent using the broker.

The replies shown are trimmed real output from `search_context` and `find_in_codebase`
run against this repository (home path replaced by /path/to/project). The numbers in the
closing stats box come from the README performance table.

Run from the repository root:
    uv run --with pillow python docs/assets/make_gif.py
"""
from __future__ import annotations

import os
import subprocess

from PIL import Image, ImageDraw, ImageFont

W, H = 900, 560
BG = (13, 17, 23)
FG = (201, 209, 217)
DIM = (125, 133, 144)
GREEN = (63, 185, 80)
BLUE = (88, 166, 255)
PURPLE = (188, 140, 255)
YELLOW = (210, 153, 34)
BORDER = (48, 54, 61)
HEADER = (22, 27, 34)

FONT_CANDIDATES = [
    "/usr/share/fonts/truetype/DejaVuSansMono.ttf",
    "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
    "/usr/share/fonts/dejavu/DejaVuSansMono.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
]


def find_font() -> str:
    for path in FONT_CANDIDATES:
        if os.path.exists(path):
            return path
    out = subprocess.run(
        ["fc-match", "-f", "%{file}", "monospace"], capture_output=True, text=True
    ).stdout.strip()
    if out and os.path.exists(out):
        return out
    raise SystemExit("no monospace font found")


FONT_PATH = find_font()
F = ImageFont.truetype(FONT_PATH, 16)
FS = ImageFont.truetype(FONT_PATH, 14)
FB = ImageFont.truetype(FONT_PATH, 18)

LINE_H = 24
PAD = 28
TOP = 52 + 22

# Each entry: (text, colour). Replies are trimmed real output.
CALL_1 = ('> search_context  query="how are secret files blocked from indexing"', BLUE)
REPLY_1 = [
    ("5 of 9 files · root: /path/to/project", DIM),
    ("### src/security.rs", PURPLE),
    ("@L30", YELLOW),
    ("/// True when the file name or relative path matches a hard-coded secret pattern.", FG),
    ("pub fn is_secret_path(rel_path: &str) -> bool {", FG),
    ("@L37", YELLOW),
    ("/// True when any line of `content` carries a secret signature.", FG),
    ("pub fn has_secret_content(content: &str) -> bool {", FG),
    ("### src/project.rs", PURPLE),
    ("@L67", YELLOW),
    ("            supported", FG),
    ("                && !security::is_secret_path(&relative(root, path))", FG),
    ("...", DIM),
]
CALL_2 = ('> find_in_codebase  pattern="fn embed"  file_glob="*.rs"', BLUE)
REPLY_2 = [
    ("1 matches in 1/8 files · root: /path/to/project", DIM),
    ("### src/index.rs", PURPLE),
    ("L124: fn embed(&mut self, texts: Vec<String>) -> Result<Vec<Vec<f32>>, String> {", FG),
]
STATS = [
    ("plain text, once, no telemetry", FG),
    ("average search reply   ~660 tokens", GREEN),
    ("warm search            12-20 ms", GREEN),
    ("tool list              11 tools, 1,067 tokens", GREEN),
]
STATS_NOTE = "measured on a 235-file repo, see README"

FPS = 10
T_SCENE_2 = 4.4
T_END = 9.0


def typed(text: str, start: float, dur: float, t: float) -> str:
    if t <= start:
        return ""
    return text[: int(len(text) * min(1.0, (t - start) / dur))]


def chrome(d: ImageDraw.ImageDraw) -> None:
    d.rounded_rectangle([8, 8, W - 8, H - 8], radius=14, outline=BORDER, width=2, fill=BG)
    d.rectangle([9, 9, W - 9, 52], fill=HEADER)
    d.rounded_rectangle([8, 8, W - 8, H - 8], radius=14, outline=BORDER, width=2)
    for i, c in enumerate([(255, 95, 86), (255, 189, 46), (39, 201, 63)]):
        d.ellipse([28 + i * 30, 24, 44 + i * 30, 40], fill=c)
    d.text((W // 2 - 120, 22), "agent  -  context-broker over MCP stdio", font=FS, fill=DIM)


def scene(d, t, call, reply, t0, y):
    """Draw one call and its reply; returns the next y, or None while still typing."""
    shown = typed(call[0], t0 + 0.2, 1.3, t)
    d.text((PAD, y), shown, font=F, fill=call[1])
    if t < t0 + 1.5:
        if int(t * 4) % 2 == 0:
            x = PAD + d.textlength(shown, font=F)
            d.rectangle([x + 2, y + 2, x + 11, y + 19], fill=FG)
        return None
    y += LINE_H + 8
    n = int((t - (t0 + 1.7)) / 0.14) + 1
    for text, colour in reply[: max(0, n)]:
        d.text((PAD, y), text, font=F, fill=colour)
        y += LINE_H
    return y if n >= len(reply) else None


def draw_frame(t: float) -> Image.Image:
    img = Image.new("RGB", (W, H), BG)
    d = ImageDraw.Draw(img)
    chrome(d)
    if t < T_SCENE_2:
        scene(d, t, CALL_1, REPLY_1, 0.0, TOP)
        return img
    y = scene(d, t, CALL_2, REPLY_2, T_SCENE_2, TOP)
    if y is None:
        return img
    y += 14
    shown = int((t - (T_SCENE_2 + 1.7 + 0.14 * len(REPLY_2) + 0.3)) / 0.35) + 1
    if shown <= 0:
        return img
    box_h = len(STATS) * LINE_H + 62
    d.rounded_rectangle([PAD, y, W - PAD, y + box_h], radius=10, outline=GREEN, width=2)
    d.text((PAD + 18, y + 12), "WHAT THE AGENT PAID", font=FS, fill=GREEN)
    for i, (text, colour) in enumerate(STATS[:shown]):
        d.text((PAD + 18, y + 38 + i * LINE_H), text, font=FB, fill=colour)
    if shown > len(STATS):
        d.text((PAD + 18, y + 38 + len(STATS) * LINE_H), STATS_NOTE, font=FS, fill=DIM)
    return img


def main() -> None:
    out = os.path.join("docs", "assets", "demo.gif")
    os.makedirs(os.path.dirname(out), exist_ok=True)
    count = int(T_END * FPS)
    frames = [draw_frame(i / FPS) for i in range(count)]
    frames[0].save(
        out,
        save_all=True,
        append_images=frames[1:],
        duration=int(1000 / FPS),
        loop=0,
        optimize=True,
    )
    print(f"wrote {out} ({os.path.getsize(out) / 1024:.0f} KiB, {count} frames, {T_END}s)")


if __name__ == "__main__":
    main()
