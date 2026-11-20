#!/usr/bin/env python3
"""Render the cdp-mcp info video from cards, with no video model and no voiceover.

Every number and line here is a measured fact, typeset rather than filmed, so the
text stays crisp at X's bitrate and nothing is faked to look like a capture.

Needs: python3, rsvg-convert, ffmpeg, Noto Sans Mono.
Output: video/out/cdp-mcp-0.10.7.mp4 (1920x1080, 30 fps, silent).
"""

import html
import shutil
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
OUT = HERE / "out"
CARDS = OUT / "cards"
MP4 = OUT / "cdp-mcp-0.10.7.mp4"
VERSION = "0.10.7"

BG = "#0e1012"
FG = "#f2f4f6"
MID = "#c3c9d0"
MUTED = "#6f7883"
ACCENT = "#7ee787"
X = 160
FONT = "Noto Sans Mono, monospace"


def el(s, size, fill, x=X, y=0, weight="normal", anchor="start"):
    return (
        f'<text x="{x}" y="{y}" font-family="{FONT}" font-size="{size}" '
        f'font-weight="{weight}" fill="{fill}" text-anchor="{anchor}" '
        f'xml:space="preserve">{html.escape(s)}</text>'
    )


def frame(kicker, title, body, caption, diagram=False, tools=False):
    parts = [
        f'<rect width="1920" height="1080" fill="{BG}"/>',
        f'<rect x="0" y="0" width="1920" height="4" fill="{ACCENT}"/>',
        el(kicker, 26, ACCENT, y=150),
        el(title, 88, FG, y=270, weight="600"),
        el(f"cdp-mcp {VERSION}", 22, MUTED, y=1036),
        f'<text x="1760" y="1036" text-anchor="end" font-family="{FONT}" '
        f'font-size="22" fill="{MUTED}">v1 · Linux x86-64</text>',
    ]
    y = 380
    if diagram:
        boxes = ["page", "MutationObserver", "CDP binding", "socket read"]
        for i, label in enumerate(boxes):
            bx = X + i * 440
            parts.append(
                f'<rect x="{bx}" y="400" width="360" height="140" rx="10" '
                f'fill="#16191d" stroke="#2a2f36"/>'
            )
            parts.append(el(label, 30, MID, x=bx + 24, y=482))
            if i < 3:
                parts.append(el("→", 44, ACCENT, x=bx + 386, y=488))
        parts.append(el("still page: 0 messages", 46, ACCENT, y=700))
        parts.append(el("one change: one wake, named", 30, MID, y=756))
    elif tools:
        names = [
            "flow", "idle", "report", "batch",
            "status", "tabs", "eval", "elements",
            "text", "input", "upload", "paste",
            "images", "clip", "link", "image",
            "download", "profile", "shot",
        ]
        for i, name in enumerate(names):
            col, row = i % 4, i // 4
            parts.append(el(name, 30, MID, x=X + col * 420, y=380 + row * 68))
    else:
        for line in body:
            parts.append(el(line, 38, MID, y=y))
            y += 62
    parts.append(el(caption, 27, MUTED, y=930))
    return (
        '<svg xmlns="http://www.w3.org/2000/svg" width="1920" height="1080" '
        'viewBox="0 0 1920 1080">' + "".join(parts) + "</svg>"
    )


CARDS_SPEC = [
    dict(
        id="01-title", dur=3.0, kicker="01 / THE TOOL", title=f"cdp-mcp {VERSION}",
        body=["a browser tool for agents,", "as one binary"],
        caption="MCP over stdio · drives a real browser over CDP",
    ),
    dict(
        id="02-binary", dur=3.5, kicker="02 / THE BINARY", title="1,111,616 bytes",
        body=["static-pie · stripped · musl · x86-64",
              "no Node · no Playwright · no extension"],
        caption="One file. Nothing to install beside it.",
    ),
    dict(
        id="03-attached", dur=3.5, kicker="03 / ATTACHED", title="your browser",
        body=["already logged in", "no new profile · no extension"],
        caption="Every session is already there.",
    ),
    dict(
        id="04-idle", dur=3.5, kicker="04 / AT REST", title="1.3 MB resident",
        body=["1 process", "0.0% CPU between calls"],
        caption="It sits still until a page moves.",
    ),
    dict(
        id="05-waits", dur=5.0, kicker="05 / WAITS", title="the page pushes",
        body=[], diagram=True,
        caption="A still page sends zero messages.",
    ),
    dict(
        id="06-tools", dur=4.0, kicker="06 / TOOLS", title="19 tools",
        body=[], tools=True,
        caption="Every tool resolves its own tab. Nothing to thread through.",
    ),
    dict(
        id="07-built", dur=3.5, kicker="07 / BUILT ON", title="CachyOS · Wayland",
        body=["niri-git · Noctalia 5.5.0",
              "Helium 0.21.3.1 · Chromium 157"],
        caption="A Linux desktop, not a server farm.",
    ),
    dict(
        id="08-install", dur=4.0, kicker="08 / INSTALL", title="one MCP entry",
        body=['"cdp": {', '  "command": ["cdp-mcp"]', "}"],
        caption="stdio · no extension · no relay",
    ),
    dict(
        id="09-limits", dur=6.0, kicker="09 / LIMITS", title="v1, stated plainly",
        body=[
            "Linux x86-64 only",
            "one browser endpoint, real profile",
            "no network awareness yet",
            "80 ms reports · 50 ms text checks",
            "one call at a time",
            "loopback by default, no auth",
        ],
        caption="Network tracking is the next known gap.",
    ),
    dict(
        id="10-yours", dur=3.5, kicker="10 / YOURS", title="fork it, go wild",
        body=["tweak it for your machine", "one ask: a star"],
        caption="github.com/benkaesy820/cdp-mcp",
    ),
]


def need(cmd):
    if shutil.which(cmd) is None:
        sys.exit(f"missing {cmd}")


def main():
    for c in ("rsvg-convert", "ffmpeg", "ffprobe"):
        need(c)
    OUT.mkdir(parents=True, exist_ok=True)
    CARDS.mkdir(parents=True, exist_ok=True)
    lines = []
    for spec in CARDS_SPEC:
        svg = frame(
            spec["kicker"], spec["title"], spec["body"], spec["caption"],
            diagram=spec.get("diagram", False), tools=spec.get("tools", False),
        )
        svg_path = CARDS / f"{spec['id']}.svg"
        png_path = CARDS / f"{spec['id']}.png"
        svg_path.write_text(svg)
        subprocess.run(
            ["rsvg-convert", "-w", "1920", "-h", "1080", str(svg_path), "-o", str(png_path)],
            check=True,
        )
        lines += [f"file '{png_path}'", f"duration {spec['dur']}"]
    total = sum(spec["dur"] for spec in CARDS_SPEC)
    list_path = OUT / "list.txt"
    list_path.write_text("\n".join(lines) + "\n")
    subprocess.run(
        [
            "ffmpeg", "-y", "-f", "concat", "-safe", "0", "-i", str(list_path),
            "-vf",
            f"fps=30,format=yuv420p,drawbox=x=0:y=1074:w='iw*t/{total}':h=4:"
            "color=#7ee787@0.55:t=fill",
            "-tune", "stillimage", "-t", f"{total:.3f}",
            "-c:v", "libx264", "-preset", "slow", "-crf", "19",
            "-movflags", "+faststart", "-an", str(MP4),
        ],
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    probe = subprocess.run(
        ["ffprobe", "-v", "error", "-show_entries", "format=duration,size",
         "-of", "default=nw=1", str(MP4)],
        check=True, capture_output=True, text=True,
    )
    print(MP4)
    print(probe.stdout.strip())


if __name__ == "__main__":
    main()
