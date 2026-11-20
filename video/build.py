#!/usr/bin/env python3
"""Render the cdp-mcp info video from cards, with no video model and no voiceover.

Every number and line here is a measured fact, typeset rather than filmed, so the
text stays crisp at X's bitrate and nothing is faked to look like a capture.

Needs: python3, rsvg-convert, ffmpeg, ffprobe, Noto Sans Mono.
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
VERSION = "0.10.7"
MP4 = OUT / f"cdp-mcp-{VERSION}.mp4"

BG = "#0e1012"
PANEL = "#14181d"
BORDER = "#262c34"
RULE = "#1e232a"
TRACK = "#1b2026"
FG = "#f2f4f6"
MID = "#d0d6dd"
CAP = "#a5aeb8"
MUTED = "#6f7883"
ACCENT = "#7ee787"
BAR_COLOR = "#58a863"

X = 160
R = 1760
W = R - X  # 1600 px content grid
FONT = "Noto Sans Mono, monospace"


def esc(s):
    return html.escape(s, quote=True)


def el(s, size, fill, x=X, y=0, weight="normal", anchor="start"):
    return (
        f'<text x="{x}" y="{y}" font-family="{FONT}" font-size="{size}" '
        f'font-weight="{weight}" fill="{fill}" text-anchor="{anchor}" '
        f'xml:space="preserve">{esc(s)}</text>'
    )


def code_line(tokens, x, y, size=27):
    spans = "".join(f'<tspan fill="{color}">{esc(text)}</tspan>' for text, color in tokens)
    return (
        f'<text x="{x}" y="{y}" font-family="{FONT}" font-size="{size}" '
        f'xml:space="preserve">{spans}</text>'
    )


def render_stats(items):
    # 3 equal cards spanning X..R (512 * 3 + 32 * 2 = 1600)
    parts = []
    cw, gap = 512, 32
    for i, (big, sub1, sub2) in enumerate(items):
        bx = X + i * (cw + gap)
        parts.append(
            f'<rect x="{bx}" y="356" width="{cw}" height="310" rx="12" '
            f'fill="{PANEL}" stroke="{BORDER}"/>'
        )
        parts.append(el(big, 68, ACCENT, x=bx + 36, y=476, weight="600"))
        parts.append(el(sub1, 26, FG, x=bx + 36, y=552))
        parts.append(el(sub2, 24, CAP, x=bx + 36, y=596))
    return parts


def render_kv(rows, y0=356, row_h=88):
    h = len(rows) * row_h + 64
    parts = [
        f'<rect x="{X}" y="{y0}" width="{W}" height="{h}" rx="12" '
        f'fill="{PANEL}" stroke="{BORDER}"/>'
    ]
    for i, (k, v) in enumerate(rows):
        ry = y0 + 76 + i * row_h
        if i > 0:
            ly = ry - row_h + 28
            parts.append(f'<line x1="{X + 36}" y1="{ly}" x2="{R - 36}" y2="{ly}" stroke="{RULE}"/>')
        parts.append(el(k, 26, ACCENT, x=X + 44, y=ry))
        parts.append(el(v, 30, MID, x=X + 260, y=ry))
    return parts


def render_diagram():
    # 4 equal boxes spanning X..R (340 * 4 + 80 * 3 = 1600)
    parts = []
    boxes = ["page", "MutationObserver", "CDP binding", "socket read"]
    bw, gap = 340, 80
    for i, label in enumerate(boxes):
        bx = X + i * (bw + gap)
        parts.append(
            f'<rect x="{bx}" y="376" width="{bw}" height="136" rx="10" '
            f'fill="{PANEL}" stroke="{BORDER}"/>'
        )
        parts.append(el(label, 27, FG, x=bx + bw // 2, y=454, anchor="middle"))
        if i < 3:
            parts.append(el("→", 40, ACCENT, x=bx + bw + gap // 2, y=458, anchor="middle"))
    parts.append(f'<rect x="{X}" y="596" width="4" height="122" fill="{ACCENT}"/>')
    parts.append(el("still page: 0 messages", 44, ACCENT, x=X + 32, y=646, weight="600"))
    parts.append(el("one change: one wake, named", 30, MID, x=X + 32, y=704))
    return parts


def render_tools(names):
    # 5 columns x 4 rows spanning X..R (304 * 5 + 20 * 4 = 1600)
    parts = []
    cw, ch, hgap, vgap = 304, 76, 20, 20
    for i, name in enumerate(names):
        col, row = i % 5, i // 5
        bx = X + col * (cw + hgap)
        by = 360 + row * (ch + vgap)
        parts.append(
            f'<rect x="{bx}" y="{by}" width="{cw}" height="{ch}" rx="8" '
            f'fill="{PANEL}" stroke="{BORDER}"/>'
        )
        parts.append(el(name, 30, MID, x=bx + 28, y=by + 48))
    return parts


def render_install():
    # 2 equal panels spanning X..R (780 * 2 + 40 = 1600)
    panels = [
        (
            "Claude Code  ·  .mcp.json",
            [
                [("{", MUTED)],
                [('  "mcpServers"', FG), (": {", MUTED)],
                [('    "cdp"', FG), (": {", MUTED)],
                [('      "command"', FG), (": ", MUTED), ('"cdp-mcp"', ACCENT)],
                [("    }", MUTED)],
                [("  }", MUTED)],
                [("}", MUTED)],
            ],
        ),
        (
            "OpenCode  ·  opencode.json",
            [
                [("{", MUTED)],
                [('  "mcp"', FG), (": {", MUTED)],
                [('    "cdp"', FG), (": {", MUTED)],
                [('      "type"', FG), (": ", MUTED), ('"local"', ACCENT), (",", MUTED)],
                [('      "command"', FG), (": [", MUTED), ('"cdp-mcp"', ACCENT), ("]", MUTED)],
                [("    }", MUTED)],
                [("  }", MUTED)],
                [("}", MUTED)],
            ],
        ),
    ]
    parts = []
    pw, gap = 780, 40
    for i, (header, lines) in enumerate(panels):
        bx = X + i * (pw + gap)
        parts.append(
            f'<rect x="{bx}" y="346" width="{pw}" height="456" rx="12" '
            f'fill="{PANEL}" stroke="{BORDER}"/>'
        )
        parts.append(f'<line x1="{bx}" y1="406" x2="{bx + pw}" y2="406" stroke="{BORDER}"/>')
        parts.append(el(header, 23, CAP, x=bx + 32, y=385))
        for j, tokens in enumerate(lines):
            parts.append(code_line(tokens, x=bx + 36, y=456 + j * 41, size=27))
    return parts


def render_limits(items):
    parts = []
    for i, line in enumerate(items):
        ry = 382 + i * 68
        parts.append(el("—", 32, ACCENT, x=X, y=ry))
        parts.append(el(line, 34, MID, x=X + 46, y=ry))
    return parts


def render_end(lines, repo):
    parts = []
    for i, line in enumerate(lines):
        parts.append(el(line, 38, MID, y=386 + i * 64))
    parts.append(
        f'<rect x="{X}" y="546" width="720" height="96" rx="12" '
        f'fill="{PANEL}" stroke="{ACCENT}" stroke-width="1.5"/>'
    )
    parts.append(el(repo, 34, ACCENT, x=X + 40, y=606, weight="600"))
    return parts


def frame(spec):
    parts = [
        f'<rect width="1920" height="1080" fill="{BG}"/>',
        f'<rect x="0" y="0" width="1920" height="4" fill="{ACCENT}"/>',
        el(spec["kicker"], 26, ACCENT, y=148),
        el(spec["title"], 84, FG, y=258, weight="600"),
    ]
    kind = spec["kind"]
    if kind == "stats":
        parts += render_stats(spec["items"])
    elif kind == "kv":
        parts += render_kv(spec["rows"])
    elif kind == "diagram":
        parts += render_diagram()
    elif kind == "tools":
        parts += render_tools(spec["names"])
    elif kind == "install":
        parts += render_install()
    elif kind == "limits":
        parts += render_limits(spec["items"])
    elif kind == "end":
        parts += render_end(spec["lines"], spec["repo"])

    parts += [
        f'<line x1="{X}" y1="856" x2="{R}" y2="856" stroke="{RULE}"/>',
        el(spec["caption"], 30, CAP, y=922),
        el(f"cdp-mcp {VERSION}", 22, MUTED, y=1024),
        el("v1 · Linux x86-64", 22, MUTED, x=R, y=1024, anchor="end"),
        f'<rect x="0" y="1076" width="1920" height="4" fill="{TRACK}"/>',
    ]
    return (
        '<svg xmlns="http://www.w3.org/2000/svg" width="1920" height="1080" '
        'viewBox="0 0 1920 1080">' + "".join(parts) + "</svg>"
    )


CARDS_SPEC = [
    dict(
        id="01-hook", dur=3.2, kind="stats",
        kicker=f"01 / CDP-MCP {VERSION}",
        title="one binary for your browser",
        items=[
            ("1.1 MB", "static-pie · musl", "stripped · x86-64"),
            ("1.3 MB", "1 process at rest", "0.0% CPU between calls"),
            ("0", "no Node · no Playwright", "no browser extension"),
        ],
        caption="MCP over stdio · drives an already-logged-in browser over CDP",
    ),
    dict(
        id="02-spec", dur=3.5, kind="kv",
        kicker="02 / ARTIFACT & SESSION",
        title="1,111,616 bytes",
        rows=[
            ("binary", "1,111,616 bytes · static-pie · stripped · musl"),
            ("at rest", "1.3 MB resident · 1 process · 0.0% CPU"),
            ("browser", "your real profile · already logged in"),
            ("wire", "MCP over stdio -> CDP on loopback"),
        ],
        caption="One file. Every session is already there.",
    ),
    dict(
        id="03-waits", dur=4.5, kind="diagram",
        kicker="03 / WAITS",
        title="the page pushes",
        caption="No polling loop. Rust blocks on the socket until the DOM moves.",
    ),
    dict(
        id="04-tools", dur=4.0, kind="tools",
        kicker="04 / TOOLS",
        title="19 tools",
        names=[
            "flow", "idle", "report", "batch", "status",
            "tabs", "eval", "elements", "text", "input",
            "upload", "paste", "images", "clip", "link",
            "image", "download", "profile", "shot",
        ],
        caption="Every tool resolves its own tab. Nothing to thread through.",
    ),
    dict(
        id="05-install", dur=5.0, kind="install",
        kicker="05 / INSTALL",
        title="one command",
        caption="./install.sh asks global or one project. These are what it writes.",
    ),
    dict(
        id="06-built", dur=3.0, kind="kv",
        kicker="06 / BUILT ON",
        title="CachyOS · Wayland",
        rows=[
            ("desktop", "niri-git · Noctalia 5.5.0"),
            ("browser", "Helium 0.21.3.1 · Chromium 157"),
        ],
        caption="A Linux desktop, not a server farm.",
    ),
    dict(
        id="07-limits", dur=5.3, kind="limits",
        kicker="07 / LIMITS",
        title="v1, stated plainly",
        items=[
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
        id="08-yours", dur=3.5, kind="end",
        kicker="08 / SOURCE",
        title="fork it, go wild",
        lines=["tweak it for your machine", "one ask: a star"],
        repo="github.com/benkaesy820/cdp-mcp",
        caption=f"MIT · cdp-mcp {VERSION} · Linux x86-64",
    ),
]


def need(cmd):
    if shutil.which(cmd) is None:
        sys.exit(f"missing {cmd}")


def run(cmd):
    res = subprocess.run(cmd, capture_output=True, text=True)
    if res.returncode != 0:
        sys.stderr.write(res.stderr)
        sys.exit(res.returncode)
    return res.stdout


def main():
    for c in ("rsvg-convert", "ffmpeg", "ffprobe"):
        need(c)
    OUT.mkdir(parents=True, exist_ok=True)
    CARDS.mkdir(parents=True, exist_ok=True)

    lines = []
    last_png = None
    for spec in CARDS_SPEC:
        svg_path = CARDS / f"{spec['id']}.svg"
        png_path = CARDS / f"{spec['id']}.png"
        svg_path.write_text(frame(spec))
        run(["rsvg-convert", "-w", "1920", "-h", "1080", str(svg_path), "-o", str(png_path)])
        lines += [f"file '{png_path}'", f"duration {spec['dur']}"]
        last_png = png_path
    # Concat demuxer quirk: repeat the final file so its full duration is held.
    lines.append(f"file '{last_png}'")

    total = sum(spec["dur"] for spec in CARDS_SPEC)
    list_path = OUT / "list.txt"
    list_path.write_text("\n".join(lines) + "\n")

    run([
        "ffmpeg", "-y",
        "-f", "concat", "-safe", "0", "-i", str(list_path),
        "-f", "lavfi", "-i", f"color=c={BAR_COLOR}:s=1920x4:r=30:d={total + 1.0:.3f}",
        "-filter_complex",
        (
            f"[0:v]fps=30,format=yuv420p[bg];"
            f"[1:v]format=yuv420p[bar];"
            f"[bg][bar]overlay=x='-w+w*t/{total:.3f}':y=1076:eof_action=repeat[out]"
        ),
        "-map", "[out]",
        "-t", f"{total:.3f}",
        "-c:v", "libx264", "-profile:v", "high", "-level", "4.1",
        "-preset", "slow", "-crf", "18", "-g", "60",
        "-pix_fmt", "yuv420p", "-movflags", "+faststart", "-an",
        str(MP4),
    ])

    info = run([
        "ffprobe", "-v", "error", "-show_entries", "format=duration,size",
        "-of", "default=nw=1", str(MP4),
    ])
    print(MP4)
    print(info.strip())


if __name__ == "__main__":
    main()