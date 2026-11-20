#!/usr/bin/env python3
"""Render the cdp-mcp info video (with terminal emulation + audio) and README GIF.

Every number and terminal line here is measured on the real machine, typeset rather
than filmed so small monospace stays crisp at X's bitrate and GitHub's GIF scale.

Needs: python3, rsvg-convert, ffmpeg, ffprobe, Noto Sans Mono.
Outputs:
  video/out/cdp-mcp-0.10.7.mp4          (1920x1080, 30 fps, AAC audio, 33.0 s)
  video/out/cdp-mcp-0.10.7-preview.gif  (800x450, 12 fps, looping, 8.6 s)
"""

import array
from concurrent.futures import ThreadPoolExecutor
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
CARDS = OUT / "cards"
VERSION = "0.10.7"
MP4 = OUT / f"cdp-mcp-{VERSION}.mp4"
GIF = OUT / f"cdp-mcp-{VERSION}-preview.gif"
WAV = OUT / f"cdp-mcp-{VERSION}.wav"

# Update this one constant if install.sh is updated from mcp.servers.cdp to mcp.cdp.
OPENCODE_CONFIG_LINE = "config: /home/you/.config/opencode/opencode.json (mcp.servers.cdp)"

BG = "#0e1012"
PANEL = "#14181d"
TERM_BG = "#101317"
CHROME = "#181d23"
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
W = R - X  # 1600 px grid
FONT = "Noto Sans Mono, monospace"
SR = 48000
WIPE_DUR = 0.22


def esc(s):
    return html.escape(s, quote=True)


def el(s, size, fill, x=X, y=0, weight="normal", anchor="start"):
    return (
        f'<text x="{x}" y="{y}" font-family="{FONT}" font-size="{size}" '
        f'font-weight="{weight}" fill="{fill}" text-anchor="{anchor}" '
        f'xml:space="preserve">{esc(s)}</text>'
    )


def shell_chrome(kicker, title, caption, compact=False):
    k_y = 104 if compact else 148
    t_y = 182 if compact else 258
    t_sz = 64 if compact else 84
    parts = [
        f'<rect width="1920" height="1080" fill="{BG}"/>',
        f'<rect x="0" y="0" width="1920" height="4" fill="{ACCENT}"/>',
        el(kicker, 24 if compact else 26, ACCENT, y=k_y),
        el(title, t_sz, FG, y=t_y, weight="600"),
    ]
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


# ---------------------------------------------------------------------------
# Card renderers
# ---------------------------------------------------------------------------

def render_hook():
    head, foot = shell_chrome(
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
        body.append(
            f'<rect x="{bx}" y="356" width="{cw}" height="318" rx="12" '
            f'fill="{PANEL}" stroke="{BORDER}"/>'
        )
        body.append(el(big, 68, ACCENT, x=bx + 36, y=476, weight="600"))
        body.append(el(sub1, 26, FG, x=bx + 36, y=552))
        body.append(el(sub2, 22, CAP, x=bx + 36, y=598))
    return wrap_svg(head + body + foot)


def render_terminal_frame(lines, cursor_on=True):
    """Render a Wayland/niri-styled terminal window with exact installer lines."""
    head, foot = shell_chrome(
        "02 / INSTALL",
        "./install.sh",
        "Detects OpenCode & Claude Code · writes binary, skill, and MCP config",
        compact=True,
    )
    wx, wy, ww, wh = X, 218, W, 622
    bar_h = 44
    body = [
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
    char_w = font_sz * 0.602  # Noto Sans Mono advance width

    for idx, (text, color, weight) in enumerate(lines):
        ly = y0 + idx * line_h
        body.append(el(text, font_sz, color, x=tx, y=ly, weight=weight))

    if cursor_on and lines:
        last_text = lines[-1][0]
        cx = tx + len(last_text) * char_w + 2
        cy = y0 + (len(lines) - 1) * line_h - 19
        body.append(f'<rect x="{cx:.1f}" y="{cy}" width="13" height="24" fill="{ACCENT}" opacity="0.9"/>')

    return wrap_svg(head + body + foot)


def render_attached_frame(stage):
    """Split view: attached real browser (left) + MCP status result (right)."""
    head, foot = shell_chrome(
        "03 / ATTACHED",
        "real browser, live status",
        "Your logged-in browser session. No extension, no relay, no fresh profile.",
        compact=True,
    )
    wy, wh = 224, 608
    pw, gap = 780, 40
    lx, rx = X, X + pw + gap
    bar_h = 46

    # Left: clean Chromium/Helium window chrome
    left = [
        f'<rect x="{lx}" y="{wy}" width="{pw}" height="{wh}" rx="10" '
        f'fill="{PANEL}" stroke="{BORDER}" stroke-width="1.5"/>',
        f'<path d="M{lx+10},{wy} h{pw-20} a10,10 0 0 1 10,10 v{bar_h-10} '
        f'h-{pw} v-{bar_h-10} a10,10 0 0 1 10,-10 z" fill="{CHROME}"/>',
        f'<line x1="{lx}" y1="{wy+bar_h}" x2="{lx+pw}" y2="{wy+bar_h}" stroke="{BORDER}"/>',
        el("Chrome/157.0.8103.78", 19, FG, x=lx + 24, y=wy + 30, weight="600"),
        el("real profile", 18, ACCENT if stage >= 2 else MUTED, x=lx + pw - 24, y=wy + 30, anchor="end"),
        # Address / CDP endpoint strip
        f'<rect x="{lx+24}" y="{wy+68}" width="{pw-48}" height="46" rx="8" '
        f'fill="{TERM_BG}" stroke="{BORDER}"/>',
        f'<rect x="{lx+36}" y="{wy+79}" width="64" height="24" rx="4" fill="#183021" stroke="{ACCENT}"/>',
        el("CDP", 16, ACCENT, x=lx + 68, y=wy + 97, weight="600", anchor="middle"),
        el("127.0.0.1:9222", 21, MID, x=lx + 116, y=wy + 98),
        el("loopback" if stage < 2 else "connected", 18, ACCENT if stage >= 2 else MUTED,
           x=lx + pw - 40, y=wy + 97, anchor="end"),
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
        left.append(el(v, 25, FG if stage >= 2 else MID, x=lx + 32, y=ry + 20))

    # Right: agent calling MCP status tool
    right = [
        f'<rect x="{rx}" y="{wy}" width="{pw}" height="{wh}" rx="10" '
        f'fill="{TERM_BG}" stroke="{ACCENT if stage >= 2 else BORDER}" stroke-width="1.5"/>',
        f'<path d="M{rx+10},{wy} h{pw-20} a10,10 0 0 1 10,10 v{bar_h-10} '
        f'h-{pw} v-{bar_h-10} a10,10 0 0 1 10,-10 z" fill="{CHROME}"/>',
        f'<line x1="{rx}" y1="{wy+bar_h}" x2="{rx+pw}" y2="{wy+bar_h}" stroke="{BORDER}"/>',
        el("MCP  ·  status tool", 19, CAP, x=rx + 24, y=wy + 30),
        el("stdio", 18, MUTED, x=rx + pw - 24, y=wy + 30, anchor="end"),
    ]
    rtx = rx + 32
    ry0 = wy + bar_h + 44
    if stage == 0:
        right.append(el("agent → call status", 24, MUTED, x=rtx, y=ry0))
        right.append(f'<rect x="{rtx + 286}" y="{ry0 - 20}" width="13" height="24" fill="{ACCENT}"/>')
    elif stage == 1:
        right.append(el("agent → call status", 24, ACCENT, x=rtx, y=ry0, weight="600"))
        right.append(el("connecting 127.0.0.1:9222 ...", 23, MUTED, x=rtx, y=ry0 + 48))
    else:
        right.append(el("agent → call status", 24, ACCENT, x=rtx, y=ry0, weight="600"))
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
        for i, (k, v, col) in enumerate(status_lines):
            ly = ry0 + 74 + i * 56
            right.append(el(k, 22, MUTED, x=rtx, y=ly))
            right.append(el(v, 23, col, x=rtx + 150, y=ly, weight="600" if col == ACCENT else "normal"))

    return wrap_svg(head + left + right + foot)


def render_waits_frame(active_step):
    """active_step in 0..3 highlights one box; 4 highlights the full settled path."""
    head, foot = shell_chrome(
        "04 / WAITS",
        "the page pushes",
        "No polling loop. Rust blocks on the socket until the DOM moves.",
    )
    boxes = ["page", "MutationObserver", "CDP binding", "socket read"]
    bw, gap = 340, 80
    body = []
    for i, label in enumerate(boxes):
        bx = X + i * (bw + gap)
        lit = (active_step == 4) or (i <= active_step)
        cur = (i == active_step)
        stroke = ACCENT if cur else (BORDER if not lit else "#3d6b47")
        sw = "2.2" if cur else "1.2"
        txt_col = FG if lit else MUTED
        body.append(
            f'<rect x="{bx}" y="376" width="{bw}" height="136" rx="10" '
            f'fill="{PANEL}" stroke="{stroke}" stroke-width="{sw}"/>'
        )
        body.append(el(label, 27, txt_col, x=bx + bw // 2, y=454, anchor="middle",
                       weight="600" if cur else "normal"))
        if i < 3:
            arrow_col = ACCENT if (active_step == 4 or i < active_step) else MUTED
            body.append(el("→", 40, arrow_col, x=bx + bw + gap // 2, y=458, anchor="middle"))

    body.append(f'<rect x="{X}" y="596" width="4" height="122" fill="{ACCENT}"/>')
    body.append(el("still page: 0 messages", 44, ACCENT, x=X + 32, y=646, weight="600"))
    body.append(el("one change: one wake, named", 30, MID, x=X + 32, y=704))
    return wrap_svg(head + body + foot)


def render_tools():
    head, foot = shell_chrome(
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
    body = []
    cw, ch, hgap, vgap = 304, 76, 20, 20
    for i, name in enumerate(names):
        col, row = i % 5, i // 5
        bx = X + col * (cw + hgap)
        by = 360 + row * (ch + vgap)
        body.append(
            f'<rect x="{bx}" y="{by}" width="{cw}" height="{ch}" rx="8" '
            f'fill="{PANEL}" stroke="{BORDER}"/>'
        )
        body.append(el(name, 30, MID, x=bx + 28, y=by + 48))
    return wrap_svg(head + body + foot)


def render_limits():
    head, foot = shell_chrome(
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
    for i, line in enumerate(items):
        ry = 382 + i * 68
        body.append(el("—", 32, ACCENT, x=X, y=ry))
        body.append(el(line, 34, MID, x=X + 46, y=ry))
    return wrap_svg(head + body + foot)


def render_source():
    head, foot = shell_chrome(
        "07 / SOURCE",
        "fork it, go wild",
        f"MIT · cdp-mcp {VERSION} · Linux x86-64 · ./install.sh",
    )
    body = [
        el("tweak it for your machine", 38, MID, y=386),
        el("one ask: a star", 38, MID, y=450),
        f'<rect x="{X}" y="544" width="760" height="96" rx="12" '
        f'fill="{PANEL}" stroke="{ACCENT}" stroke-width="1.5"/>',
        el("github.com/benkaesy820/cdp-mcp", 34, ACCENT, x=X + 40, y=604, weight="600"),
    ]
    return wrap_svg(head + body + foot)


# ---------------------------------------------------------------------------
# Terminal script timeline (shared by MP4, audio cues, and README GIF)
# ---------------------------------------------------------------------------

def build_terminal_sequence(t_offset):
    """Return (frames, cues) where frames = [(id, dur, svg)], cues = [(abs_t, kind, arg)]."""
    frames = []
    cues = []
    lines = []
    t = 0.0
    fid = 0

    def push(dur, cursor=True, cue=None):
        nonlocal t, fid
        if cue is not None:
            cues.append((t_offset + t, cue[0], cue[1]))
        fid += 1
        frames.append((f"02-term-{fid:03d}", dur, render_terminal_frame(list(lines), cursor_on=cursor)))
        t += dur

    def type_into_last(text, per_char=0.055):
        prefix, color, weight = lines[-1]
        for idx, ch in enumerate(text):
            lines[-1] = (prefix + text[: idx + 1], color, weight)
            push(per_char, cursor=True, cue=("key", idx))

    # Initial prompt + brief blink
    lines.append(("$ ", FG, "600"))
    push(0.25, cursor=True)

    # Type ./install.sh
    type_into_last("./install.sh", per_char=0.058)
    push(0.16, cursor=True)

    # Enter -> detect harnesses
    lines.append(("Harnesses found: opencode, claude", CAP, "normal"))
    push(0.32, cursor=False, cue=("enter", 0))

    # Prompt 1: harness selection
    lines.append(("Install for [opencode,claude]: ", MID, "normal"))
    push(0.28, cursor=True)
    type_into_last("opencode", per_char=0.055)
    push(0.15, cursor=True)

    # Prompt 2: scope
    lines.append(("Where should cdp-mcp be available?", CAP, "normal"))
    lines.append(("  1) Every project", MID, "normal"))
    lines.append(("  2) One project only", MID, "normal"))
    lines.append(("Choose [1]: ", MID, "normal"))
    push(0.42, cursor=True, cue=("enter", 0))
    type_into_last("1", per_char=0.06)
    push(0.16, cursor=True)

    # Prompt 3: proceed
    lines.append(("Proceed? [Y/n] ", MID, "normal"))
    push(0.30, cursor=True, cue=("enter", 0))
    type_into_last("y", per_char=0.06)
    push(0.16, cursor=True)

    # Install summary block (verbatim lines)
    lines.append(("    harnesses : opencode", CAP, "normal"))
    lines.append(("    scope     : global", CAP, "normal"))
    lines.append(("    binary    : /home/you/.local/bin/cdp-mcp", CAP, "normal"))
    push(0.42, cursor=False, cue=("enter", 0))

    lines.append((f"binary: /home/you/.local/bin/cdp-mcp (cdp-mcp {VERSION})", MID, "normal"))
    push(0.35, cursor=False, cue=("tick", 0))

    lines.append(("skill : /home/you/.config/opencode/skills/cdp-mcp/SKILL.md", MID, "normal"))
    push(0.35, cursor=False, cue=("tick", 1))

    lines.append((OPENCODE_CONFIG_LINE, MID, "normal"))
    push(0.40, cursor=False, cue=("tick", 2))

    lines.append(("browser: Chrome/157.0.8103.78 on 127.0.0.1:9222", ACCENT, "600"))
    push(0.45, cursor=False, cue=("tick", 3))

    lines.append(("done. Restart the agent, then call the status tool.", FG, "600"))
    lines.append(("$ ", FG, "600"))
    push(0.45, cursor=True, cue=("confirm", 0))

    # Calm cursor blink hold until exact 8.500 s + WIPE_DUR tail overlap
    target = 8.500 + WIPE_DUR
    blink = False
    while t < target - 0.005:
        step = min(0.45, target - t)
        push(step, cursor=blink)
        blink = not blink

    return frames, cues


# ---------------------------------------------------------------------------
# Procedural audio synthesis (stdlib only, no external files)
# ---------------------------------------------------------------------------

def synthesize_audio(total_dur, cues, wav_path):
    n_total = int(round(total_dur * SR))

    # Warm, very low ambient bed (-35 dBFS): exact integer cycles in 1.0 s for clean tiling
    one_sec = array.array("f", [0.0] * SR)
    two_pi = 2.0 * math.pi
    for i in range(SR):
        sec = i / SR
        val = (
            0.55 * math.sin(two_pi * 110.0 * sec)
            + 0.30 * math.sin(two_pi * 165.0 * sec)
            + 0.15 * math.sin(two_pi * 220.0 * sec)
        )
        one_sec[i] = val * 0.011

    buf = array.array("f")
    full_secs, rem = divmod(n_total, SR)
    for _ in range(full_secs):
        buf.extend(one_sec)
    if rem:
        buf.extend(one_sec[:rem])

    # Smooth fade-in (0.8 s) and fade-out (1.0 s) on the bed
    fi = min(int(0.8 * SR), n_total)
    for i in range(fi):
        buf[i] *= i / fi
    fo = min(int(1.0 * SR), n_total)
    for i in range(fo):
        buf[n_total - 1 - i] *= i / fo

    def mix_key(t0, idx):
        start = int(t0 * SR)
        length = int(0.018 * SR)
        freq = 185.0 + (idx % 5) * 11.0
        state = (idx * 1103515245 + 12345) & 0x7FFFFFFF
        lp = 0.0
        for i in range(length):
            pos = start + i
            if pos >= n_total:
                break
            u = i / SR
            state = (state * 1103515245 + 12345) & 0x7FFFFFFF
            noise = (state / 0x3FFFFFFF) - 1.0
            lp = 0.55 * lp + 0.45 * noise
            env_n = math.exp(-u / 0.0028)
            env_b = math.exp(-u / 0.0055)
            sample = (0.085 * lp * env_n) + (0.045 * math.sin(two_pi * freq * u) * env_b)
            buf[pos] += sample

    def mix_enter(t0):
        start = int(t0 * SR)
        length = int(0.030 * SR)
        state = 987654321
        lp = 0.0
        for i in range(length):
            pos = start + i
            if pos >= n_total:
                break
            u = i / SR
            state = (state * 1103515245 + 12345) & 0x7FFFFFFF
            noise = (state / 0x3FFFFFFF) - 1.0
            lp = 0.65 * lp + 0.35 * noise
            sample = (0.095 * lp * math.exp(-u / 0.004)) + (
                0.075 * math.sin(two_pi * 132.0 * u) * math.exp(-u / 0.009)
            )
            buf[pos] += sample

    def mix_tick(t0, step=0):
        start = int(t0 * SR)
        length = int(0.016 * SR)
        freq = 760.0 + step * 75.0
        for i in range(length):
            pos = start + i
            if pos >= n_total:
                break
            u = i / SR
            att = min(1.0, u / 0.0015)
            buf[pos] += 0.038 * att * math.exp(-u / 0.0045) * math.sin(two_pi * freq * u)

    def mix_wipe(t0):
        start = int(t0 * SR)
        length = int(WIPE_DUR * SR)
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
            lp = 0.88 * lp + 0.12 * noise
            env = math.sin(math.pi * frac) ** 2
            freq = 210.0 + 110.0 * frac
            buf[pos] += env * (0.028 * lp + 0.018 * math.sin(two_pi * freq * u))

    def mix_tone(t0, freq, dur, amp):
        start = int(t0 * SR)
        length = int(dur * SR)
        for i in range(length):
            pos = start + i
            if pos >= n_total:
                break
            u = i / SR
            att = min(1.0, u / 0.004)
            dec = math.exp(-u / (dur * 0.34))
            buf[pos] += amp * att * dec * math.sin(two_pi * freq * u)

    for abs_t, kind, arg in cues:
        if kind == "key":
            mix_key(abs_t, arg)
        elif kind == "enter":
            mix_enter(abs_t)
        elif kind == "tick":
            mix_tick(abs_t, arg)
        elif kind == "wipe":
            mix_wipe(abs_t)
        elif kind == "confirm":
            mix_tone(abs_t, 523.25, 0.18, 0.085)
            mix_tone(abs_t + 0.075, 659.25, 0.26, 0.090)
        elif kind == "ping":
            mix_tone(abs_t, 587.33, 0.19, 0.070)
        elif kind == "final":
            mix_tone(abs_t, 392.00, 0.22, 0.055)
            mix_tone(abs_t + 0.05, 587.33, 0.28, 0.060)

    pcm = bytearray(n_total * 2)
    pack_into = struct.pack_into
    for i in range(n_total):
        v = buf[i]
        if v > 0.95:
            v = 0.95
        elif v < -0.95:
            v = -0.95
        pack_into("<h", pcm, i * 2, int(v * 32767.0))

    with wave.open(str(wav_path), "wb") as wf:
        wf.setnchannels(1)
        wf.setsampwidth(2)
        wf.setframerate(SR)
        wf.writeframes(pcm)


# ---------------------------------------------------------------------------
# Build orchestration
# ---------------------------------------------------------------------------

def need(cmd):
    if shutil.which(cmd) is None:
        sys.exit(f"missing {cmd}")


def run(cmd):
    res = subprocess.run(cmd, capture_output=True, text=True)
    if res.returncode != 0:
        sys.stderr.write(res.stderr)
        sys.exit(res.returncode)
    return res.stdout


def render_svg_to_png(item):
    fid, svg_text = item
    svg_path = CARDS / f"{fid}.svg"
    png_path = CARDS / f"{fid}.png"
    svg_path.write_text(svg_text)
    run(["rsvg-convert", "-w", "1920", "-h", "1080", str(svg_path), "-o", str(png_path)])
    return fid, png_path


def write_concat(path, entries):
    lines = []
    for png, dur in entries:
        lines += [f"file '{png}'", f"duration {dur:.4f}"]
    # Concat demuxer quirk: repeat the last file so its full duration holds.
    lines.append(f"file '{entries[-1][0]}'")
    path.write_text("\n".join(lines) + "\n")


def main():
    for c in ("rsvg-convert", "ffmpeg", "ffprobe"):
        need(c)
    OUT.mkdir(parents=True, exist_ok=True)
    CARDS.mkdir(parents=True, exist_ok=True)

    # Segment A: 01-hook (3.0 s) + 02-terminal (8.5 s + WIPE_DUR overlap)
    hook_dur = 3.0
    term_frames, cues = build_terminal_sequence(t_offset=hook_dur)

    seg_a_spec = [("01-hook", hook_dur, render_hook())] + term_frames
    wipe_offset = hook_dur + 8.5  # 11.50 s

    # Segment B: starts at 11.50 s (first 0.22 s is the right-wipe)
    seg_b_spec = [
        ("03-attached-0", 0.45, render_attached_frame(0)),
        ("03-attached-1", 0.50, render_attached_frame(1)),
        ("03-attached-2", 3.05, render_attached_frame(2)),
        ("04-waits-0", 0.38, render_waits_frame(0)),
        ("04-waits-1", 0.38, render_waits_frame(1)),
        ("04-waits-2", 0.38, render_waits_frame(2)),
        ("04-waits-3", 0.38, render_waits_frame(3)),
        ("04-waits-4", 2.98, render_waits_frame(4)),
        ("05-tools", 4.00, render_tools()),
        ("06-limits", 5.50, render_limits()),
        ("07-source", 3.50, render_source()),
    ]
    seg_b_total = sum(d for _, d, _ in seg_b_spec)  # 21.50 s
    total = wipe_offset + seg_b_total               # 33.00 s

    # Add Segment B audio cues at exact absolute timestamps
    cues += [
        (wipe_offset, "wipe", 0),
        (wipe_offset + 0.45, "tick", 1),
        (wipe_offset + 0.95, "ping", 0),
        (wipe_offset + 4.00, "tick", 0),
        (wipe_offset + 4.38, "tick", 1),
        (wipe_offset + 4.76, "tick", 2),
        (wipe_offset + 5.14, "tick", 3),
        (wipe_offset + 8.50, "tick", 0),
        (wipe_offset + 12.50, "tick", 0),
        (wipe_offset + 18.00, "final", 0),
    ]

    # Render all unique SVGs in parallel
    unique = {}
    for fid, _, svg in seg_a_spec + seg_b_spec:
        unique[fid] = svg
    png_map = {}
    workers = min(os.cpu_count() or 4, 8)
    with ThreadPoolExecutor(max_workers=workers) as pool:
        for fid, png_path in pool.map(render_svg_to_png, unique.items()):
            png_map[fid] = png_path

    list_a = OUT / "list_a.txt"
    list_b = OUT / "list_b.txt"
    write_concat(list_a, [(png_map[fid], dur) for fid, dur, _ in seg_a_spec])
    write_concat(list_b, [(png_map[fid], dur) for fid, dur, _ in seg_b_spec])

    # Synthesize audio track
    synthesize_audio(total, cues, WAV)

    # Encode main MP4: concat A + B -> right wipe -> animated bottom progress bar + AAC
    run([
        "ffmpeg", "-y",
        "-f", "concat", "-safe", "0", "-i", str(list_a),
        "-f", "concat", "-safe", "0", "-i", str(list_b),
        "-f", "lavfi", "-i", f"color=c={BAR_COLOR}:s=1920x4:r=30:d={total + 1.0:.3f}",
        "-i", str(WAV),
        "-filter_complex",
        (
            f"[0:v]fps=30,format=yuv420p,settb=1/30,setpts=PTS-STARTPTS[va];"
            f"[1:v]fps=30,format=yuv420p,settb=1/30,setpts=PTS-STARTPTS[vb];"
            f"[va][vb]xfade=transition=wiperight:duration={WIPE_DUR:.2f}:offset={wipe_offset:.3f}[main];"
            f"[2:v]format=yuv420p[bar];"
            f"[main][bar]overlay=x='-w+w*t/{total:.3f}':y=1076:eof_action=repeat[out]"
        ),
        "-map", "[out]", "-map", "3:a",
        "-t", f"{total:.3f}",
        "-c:v", "libx264", "-profile:v", "high", "-level", "4.1",
        "-preset", "slow", "-crf", "18", "-g", "60",
        "-pix_fmt", "yuv420p",
        "-c:a", "aac", "-b:a", "128k", "-ar", str(SR),
        "-movflags", "+faststart",
        str(MP4),
    ])

    # Build 8.6 s GitHub README looping GIF (terminal 5.0 s -> attached 2.0 s -> hook 1.6 s)
    # Reuse the already-rendered terminal PNGs at tighter speed (exclude the WIPE_DUR tail hold)
    term_core = term_frames[:-4]
    raw_term = sum(d for _, d, _ in term_core)
    scale_t = 5.0 / raw_term
    gif_entries = [(png_map[fid], max(0.04, d * scale_t)) for fid, d, _ in term_core]
    gif_entries += [
        (png_map["03-attached-1"], 0.35),
        (png_map["03-attached-2"], 1.65),
        (png_map["01-hook"], 1.60),
    ]
    list_gif = OUT / "list_gif.txt"
    write_concat(list_gif, gif_entries)
    palette = OUT / "palette.png"

    run([
        "ffmpeg", "-y", "-f", "concat", "-safe", "0", "-i", str(list_gif),
        "-vf", "fps=12,scale=800:-1:flags=lanczos,palettegen=max_colors=128:stats_mode=diff",
        str(palette),
    ])
    run([
        "ffmpeg", "-y", "-f", "concat", "-safe", "0", "-i", str(list_gif),
        "-i", str(palette),
        "-lavfi",
        "fps=12,scale=800:-1:flags=lanczos[x];"
        "[x][1:v]paletteuse=dither=bayer:bayer_scale=3:diff_mode=rectangle",
        "-loop", "0",
        str(GIF),
    ])

    info = run([
        "ffprobe", "-v", "error", "-show_entries", "format=duration,size",
        "-of", "default=nw=1", str(MP4),
    ])
    print(f"MP4: {MP4}")
    print(info.strip())
    print(f"GIF: {GIF} ({GIF.stat().st_size / 1024:.1f} KB)")


if __name__ == "__main__":
    main()