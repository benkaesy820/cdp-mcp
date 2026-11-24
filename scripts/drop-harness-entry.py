#!/usr/bin/env python3
"""Remove mcp.servers.<name> from an OpenCode-style JSON config.

Usage: drop-harness-entry.py <config.json> <name>

Backs the file up first, is silent when there is nothing to remove, and exits 0
in that case so a caller under `set -e` is not tripped by "already gone".
"""
import json
import shutil
import sys
import time

if len(sys.argv) != 3:
    sys.exit("usage: drop-harness-entry.py <config.json> <name>")

path, name = sys.argv[1], sys.argv[2]
try:
    with open(path) as f:
        cfg = json.load(f)
except FileNotFoundError:
    sys.exit(0)
except Exception as e:
    sys.exit(f"cannot parse {path}: {e}")

servers = (cfg.get("mcp") or {}).get("servers") or {}
if name not in servers:
    sys.exit(0)

shutil.copy2(path, f"{path}.bak.{time.strftime('%Y%m%d%H%M%S')}")
del servers[name]
with open(path, "w") as f:
    json.dump(cfg, f, indent=2)
    f.write("\n")
print(f"  removed mcp.servers.{name} from {path}")
