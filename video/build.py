#!/usr/bin/env python3
"""Render the cdp-mcp info video (with terminal emulation + stereo audio) and README GIF.

Every number and terminal line here is measured on the real machine, typeset rather
than filmed so small monospace stays crisp at X's bitrate and GitHub's GIF scale.

Needs: python3, rsvg-convert, ffmpeg, ffprobe, Noto Sans Mono.
Outputs:
  video/out/cdp-mcp-0.10.7.mp4          (1920x1080, 60 fps, stereo AAC, 33.0 s)
  video/out/cdp-mcp-0.10.7-preview.gif  (800x450, 15 fps, looping, ~8.4 s)

Usage:
  ./build.py                  # full high-quality build (MP4 + GIF)
  ./build.py --fast           # quick iteration build (veryfast x264)
  ./build.py --only-gif       # rebuild just the README GIF
  ./build.py --only-mp4       # rebuild just the MP4
  ./build.py --clean          # clear PNG cache before rendering
"""

import argparse
import array
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
import hashlib
import html
import math
import os
import shutil
import struct
import subprocess
import sys
import wave
from pathlib import Path

HERE = Path(__file__).resolve().parent
OUT = HERE / "out"
CACHE = OUT / "cache"
VERSION = "0.10.7"
MP4 = OUT / f"cdp-mcp-{VERSION}.mp4"
GIF = OUT / f"cdp-mcp-{VERSION}-preview.gif"
WAV = OUT / f"cdp-mcp-{VERSION}.wav"

# Update this one constant if install.sh is updated from mcp.servers.cdp to mcp.cdp.
OPENCODE_CONFIG_LINE = "config: /home/you/.config/opencode/opencode.json (mcp.servers.cdp)"

# Palette
BG = "#0e1012"
BG_GLOW = "#16211c"
PANEL = "#14181d"
PANEL_LIT = "#171f1c"
TERM_BG = "#101317"
CHROME = "#181d23"
SHADOW = "#06080a"
BORDER = "#262c34"
BORDER_DIM = "#1b2027"
RULE = "#1e232a"
TRACK = "#1b2026"
FG = "#f2f4f6"
MID = "#d0d6dd"
CAP = "#a5aeb8"
MUTED = "#6f7883"
ACCENT = "#7ee787"
ACCENT_DIM = "#3d6b47"
ACCENT_BG = "#15291c"
BAR_COLOR = "#58a863"

X = 160
R = 1760
W = R - X  # 1600 px grid
FONT = "Noto Sans Mono, monospace"
SR = 48000
NUM_SECTIONS = 7


# ---------------------------------------------------------------------------
# SVG primitives & chrome
# ---------------------------------------------------------------------------

def esc(s):
    return html.escape(s, quote=True)


def text_w(s, size):
    """Approximate Noto Sans Mono advance width for cursor placement."""
    return len(s) * size * 0.602


def el(s, size, fill, x=X, y=0, weight="normal", anchor="start", opacity=None):
    op = f' opacity="{opacity}"' if opacity is not None else ""
    return (
        f'<text x="{x}" y="{y}" font-family="{FONT}" font-size="{size}" '
        f'font-weight="{weight}" fill="{fill}" text-anchor="{anchor}"{op} '
        f'xml:space="preserve">{esc(s)}</text>'
    )


def shell_chrome(section_idx, kicker, title, caption, compact=False):
    k_y = 102 if compact else 146
    t_y = 180 if compact else 256
    t_sz = 64 if compact else 84
    defs = (
        "<defs>"
        '<radialGradient id="bg-glow" cx="50%" cy="38%" r="65%">'
        f'<stop offset="0%" stop-color="{BG_GLOW}" stop-opacity="0.55"/>'
        f'<stop offset="100%" stop-color="{BG}" stop-opacity="0"/>'
        "</radialGradient>"
        "</defs>"
    )
    parts = [
        defs,
        f'<rect width="1920" height="1080" fill="{BG}"/>',
        '<rect width="1920" height="1080" fill="url(#bg-glow)"/>',
        f'<rect x="0" y="0" width="1920" height="4" fill="{ACCENT}"/>',
        el(kicker, 24 if compact else 26, ACCENT, y=k_y, weight="bold"),
        el(title, t_sz, FG, y=t_y, weight="bold"),
    ]

    # Subtle 7-dot section progress indicator in top-right header
    dot_w, dot_gap = 22, 8
    total_dots_w = NUM_SECTIONS * dot_w + (NUM_SECTIONS - 1) * dot_gap
    dx0 = R - total_dots_w
    dy = k_y - 11
    for i in range(NUM_SECTIONS):
        dx = dx0 + i * (dot_w + dot_gap)
        if i == section_idx:
            col = ACCENT
        elif i < section_idx:
            col = ACCENT_DIM
        else:
            col = BORDER
        parts.append(f'<rect x="{dx}" y="{dy}" width="{dot_w}" height="4" rx="2" fill="{col}"/>')

    footer = [
        f'<line x1="{X}" y1="866" x2="{R}" y2="866" stroke="{RULE}"/>',
        el(caption, 29, CAP, y=930),
        el(f"cdp-mcp {VERSION}", 22, MUTED, y=1024),
        el("v1 · Linux x86-64", 22, MUTED, x=R, y=1024, anchor="end"),
        f'<rect x="0" y="1076" width="1920" height="4" fill="{TRACK}"/>',
    ]
    return parts, footer


def wrap_svg(parts):
    return (
        '<svg xmlns="http://www.w3.org/2000/svg" width="1920" height="1080" '
        'viewBox="0 0 1920 1080">' + "".join(parts) + "</svg>"
    )


def window_shadow(x, y, w, h, rx=10):
    return (
        f'<rect x="{x + 6}" y="{y + 10}" width="{w}" height="{h}" rx="{rx}" '
        f'fill="{SHADOW}" opacity="0.55"/>'
    )


# ---------------------------------------------------------------------------
# Card renderers
# ---------------------------------------------------------------------------

def render_hook(revealed=3, active=-1):
    """revealed in 0..3 controls staggered card entry; active highlights latest."""
    head, foot = shell_chrome(
        0,
        f"01 / CDP-MCP {VERSION}",
        "one binary for your browser",
        "MCP over stdio · drives an already-logged-in browser over CDP",
    )
    items = [
        ("1.1 MB", "static-pie · musl", "1,111,616 B · stripped · x86-64"),
        ("1.3 MB", "1 process at rest", "0.0% CPU between calls"),
        ("0", "no Node · no Playwright", "no browser extension"),
    ]
    body = []
    cw, gap = 512, 32
    for i, (big, sub1, sub2) in enumerate(items):
        bx = X + i * (cw + gap)
        shown = i < revealed
        is_cur = i == active
        fill = PANEL_LIT if is_cur else (PANEL if shown else BG)
        stroke = ACCENT if is_cur else (BORDER if shown else BORDER_DIM)
        sw = "2" if is_cur else "1.2"
        if shown:
            body.append(window_shadow(bx, 356, cw, 318, rx=12))
        body.append(
            f'<rect x="{bx}" y="356" width="{cw}" height="318" rx="12" '
            f'fill="{fill}" stroke="{stroke}" stroke-width="{sw}"/>'
        )
        if shown:
            body.append(
                f'<rect x="{bx + 36}" y="388" width="44" height="3" rx="1.5" fill="{ACCENT}"/>'
            )
            body.append(el(big, 68, ACCENT, x=bx + 36, y=482, weight="bold"))
            body.append(el(sub1, 26, FG, x=bx + 36, y=556, weight="bold"))
            body.append(el(sub2, 22, CAP, x=bx + 36, y=602))
    return wrap_svg(head + body + foot)


def render_terminal_frame(lines, cursor_on=True):
    """Render a Wayland/niri-styled terminal window with exact installer lines."""
    head, foot = shell_chrome(
        1,
        "02 / INSTALL",
        "./install.sh",
        "Detects OpenCode & Claude Code · writes binary, skill, and MCP config",
        compact=True,
    )
    wx, wy, ww, wh = X, 214, W, 626
    bar_h = 44
    body = [
        f'<defs><clipPath id="term-clip">'
        f'<rect x="{wx + 4}" y="{wy + bar_h + 2}" width="{ww - 8}" height="{wh - bar_h - 6}"/>'
        f"</clipPath></defs>",
        window_shadow(wx, wy, ww, wh, rx=10),
        f'<rect x="{wx}" y="{wy}" width="{ww}" height="{wh}" rx="10" '
        f'fill="{TERM_BG}" stroke="{BORDER}" stroke-width="1.5"/>',
        f'<path d="M{wx+10},{wy} h{ww-20} a10,10 0 0 1 10,10 v{bar_h-10} '
        f'h-{ww} v-{bar_h-10} a10,10 0 0 1 10,-10 z" fill="{CHROME}"/>',
        f'<line x1="{wx}" y1="{wy+bar_h}" x2="{wx+ww}" y2="{wy+bar_h}" stroke="{BORDER}"/>',
        f'<rect x="{wx+22}" y="{wy+15}" width="10" height="14" rx="2" fill="{ACCENT}"/>',
        el("cdp-mcp  —  ~/cdp-mcp", 19, CAP, x=wx + 44, y=wy + 29),
        el("sh · interactive", 18, MUTED, x=wx + ww - 24, y=wy + 29, anchor="end"),
    ]

    tx = wx + 32
    y0 = wy + bar_h + 36
    line_h = 31
    font_sz = 23

    term_content = ['<g clip-path="url(#term-clip)">']
    for idx, (text, color, weight) in enumerate(lines):
        ly = y0 + idx * line_h
        term_content.append(el(text, font_sz, color, x=tx, y=ly, weight=weight))

    if cursor_on and lines:
        last_text = lines[-1][0]
        cx = tx + text_w(last_text, font_sz) + 2
        cy = y0 + (len(lines) - 1) * line_h - 19
        term_content.append(
            f'<rect x="{cx:.1f}" y="{cy}" width="13" height="24" rx="1.5" '
            f'fill="{ACCENT}" opacity="0.92"/>'
        )
    term_content.append("</g>")

    return wrap_svg(head + body + term_content + foot)


def render_attached_frame(stage):
    """Split view: attached real browser (left) + MCP status result (right).

    stage 0: prompt ready
    stage 1: connecting
    stage 2: first status rows streamed in
    stage 3: full status + browser panel connected
    """
    head, foot = shell_chrome(
        2,
        "03 / ATTACHED",
        "real browser, live status",
        "Your logged-in browser session. No extension, no relay, no fresh profile.",
        compact=True,
    )
    wy, wh = 220, 612
    pw, gap = 780, 40
    lx, rx = X, X + pw + gap
    bar_h = 46
    connected = stage >= 3

    # Left: clean Chromium window chrome
    left = [
        window_shadow(lx, wy, pw, wh, rx=10),
        f'<rect x="{lx}" y="{wy}" width="{pw}" height="{wh}" rx="10" '
        f'fill="{PANEL}" stroke="{ACCENT_DIM if connected else BORDER}" stroke-width="1.5"/>',
        f'<path d="M{lx+10},{wy} h{pw-20} a10,10 0 0 1 10,10 v{bar_h-10} '
        f'h-{pw} v-{bar_h-10} a10,10 0 0 1 10,-10 z" fill="{CHROME}"/>',
        f'<line x1="{lx}" y1="{wy+bar_h}" x2="{lx+pw}" y2="{wy+bar_h}" stroke="{BORDER}"/>',
        f'<circle cx="{lx+26}" cy="{wy+23}" r="5" fill="{ACCENT if connected else MUTED}"/>',
        el("Chrome/157.0.8103.78", 19, FG, x=lx + 42, y=wy + 30, weight="bold"),
        el("real profile", 18, ACCENT if connected else MUTED, x=lx + pw - 24, y=wy + 30, anchor="end"),
        # Address / CDP endpoint strip
        f'<rect x="{lx+24}" y="{wy+68}" width="{pw-48}" height="46" rx="8" '
        f'fill="{TERM_BG}" stroke="{ACCENT_DIM if stage >= 1 else BORDER}"/>',
        f'<rect x="{lx+36}" y="{wy+79}" width="64" height="24" rx="4" fill="{ACCENT_BG}" stroke="{ACCENT}"/>',
        el("CDP", 16, ACCENT, x=lx + 68, y=wy + 97, weight="bold", anchor="middle"),
        el("127.0.0.1:9222", 21, FG if stage >= 1 else MID, x=lx + 116, y=wy + 98),
        el(
            "connected" if connected else ("handshake" if stage >= 1 else "loopback"),
            18,
            ACCENT if connected else (CAP if stage >= 1 else MUTED),
            x=lx + pw - 40,
            y=wy + 97,
            anchor="end",
        ),
    ]
    browser_rows = [
        ("session", "already logged in"),
        ("extension", "none installed"),
        ("watcher", "MutationObserver → CDP binding"),
        ("idle cost", "0 messages on still page"),
    ]
    for i, (k, v) in enumerate(browser_rows):
        ry = wy + 178 + i * 94
        if i > 0:
            left.append(f'<line x1="{lx+28}" y1="{ry-56}" x2="{lx+pw-28}" y2="{ry-56}" stroke="{RULE}"/>')
        left.append(el(k, 21, MUTED, x=lx + 32, y=ry - 14))
        left.append(el(v, 25, FG if connected else MID, x=lx + 32, y=ry + 20))

    # Right: agent calling MCP status tool
    right = [
        window_shadow(rx, wy, pw, wh, rx=10),
        f'<rect x="{rx}" y="{wy}" width="{pw}" height="{wh}" rx="10" '
        f'fill="{TERM_BG}" stroke="{ACCENT if connected else BORDER}" stroke-width="1.5"/>',
        f'<path d="M{rx+10},{wy} h{pw-20} a10,10 0 0 1 10,10 v{bar_h-10} '
        f'h-{pw} v-{bar_h-10} a10,10 0 0 1 10,-10 z" fill="{CHROME}"/>',
        f'<line x1="{rx}" y1="{wy+bar_h}" x2="{rx+pw}" y2="{wy+bar_h}" stroke="{BORDER}"/>',
        el("MCP  ·  status tool", 19, CAP, x=rx + 24, y=wy + 30),
        el("stdio", 18, MUTED, x=rx + pw - 24, y=wy + 30, anchor="end"),
    ]
    rtx = rx + 32
    ry0 = wy + bar_h + 44
    call_txt = "agent → call status"
    if stage == 0:
        right.append(el(call_txt, 24, MID, x=rtx, y=ry0))
        cx = rtx + text_w(call_txt, 24) + 6
        right.append(f'<rect x="{cx:.1f}" y="{ry0 - 20}" width="13" height="24" rx="1.5" fill="{ACCENT}"/>')
    elif stage == 1:
        right.append(el(call_txt, 24, ACCENT, x=rtx, y=ry0, weight="bold"))
        right.append(el("connecting 127.0.0.1:9222 ...", 23, MUTED, x=rtx, y=ry0 + 50))
    else:
        right.append(el(call_txt, 24, ACCENT, x=rtx, y=ry0, weight="bold"))
        right.append(f'<line x1="{rx+28}" y1="{ry0+24}" x2="{rx+pw-28}" y2="{ry0+24}" stroke="{RULE}"/>')
        status_lines = [
            ("server", f"cdp-mcp {VERSION} (stdio)", FG),
            ("browser", "Chrome/157.0.8103.78", FG),
            ("endpoint", "127.0.0.1:9222", MID),
            ("process", "1.3 MB rss · 0.0% cpu · 1 pid", MID),
            ("tools", "19 registered", MID),
            ("waits", "push-based · 80 ms coalesce", MID),
            ("state", "attached", ACCENT),
        ]
        visible = 4 if stage == 2 else len(status_lines)
        for i, (k, v, col) in enumerate(status_lines[:visible]):
            ly = ry0 + 74 + i * 56
            right.append(el(k, 22, MUTED, x=rtx, y=ly))
            right.append(el(v, 23, col, x=rtx + 150, y=ly, weight="bold" if col == ACCENT else "normal"))

    return wrap_svg(head + left + right + foot)


def render_waits_frame(active_step):
    """active_step in 0..3 lights one node; 4 reveals callout line 1; 5 settles all."""
    head, foot = shell_chrome(
        3,
        "04 / WAITS",
        "the page pushes",
        "No polling loop. Rust blocks on the socket until the DOM moves.",
    )
    boxes = ["page", "MutationObserver", "CDP binding", "socket read"]
    bw, gap = 340, 80
    body = []
    for i, label in enumerate(boxes):
        bx = X + i * (bw + gap)
        lit = (active_step >= 4) or (i <= active_step)
        cur = (i == active_step)
        fill = PANEL_LIT if cur else PANEL
        stroke = ACCENT if cur else (ACCENT_DIM if lit else BORDER)
        sw = "2.2" if cur else "1.3"
        txt_col = FG if lit else MUTED
        body.append(window_shadow(bx, 372, bw, 138, rx=10))
        body.append(
            f'<rect x="{bx}" y="372" width="{bw}" height="138" rx="10" '
            f'fill="{fill}" stroke="{stroke}" stroke-width="{sw}"/>'
        )
        body.append(
            el(label, 27, txt_col, x=bx + bw // 2, y=451, anchor="middle",
               weight="bold" if cur else "normal")
        )
        if i < 3:
            ax = bx + bw + gap // 2
            arrow_lit = (active_step >= 4) or (i < active_step)
            arrow_col = ACCENT if arrow_lit else MUTED
            body.append(el("→", 40, arrow_col, x=ax, y=454, anchor="middle"))
            if i == active_step - 1 and active_step <= 3:
                body.append(f'<circle cx="{ax}" cy="412" r="5" fill="{ACCENT}"/>')

    bar_h = 122 if active_step >= 5 else (62 if active_step >= 4 else 0)
    if bar_h:
        body.append(f'<rect x="{X}" y="596" width="4" height="{bar_h}" rx="2" fill="{ACCENT}"/>')
    if active_step >= 4:
        body.append(el("still page: 0 messages", 44, ACCENT, x=X + 32, y=646, weight="bold"))
    if active_step >= 5:
        body.append(el("one change: one wake, named", 30, MID, x=X + 32, y=704))
    return wrap_svg(head + body + foot)


def render_tools(lit_rows=4):
    """lit_rows in 1..4 sweeps rows in cleanly; distinguishes core primitives."""
    head, foot = shell_chrome(
        4,
        "05 / TOOLS",
        "19 tools",
        "Every tool resolves its own tab. Nothing to thread through.",
    )
    names = [
        "flow", "idle", "report", "batch", "status",
        "tabs", "eval", "elements", "text", "input",
        "upload", "paste", "images", "clip", "link",
        "image", "download", "profile", "shot",
    ]
    core = {"flow", "idle", "report", "batch", "status"}
    body = []
    cw, ch, hgap, vgap = 304, 76, 20, 20
    for i, name in enumerate(names):
        col, row = i % 5, i // 5
        bx = X + col * (cw + hgap)
        by = 348 + row * (ch + vgap)
        shown = row < lit_rows
        is_core = name in core
        if not shown:
            body.append(
                f'<rect x="{bx}" y="{by}" width="{cw}" height="{ch}" rx="8" '
                f'fill="{BG}" stroke="{BORDER_DIM}"/>'
            )
            continue
        fill = PANEL_LIT if is_core else PANEL
        stroke = ACCENT_DIM if is_core else BORDER
        txt_col = FG if is_core else MID
        body.append(
            f'<rect x="{bx}" y="{by}" width="{cw}" height="{ch}" rx="8" '
            f'fill="{fill}" stroke="{stroke}" stroke-width="1.3"/>'
        )
        if is_core:
            body.append(f'<circle cx="{bx + 26}" cy="{by + 38}" r="4.5" fill="{ACCENT}"/>')
            body.append(el(name, 30, txt_col, x=bx + 44, y=by + 48, weight="bold"))
        else:
            body.append(el(name, 30, txt_col, x=bx + 28, y=by + 48))

    if lit_rows >= 4:
        body.append(f'<circle cx="{X + 8}" cy="788" r="4.5" fill="{ACCENT}"/>')
        body.append(el("orchestration, waits & live status", 22, CAP, x=X + 26, y=796))
    return wrap_svg(head + body + foot)


def render_limits(visible_count=6):
    head, foot = shell_chrome(
        5,
        "06 / LIMITS",
        "v1, stated plainly",
        "Network tracking is the next known gap.",
    )
    items = [
        "Linux x86-64 only",
        "one browser endpoint, real profile",
        "no network awareness yet",
        "80 ms reports · 50 ms text checks",
        "one call at a time",
        "loopback by default, no auth",
    ]
    body = []
    for i, line in enumerate(items[:visible_count]):
        ry = 382 + i * 68
        is_latest = (i == visible_count - 1) and (visible_count < len(items))
        body.append(el("—", 32, ACCENT, x=X, y=ry, weight="bold"))
        body.append(el(line, 34, FG if is_latest else MID, x=X + 46, y=ry))
    return wrap_svg(head + body + foot)


def render_source(cta_lit=True):
    head, foot = shell_chrome(
        6,
        "07 / SOURCE",
        "fork it, go wild",
        f"MIT · cdp-mcp {VERSION} · Linux x86-64 · ./install.sh",
    )
    body = [
        el("tweak it for your machine", 38, MID, y=386),
        el("one ask: a star", 38, MID, y=450),
    ]
    if cta_lit:
        body += [
            window_shadow(X, 544, 780, 96, rx=12),
            f'<rect x="{X}" y="544" width="780" height="96" rx="12" '
            f'fill="{PANEL_LIT}" stroke="{ACCENT}" stroke-width="1.8"/>',
            el("github.com/benkaesy820/cdp-mcp", 34, ACCENT, x=X + 40, y=604, weight="bold"),
        ]
    return wrap_svg(head + body + foot)


# ---------------------------------------------------------------------------
# Declarative Timeline & Scene model (video + audio stay 100% locked in sync)
# ---------------------------------------------------------------------------

@dataclass
class Frame:
    svg: str
    dur: float
    tag: str = ""


@dataclass
class Scene:
    name: str
    frames: list = field(default_factory=list)
    cues: list = field(default_factory=list)  # [(local_t, kind, arg, pan)]
    transition_out: tuple = None              # (xfade_name, dur) or None
    _t: float = 0.0

    @property
    def duration(self):
        return self._t

    def add(self, svg, dur, cue=None, pan=0.0, tag=""):
        if cue is not None:
            self.cues.append((self._t, cue[0], cue[1], pan))
        self.frames.append(Frame(svg=svg, dur=dur, tag=tag))
        self._t += dur


def build_terminal_scene(target_dur=7.50):
    """Build Scene 02 (interactive ./install.sh) with exact verbatim lines."""
    sc = Scene("02-install", transition_out=("wiperight", 0.24))
    lines = []

    def push(dur, cursor=True, cue=None, pan=0.0, tag="core"):
        sc.add(render_terminal_frame(list(lines), cursor_on=cursor), dur, cue=cue, pan=pan, tag=tag)

    def type_into_last(text, per_char=0.052):
        prefix, color, weight = lines[-1]
        for idx, _ in enumerate(text):
            lines[-1] = (prefix + text[: idx + 1], color, weight)
            # Subtle stereo spread across the keyboard
            pan = ((idx % 7) - 3) * 0.08
            push(per_char, cursor=True, cue=("key", idx), pan=pan, tag="type")

    # Initial prompt
    lines.append(("$ ", FG, "bold"))
    push(0.24, cursor=True)

    # ./install.sh
    type_into_last("./install.sh", per_char=0.054)
    push(0.15, cursor=True)

    # Detect harnesses
    lines.append(("Harnesses found: opencode, claude", CAP, "normal"))
    push(0.30, cursor=False, cue=("enter", 0))

    # Harness prompt
    lines.append(("Install for [opencode,claude]: ", MID, "normal"))
    push(0.26, cursor=True)
    type_into_last("opencode", per_char=0.050)
    push(0.14, cursor=True)

    # Scope prompt
    lines.append(("Where should cdp-mcp be available?", CAP, "normal"))
    lines.append(("  1) Every project", MID, "normal"))
    lines.append(("  2) One project only", MID, "normal"))
    lines.append(("Choose [1]: ", MID, "normal"))
    push(0.40, cursor=True, cue=("enter", 0))
    type_into_last("1", per_char=0.06)
    push(0.15, cursor=True)

    # Proceed prompt
    lines.append(("Proceed? [Y/n] ", MID, "normal"))
    push(0.28, cursor=True, cue=("enter", 0))
    type_into_last("y", per_char=0.06)
    push(0.15, cursor=True)

    # Summary + install steps (verbatim)
    lines.append(("    harnesses : opencode", CAP, "normal"))
    lines.append(("    scope     : global", CAP, "normal"))
    lines.append(("    binary    : /home/you/.local/bin/cdp-mcp", CAP, "normal"))
    push(0.38, cursor=False, cue=("enter", 0))

    lines.append((f"binary: /home/you/.local/bin/cdp-mcp (cdp-mcp {VERSION})", MID, "normal"))
    push(0.32, cursor=False, cue=("tick", 0), pan=-0.18)

    lines.append(("skill : /home/you/.config/opencode/skills/cdp-mcp/SKILL.md", MID, "normal"))
    push(0.32, cursor=False, cue=("tick", 1), pan=-0.06)

    lines.append((OPENCODE_CONFIG_LINE, MID, "normal"))
    push(0.36, cursor=False, cue=("tick", 2), pan=0.06)

    lines.append(("browser: Chrome/157.0.8103.78 on 127.0.0.1:9222", ACCENT, "bold"))
    push(0.42, cursor=False, cue=("tick", 3), pan=0.18)

    lines.append(("done. Restart the agent, then call the status tool.", FG, "bold"))
    lines.append(("$ ", FG, "bold"))
    push(0.42, cursor=True, cue=("confirm", 0), tag="done")

    # Clean calm cursor blink to reach exact target_dur
    blink = False
    while sc.duration < target_dur - 0.005:
        step = min(0.42, target_dur - sc.duration)
        push(step, cursor=blink, tag="blink")
        blink = not blink

    return sc


def build_gif_terminal_frames():
    """Dedicated crisp terminal sequence tuned for 15 fps GIF (66.7 ms/frame).

    Steps 2 chars at a time so every frame is >= 1 full GIF frame with zero dropped keys.
    """
    frames = []
    lines = []

    def push(dur, cursor=True):
        frames.append(Frame(render_terminal_frame(list(lines), cursor_on=cursor), dur))

    def type_chunked(text, step=2, per_step=0.08):
        prefix, color, weight = lines[-1]
        for i in range(step, len(text) + step, step):
            lines[-1] = (prefix + text[:i], color, weight)
            push(per_step, cursor=True)

    lines.append(("$ ", FG, "bold"))
    push(0.20, cursor=True)
    type_chunked("./install.sh", step=2, per_step=0.08)
    push(0.14, cursor=True)

    lines.append(("Harnesses found: opencode, claude", CAP, "normal"))
    lines.append(("Install for [opencode,claude]: ", MID, "normal"))
    push(0.24, cursor=True)
    type_chunked("opencode", step=2, per_step=0.08)
    push(0.14, cursor=True)

    lines.append(("Where should cdp-mcp be available?", CAP, "normal"))
    lines.append(("  1) Every project", MID, "normal"))
    lines.append(("  2) One project only", MID, "normal"))
    lines.append(("Choose [1]: ", MID, "normal"))
    push(0.30, cursor=True)
    type_chunked("1", step=1, per_step=0.12)

    lines.append(("Proceed? [Y/n] ", MID, "normal"))
    push(0.24, cursor=True)
    type_chunked("y", step=1, per_step=0.12)

    lines.append(("    harnesses : opencode", CAP, "normal"))
    lines.append(("    scope     : global", CAP, "normal"))
    lines.append(("    binary    : /home/you/.local/bin/cdp-mcp", CAP, "normal"))
    push(0.28, cursor=False)

    lines.append((f"binary: /home/you/.local/bin/cdp-mcp (cdp-mcp {VERSION})", MID, "normal"))
    push(0.24, cursor=False)
    lines.append(("skill : /home/you/.config/opencode/skills/cdp-mcp/SKILL.md", MID, "normal"))
    push(0.24, cursor=False)
    lines.append((OPENCODE_CONFIG_LINE, MID, "normal"))
    push(0.28, cursor=False)
    lines.append(("browser: Chrome/157.0.8103.78 on 127.0.0.1:9222", ACCENT, "bold"))
    push(0.34, cursor=False)
    lines.append(("done. Restart the agent, then call the status tool.", FG, "bold"))
    lines.append(("$ ", FG, "bold"))
    push(0.90, cursor=True)
    return frames


def build_all_scenes():
    """Assemble all 7 scenes totaling exactly 33.00 s."""
    scenes = []

    # 01 / HOOK (3.20 s) — immediate staggered card motion for autoplay feeds
    s1 = Scene("01-hook", transition_out=("fade", 0.16))
    s1.add(render_hook(revealed=0), 0.18)
    s1.add(render_hook(revealed=1, active=0), 0.24, cue=("tick", 0), pan=-0.35)
    s1.add(render_hook(revealed=2, active=1), 0.24, cue=("tick", 1), pan=0.0)
    s1.add(render_hook(revealed=3, active=2), 0.28, cue=("tick", 2), pan=0.35)
    s1.add(render_hook(revealed=3, active=-1), 2.26)
    scenes.append(s1)

    # 02 / INSTALL (7.50 s) — interactive terminal -> signature right-wipe
    s2 = build_terminal_scene(target_dur=7.50)
    scenes.append(s2)

    # 03 / ATTACHED (4.30 s) — browser + live MCP status stream
    s3 = Scene("03-attached", transition_out=("fade", 0.16))
    s3.add(render_attached_frame(0), 0.40)
    s3.add(render_attached_frame(1), 0.45, cue=("tick", 1), pan=0.15)
    s3.add(render_attached_frame(2), 0.45, cue=("tick", 2), pan=0.20)
    s3.add(render_attached_frame(3), 3.00, cue=("ping", 0), pan=0.0)
    scenes.append(s3)

    # 04 / WAITS (4.60 s) — push pipeline sweeps L -> R with stereo audio
    s4 = Scene("04-waits", transition_out=("fade", 0.16))
    s4.add(render_waits_frame(0), 0.34, cue=("tick", 0), pan=-0.45)
    s4.add(render_waits_frame(1), 0.34, cue=("tick", 1), pan=-0.15)
    s4.add(render_waits_frame(2), 0.34, cue=("tick", 2), pan=0.15)
    s4.add(render_waits_frame(3), 0.34, cue=("tick", 3), pan=0.45)
    s4.add(render_waits_frame(4), 0.38, cue=("ping", 1), pan=0.0)
    s4.add(render_waits_frame(5), 2.86)
    scenes.append(s4)

    # 05 / TOOLS (4.30 s) — 4 crisp row sweeps + core primitive highlight
    s5 = Scene("05-tools", transition_out=("fade", 0.16))
    s5.add(render_tools(lit_rows=1), 0.22, cue=("tick", 0), pan=-0.15)
    s5.add(render_tools(lit_rows=2), 0.22, cue=("tick", 1), pan=-0.05)
    s5.add(render_tools(lit_rows=3), 0.22, cue=("tick", 2), pan=0.05)
    s5.add(render_tools(lit_rows=4), 3.64, cue=("tick", 3), pan=0.15)
    scenes.append(s5)

    # 06 / LIMITS (5.30 s) — honest bullets stepped in cleanly
    s6 = Scene("06-limits", transition_out=("fade", 0.18))
    for k in range(1, 6):
        s6.add(render_limits(visible_count=k), 0.22, cue=("soft_tick", k - 1), pan=-0.12 + k * 0.04)
    s6.add(render_limits(visible_count=6), 4.20, cue=("soft_tick", 5), pan=0.12)
    scenes.append(s6)

    # 07 / SOURCE (3.80 s) — clean CTA landing + resolving chord
    s7 = Scene("07-source", transition_out=None)
    s7.add(render_source(cta_lit=False), 0.35)
    s7.add(render_source(cta_lit=True), 3.45, cue=("final", 0), pan=0.0)
    scenes.append(s7)

    return scenes


# ---------------------------------------------------------------------------
# Procedural stereo audio engine (stdlib only, broadcast/social loudness)
# ---------------------------------------------------------------------------

def pan_gains(pan):
    """Equal-power stereo panning (-1.0 left .. +1.0 right)."""
    ang = (max(-1.0, min(1.0, pan)) + 1.0) * (math.pi * 0.25)
    return math.cos(ang), math.sin(ang)


def synthesize_audio(total_dur, cues, wav_path):
    n_total = int(round(total_dur * SR))
    two_pi = 2.0 * math.pi

    # Warm 5.0-second stereo breathing pad (just-intonation A-add9: exact integer cycles -> zero seam)
    tile_n = 5 * SR
    tile_l = array.array("f", [0.0] * tile_n)
    tile_r = array.array("f", [0.0] * tile_n)
    for i in range(tile_n):
        sec = i / SR
        breath = 0.82 + 0.18 * math.sin(two_pi * 0.2 * sec)
        sub = 0.20 * math.sin(two_pi * 110.0 * sec)
        root = 0.42 * math.sin(two_pi * 220.0 * sec)
        fifth_l = 0.24 * math.sin(two_pi * 330.0 * sec)
        fifth_r = 0.24 * math.sin(two_pi * 330.0 * sec + 0.35)
        ninth_l = 0.14 * math.sin(two_pi * 495.0 * sec + 0.55)
        ninth_r = 0.14 * math.sin(two_pi * 495.0 * sec)
        g = 0.022 * breath
        tile_l[i] = (sub + root + fifth_l + ninth_l) * g
        tile_r[i] = (sub + root + fifth_r + ninth_r) * g

    left = array.array("f")
    right = array.array("f")
    full_tiles, rem = divmod(n_total, tile_n)
    for _ in range(full_tiles):
        left.extend(tile_l)
        right.extend(tile_r)
    if rem:
        left.extend(tile_l[:rem])
        right.extend(tile_r[:rem])

    # Smooth fade-in (0.7 s) and fade-out (1.2 s) on the ambient bed
    fi = min(int(0.7 * SR), n_total)
    for i in range(fi):
        g = i / fi
        left[i] *= g
        right[i] *= g
    fo = min(int(1.2 * SR), n_total)
    for i in range(fo):
        g = i / fo
        idx = n_total - 1 - i
        left[idx] *= g
        right[idx] *= g

    def add_mono_panned(start, sample_fn, length, pan):
        gl, gr = pan_gains(pan)
        for i in range(length):
            pos = start + i
            if pos >= n_total:
                break
            s = sample_fn(i)
            left[pos] += s * gl
            right[pos] += s * gr

    def mix_key(t0, idx, pan):
        start = int(t0 * SR)
        length = int(0.020 * SR)
        freq = 190.0 + (idx % 5) * 13.0
        state = (idx * 1103515245 + 12345) & 0x7FFFFFFF
        lp = 0.0
        gl, gr = pan_gains(pan)
        for i in range(length):
            pos = start + i
            if pos >= n_total:
                break
            u = i / SR
            state = (state * 1103515245 + 12345) & 0x7FFFFFFF
            noise = (state / 0x3FFFFFFF) - 1.0
            lp = 0.52 * lp + 0.48 * noise
            # Crisp mechanical click + woody body
            s = (
                0.19 * lp * math.exp(-u / 0.0026)
                + 0.11 * math.sin(two_pi * freq * u) * math.exp(-u / 0.0055)
            )
            left[pos] += s * gl
            right[pos] += s * gr

    def mix_enter(t0, pan):
        start = int(t0 * SR)
        length = int(0.032 * SR)
        state = 987654321
        lp = 0.0
        gl, gr = pan_gains(pan)
        for i in range(length):
            pos = start + i
            if pos >= n_total:
                break
            u = i / SR
            state = (state * 1103515245 + 12345) & 0x7FFFFFFF
            noise = (state / 0x3FFFFFFF) - 1.0
            lp = 0.64 * lp + 0.36 * noise
            s = (
                0.22 * lp * math.exp(-u / 0.0040)
                + 0.18 * math.sin(two_pi * 136.0 * u) * math.exp(-u / 0.0095)
            )
            left[pos] += s * gl
            right[pos] += s * gr

    def mix_tick(t0, step, pan, amp=0.11):
        start = int(t0 * SR)
        length = int(0.020 * SR)
        freq = 780.0 + step * 72.0
        gl, gr = pan_gains(pan)
        for i in range(length):
            pos = start + i
            if pos >= n_total:
                break
            u = i / SR
            att = min(1.0, u / 0.0014)
            s = amp * att * math.exp(-u / 0.0052) * math.sin(two_pi * freq * u)
            left[pos] += s * gl
            right[pos] += s * gr

    def mix_wipe(t0, dur):
        """Left-to-right stereo swoosh synced to wiperight."""
        start = int(t0 * SR)
        length = int(dur * SR)
        state = 424242
        lp = 0.0
        for i in range(length):
            pos = start + i
            if pos >= n_total:
                break
            frac = i / length
            u = i / SR
            state = (state * 1103515245 + 12345) & 0x7FFFFFFF
            noise = (state / 0x3FFFFFFF) - 1.0
            lp = 0.86 * lp + 0.14 * noise
            env = math.sin(math.pi * frac) ** 2
            freq = 210.0 + 135.0 * frac
            s = env * (0.095 * lp + 0.055 * math.sin(two_pi * freq * u))
            gl, gr = pan_gains(-0.80 + 1.60 * frac)
            left[pos] += s * gl
            right[pos] += s * gr

    def mix_fade_whoosh(t0, dur):
        start = int(t0 * SR)
        length = int(dur * SR)
        state = 135791
        lp = 0.0
        for i in range(length):
            pos = start + i
            if pos >= n_total:
                break
            frac = i / length
            state = (state * 1103515245 + 12345) & 0x7FFFFFFF
            noise = (state / 0x3FFFFFFF) - 1.0
            lp = 0.92 * lp + 0.08 * noise
            s = (math.sin(math.pi * frac) ** 2) * 0.028 * lp
            left[pos] += s
            right[pos] += s

    def mix_tone(t0, freq, dur, amp, pan=0.0):
        start = int(t0 * SR)
        length = int(dur * SR)
        gl, gr = pan_gains(pan)
        for i in range(length):
            pos = start + i
            if pos >= n_total:
                break
            u = i / SR
            att = min(1.0, u / 0.004)
            dec = math.exp(-u / (dur * 0.34))
            # Subtle warm 2nd harmonic
            s = amp * att * dec * (
                0.85 * math.sin(two_pi * freq * u)
                + 0.15 * math.sin(two_pi * freq * 2.0 * u)
            )
            left[pos] += s * gl
            right[pos] += s * gr

    for abs_t, kind, arg, pan in cues:
        if kind == "key":
            mix_key(abs_t, arg, pan)
        elif kind == "enter":
            mix_enter(abs_t, pan)
        elif kind == "tick":
            mix_tick(abs_t, arg, pan, amp=0.115)
        elif kind == "soft_tick":
            mix_tick(abs_t, arg, pan, amp=0.068)
        elif kind == "wipe":
            mix_wipe(abs_t, arg)
        elif kind == "fade":
            mix_fade_whoosh(abs_t, arg)
        elif kind == "confirm":
            mix_tone(abs_t, 523.25, 0.22, 0.19, pan=-0.12)
            mix_tone(abs_t + 0.075, 659.25, 0.30, 0.21, pan=0.12)
        elif kind == "ping":
            freq = 587.33 if arg == 0 else 659.25
            mix_tone(abs_t, freq, 0.24, 0.17, pan=pan)
        elif kind == "final":
            mix_tone(abs_t, 330.00, 0.42, 0.12, pan=-0.22)
            mix_tone(abs_t + 0.045, 440.00, 0.46, 0.14, pan=0.0)
            mix_tone(abs_t + 0.090, 554.37, 0.54, 0.15, pan=0.22)

    # Normalize cleanly to -2.5 dBFS peak with smooth tanh soft-ceiling
    peak = max(
        max((abs(v) for v in left), default=0.001),
        max((abs(v) for v in right), default=0.001),
    )
    target_peak = 0.75  # ~ -2.5 dBFS (clear on phone speakers without clipping)
    gain = min(2.4, target_peak / peak) if peak > 1e-6 else 1.0

    pcm = bytearray(n_total * 4)
    pack_into = struct.pack_into
    tanh = math.tanh
    for i in range(n_total):
        lv = tanh(left[i] * gain)
        rv = tanh(right[i] * gain)
        pack_into("<hh", pcm, i * 4, int(lv * 32767.0), int(rv * 32767.0))

    with wave.open(str(wav_path), "wb") as wf:
        wf.setnchannels(2)
        wf.setsampwidth(2)
        wf.setframerate(SR)
        wf.writeframes(pcm)


# ---------------------------------------------------------------------------
# Rendering, caching & ffmpeg pipelines
# ---------------------------------------------------------------------------

def need(cmd):
    if shutil.which(cmd) is None:
        sys.exit(f"missing required tool: {cmd}")


def run(cmd):
    res = subprocess.run(cmd, capture_output=True, text=True)
    if res.returncode != 0:
        sys.stderr.write(res.stderr)
        sys.exit(res.returncode)
    return res.stdout


def svg_hash(svg_text):
    return hashlib.sha1(svg_text.encode("utf-8")).hexdigest()[:16]


def render_cached_png(item):
    h, svg_text = item
    png_path = CACHE / f"{h}.png"
    if png_path.exists():
        return h, png_path, False
    svg_path = CACHE / f"{h}.svg"
    svg_path.write_text(svg_text)
    run(["rsvg-convert", "-w", "1920", "-h", "1080", str(svg_path), "-o", str(png_path)])
    return h, png_path, True


def rasterize_svgs(svg_list):
    """Deduplicate by content hash and rasterize only cache misses in parallel."""
    unique = {}
    for svg in svg_list:
        h = svg_hash(svg)
        if h not in unique:
            unique[h] = svg

    png_map = {}
    rendered = 0
    workers = min(os.cpu_count() or 4, 8)
    with ThreadPoolExecutor(max_workers=workers) as pool:
        for h, png_path, did_render in pool.map(render_cached_png, unique.items()):
            png_map[h] = png_path
            if did_render:
                rendered += 1
    return png_map, rendered, len(unique)


def write_concat(path, entries, tail_pad=0.0):
    lines = []
    for idx, (png, dur) in enumerate(entries):
        d = dur + (tail_pad if idx == len(entries) - 1 else 0.0)
        lines += [f"file '{png}'", f"duration {d:.4f}"]
    # Concat demuxer quirk: repeat the last file so its full duration holds.
    lines.append(f"file '{entries[-1][0]}'")
    path.write_text("\n".join(lines) + "\n")


def encode_mp4(scenes, png_map, fps, fast=False):
    # Compute global timestamps + transition cues automatically from scenes
    all_cues = []
    t_cursor = 0.0
    list_paths = []

    for idx, sc in enumerate(scenes):
        for local_t, kind, arg, pan in sc.cues:
            all_cues.append((t_cursor + local_t, kind, arg, pan))

        tail_pad = 0.0
        if sc.transition_out is not None and idx < len(scenes) - 1:
            tr_name, tr_dur = sc.transition_out
            # Extend outgoing last frame past the xfade overlap so xfade never starves
            tail_pad = tr_dur + 0.08
            cue_kind = "wipe" if tr_name == "wiperight" else "fade"
            all_cues.append((t_cursor + sc.duration, cue_kind, tr_dur, 0.0))

        entries = [(png_map[svg_hash(f.svg)], f.dur) for f in sc.frames]
        lp = OUT / f"list_{idx:02d}_{sc.name}.txt"
        write_concat(lp, entries, tail_pad=tail_pad)
        list_paths.append(lp)
        t_cursor += sc.duration

    total = t_cursor
    synthesize_audio(total, all_cues, WAV)

    # Build multi-scene xfade filtergraph
    cmd = ["ffmpeg", "-y"]
    for lp in list_paths:
        cmd += ["-f", "concat", "-safe", "0", "-i", str(lp)]

    bar_idx = len(list_paths)
    wav_idx = bar_idx + 1
    cmd += [
        "-f", "lavfi", "-i", f"color=c={BAR_COLOR}:s=1920x4:r={fps}:d={total + 1.0:.3f}",
        "-i", str(WAV),
    ]

    fparts = []
    for i in range(len(scenes)):
        fparts.append(
            f"[{i}:v]fps={fps},format=yuv420p,settb=1/{fps},setpts=PTS-STARTPTS[v{i}]"
        )

    prev = "v0"
    offset = 0.0
    for i in range(len(scenes) - 1):
        offset += scenes[i].duration
        tr_name, tr_dur = scenes[i].transition_out or ("fade", 0.16)
        nxt = "main" if i == len(scenes) - 2 else f"x{i}"
        fparts.append(
            f"[{prev}][v{i+1}]xfade=transition={tr_name}:duration={tr_dur:.3f}:offset={offset:.3f}[{nxt}]"
        )
        prev = nxt

    fparts.append(f"[{bar_idx}:v]format=yuv420p[bar]")
    fparts.append(
        f"[main][bar]overlay=x='-w+w*t/{total:.3f}':y=1076:eof_action=repeat[out]"
    )

    preset = "veryfast" if fast else "slow"
    crf = "20" if fast else "18"

    cmd += [
        "-filter_complex", ";".join(fparts),
        "-map", "[out]", "-map", f"{wav_idx}:a",
        "-t", f"{total:.3f}",
        "-c:v", "libx264", "-profile:v", "high", "-level", "4.2",
        "-preset", preset, "-crf", crf, "-g", str(fps * 2),
        "-pix_fmt", "yuv420p",
        "-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709",
        "-c:a", "aac", "-b:a", "192k", "-ar", str(SR),
        "-movflags", "+faststart",
        str(MP4),
    ]
    run(cmd)


def encode_gif(gif_frames, png_map):
    entries = [(png_map[svg_hash(f.svg)], f.dur) for f in gif_frames]
    list_gif = OUT / "list_gif.txt"
    write_concat(list_gif, entries)
    palette = OUT / "palette.png"

    run([
        "ffmpeg", "-y", "-f", "concat", "-safe", "0", "-i", str(list_gif),
        "-vf", "fps=15,scale=800:-1:flags=lanczos,palettegen=max_colors=128:stats_mode=diff",
        str(palette),
    ])
    run([
        "ffmpeg", "-y", "-f", "concat", "-safe", "0", "-i", str(list_gif),
        "-i", str(palette),
        "-lavfi",
        "fps=15,scale=800:-1:flags=lanczos[x];"
        "[x][1:v]paletteuse=dither=bayer:bayer_scale=2:diff_mode=rectangle",
        "-loop", "0",
        str(GIF),
    ])


def main():
    parser = argparse.ArgumentParser(description="Build cdp-mcp launch video and README GIF.")
    parser.add_argument("--fast", action="store_true", help="Use faster x264 preset for quick iteration")
    parser.add_argument("--fps", type=int, default=60, help="MP4 frame rate (default: 60)")
    parser.add_argument("--only-mp4", action="store_true", help="Build only the MP4")
    parser.add_argument("--only-gif", action="store_true", help="Build only the README GIF")
    parser.add_argument("--clean", action="store_true", help="Clear cached SVGs/PNGs before building")
    args = parser.parse_args()

    for c in ("rsvg-convert", "ffmpeg", "ffprobe"):
        need(c)

    if args.clean and CACHE.exists():
        shutil.rmtree(CACHE)
    OUT.mkdir(parents=True, exist_ok=True)
    CACHE.mkdir(parents=True, exist_ok=True)

    do_mp4 = not args.only_gif
    do_gif = not args.only_mp4

    scenes = build_all_scenes() if do_mp4 else []
    gif_frames = []
    if do_gif:
        gif_frames = build_gif_terminal_frames() + [
            Frame(render_attached_frame(1), 0.35),
            Frame(render_attached_frame(3), 1.75),
            Frame(render_hook(revealed=3, active=-1), 1.65),
        ]

    all_svgs = [f.svg for sc in scenes for f in sc.frames] + [f.svg for f in gif_frames]
    png_map, rendered, unique_count = rasterize_svgs(all_svgs)
    print(f"Frames: {unique_count} unique ({rendered} rendered, {unique_count - rendered} cached)")

    if do_mp4:
        encode_mp4(scenes, png_map, fps=args.fps, fast=args.fast)
        info = run([
            "ffprobe", "-v", "error", "-show_entries", "format=duration,size",
            "-of", "default=nw=1", str(MP4),
        ])
        print(f"MP4: {MP4}")
        print(info.strip())

    if do_gif:
        encode_gif(gif_frames, png_map)
        print(f"GIF: {GIF} ({GIF.stat().st_size / 1024:.1f} KB)")


if __name__ == "__main__":
    main()