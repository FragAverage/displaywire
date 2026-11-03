#!/usr/bin/env python3
"""Hands-off performance comparison between versions, on the real PC and Mac.

For each revision: builds its server on the PC (over SSH) and installs it over the service,
builds its Mac app, shows the same moving picture full screen on the PC (scripts/perf/motion.html
in Edge), connects for a while with per-frame timing on (DISPLAYWIRE_TRACE), and reads the
server's own stats. Then prints a comparison and writes it to target/perf/report-*.md. Afterwards the PC gets `--restore`'s
server (main by default) back.

    scripts/perf.py                        # main, v0.6.3, main again (the repeat shows the noise)
    scripts/perf.py --revs v0.6.3 main --seconds 60

Needs: an SSH host for the PC (`--pc`, default frag-pc) with a clone at ~/dw-build and Rust
installed; the PC paired with this Mac in the app; the PC's address as the app knows it (`--addr`).
"""

import argparse
import csv
import datetime
import os
import re
import shutil
import statistics
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "target", "perf")
STATE = os.path.expanduser("~/Library/Application Support/DisplayWire")
EDGE = r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe"


def run(cmd, **kw):
    return subprocess.run(cmd, check=True, text=True, capture_output=True, **kw).stdout


def ps(pc, script):
    """Runs PowerShell on the PC; returns its output."""
    # The server log isn't UTF-8 (the console's code page mangles its arrows).
    r = subprocess.run(["ssh", pc, "powershell", "-NoProfile", "-Command", "-"], input=script, text=True, errors="replace", capture_output=True)
    if r.returncode != 0:
        raise RuntimeError(f"PC: {r.stderr.strip() or r.stdout.strip()}")
    return r.stdout


def log(msg):
    print(f"[{datetime.datetime.now():%H:%M:%S}] {msg}", flush=True)


def build_server(pc, rev, toolchain):
    sha = run(["git", "-C", ROOT, "rev-parse", f"{rev}^{{commit}}"]).strip()
    bundle = os.path.join(OUT, "all.bundle")
    run(["git", "-C", ROOT, "bundle", "create", bundle, "--all"])
    run(["scp", "-q", bundle, f"{pc}:dw-perf.bundle"])
    log(f"server {rev}: building on the PC")
    out = ps(pc, f"""
        $ErrorActionPreference = 'Continue'
        cd $HOME\\dw-build
        git fetch -q ..\\dw-perf.bundle 'refs/heads/*:refs/remotes/perf/*' 'refs/tags/*:refs/tags/*' 2>&1 | Out-Null
        git checkout -q -f {sha} 2>&1 | Out-Null
        cargo +{toolchain} build --release -p displaywire-server 2>&1 | Select-String -Pattern '^error' -Context 0,6
        $d = 'C:\\Program Files\\DisplayWire'
        Stop-Service DisplayWireServer
        Start-Sleep 2
        Get-Process displaywire-server -ErrorAction SilentlyContinue | Stop-Process -Force
        Start-Sleep 1
        Copy-Item target\\release\\displaywire-server.exe "$d\\displaywire-server.exe" -Force
        Start-Service DisplayWireServer
        Start-Sleep 4
        'service ' + (Get-Service DisplayWireServer).Status
    """)
    if "error" in out.lower() or "service Running" not in out:
        raise RuntimeError(f"server {rev} didn't build or start:\n{out}")


def build_client(rev):
    """The Mac app of `rev`, built in a worktree of its own (current main uses the main checkout)."""
    app_dir = os.path.join(OUT, "apps", rev.replace("/", "_"))
    tree = os.path.join(OUT, "trees", rev.replace("/", "_"))
    if os.path.exists(tree):
        run(["git", "-C", ROOT, "worktree", "remove", "--force", tree])
    run(["git", "-C", ROOT, "worktree", "add", "--detach", tree, rev])
    log(f"client {rev}: building")
    env = dict(os.environ, CARGO_TARGET_DIR=os.path.join(OUT, "cargo-target"))
    run(["cargo", "build", "--release", "-p", "displaywire-client"], cwd=tree, env=env)
    shutil.rmtree(app_dir, ignore_errors=True)
    run(["sh", os.path.join(tree, "scripts", "bundle-mac.sh"), os.path.join(env["CARGO_TARGET_DIR"], "release", "displaywire-client"), app_dir])
    run(["git", "-C", ROOT, "worktree", "remove", "--force", tree])
    return os.path.join(app_dir, "DisplayWire.app")


def motion(pc, on):
    """The moving picture, full screen on the PC's desktop (through a task, as SSH has no desktop)."""
    stop = """
        Get-CimInstance Win32_Process -Filter "Name='msedge.exe'" | ? { $_.CommandLine -like '*dw-bench*' } | % { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
        schtasks /delete /f /tn DisplayWireBench 2>&1 | Out-Null
    """
    if not on:
        ps(pc, stop)
        return
    run(["ssh", pc, "powershell", "-NoProfile", "-Command", "New-Item -ItemType Directory -Force $HOME\\dw-bench | Out-Null"])
    run(["scp", "-q", os.path.join(ROOT, "scripts", "perf", "motion.html"), f"{pc}:dw-bench/motion.html"])
    ps(pc, stop + f"""
        $page = "file:///" + ($HOME -replace '\\\\','/') + "/dw-bench/motion.html"
        $profile = "$HOME\\dw-bench\\edge"
        $action = New-ScheduledTaskAction -Execute '{EDGE}' -Argument "--kiosk $page --edge-kiosk-type=fullscreen --no-first-run --user-data-dir=$profile"
        $principal = New-ScheduledTaskPrincipal -UserId $env:USERNAME -LogonType Interactive
        Register-ScheduledTask -TaskName DisplayWireBench -Action $action -Principal $principal -Force | Out-Null
        Start-ScheduledTask -TaskName DisplayWireBench
    """)


def server_log_lines(pc):
    return int(ps(pc, "(Get-Content C:\\ProgramData\\DisplayWire\\server.log | Measure-Object -Line).Lines").strip() or 0)


def server_stats(pc, since_line):
    text = ps(pc, f"Get-Content C:\\ProgramData\\DisplayWire\\server.log | Select -Skip {since_line}")
    rows = []
    for line in text.splitlines():
        m = re.search(r"^\s*(\d+) fps\s+([\d.]+) Mbps.*?encoded ([\d.]+) ms\s+send ([\d.]+) ms\s+keyframes (\d+)", line)
        if m:
            rows.append(tuple(float(x) for x in m.groups()))
    return rows


def measure(app, addr, seconds, warmup, name):
    # `open --stdout` only takes some places (not an external volume): write in /tmp, keep in target/perf.
    tmp_trace, tmp_stdout = f"/tmp/dw-perf-trace-{name}.csv", f"/tmp/dw-perf-client-{name}.log"
    trace = os.path.join(OUT, f"trace-{name}.csv")
    stdout = os.path.join(OUT, f"client-{name}.log")
    for f in (trace, stdout, tmp_trace, tmp_stdout):
        if os.path.exists(f):
            os.remove(f)
    exe = os.path.join(app, "Contents", "MacOS")
    subprocess.run(["pkill", "-f", exe])
    # Through `open`, so the app runs as itself (with its Local Network permission).
    run(["open", "-n", "--env", f"DISPLAYWIRE_TRACE={tmp_trace}", "--stdout", tmp_stdout, "--stderr", tmp_stdout, app, "--args", addr])
    time.sleep(warmup + seconds)
    subprocess.run(["pkill", "-f", exe])
    time.sleep(1)
    for src, dst in ((tmp_trace, trace), (tmp_stdout, stdout)):
        if os.path.exists(src):
            shutil.move(src, dst)
    return trace, stdout


def pct(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(p / 100 * len(xs)))] if xs else float("nan")


def summarise(trace, stdout, server_rows, warmup):
    with open(trace) as f:
        # The last line can be cut short: the app is stopped mid-write.
        rows = [r for r in csv.DictReader(f) if all((r.get(k) or "").isdigit() for k in ("bytes", "capture_ns", "decoded_ns", "decode_ns"))]
    if not rows:
        raise RuntimeError(f"no frames decoded (see {stdout})")
    decoded = [int(r["decoded_ns"]) for r in rows]
    start = min(decoded) + warmup * 1e9
    rows = [r for r in rows if int(r["decoded_ns"]) >= start]
    span = (max(int(r["decoded_ns"]) for r in rows) - min(int(r["decoded_ns"]) for r in rows)) / 1e9
    latency = [(int(r["decoded_ns"]) - int(r["capture_ns"])) / 1e6 for r in rows]
    decode = [int(r["decode_ns"]) / 1e6 for r in rows]
    lost = dropped = errors = 0
    for line in open(stdout, errors="replace"):
        m = re.search(r"lost (\d+) dropped (\d+) errors (\d+)", line)
        if m:
            lost, dropped, errors = lost + int(m[1]), dropped + int(m[2]), errors + int(m[3])
    # The server's per-second lines, skipping the warm-up seconds.
    srv = server_rows[int(warmup):] or server_rows
    return {
        "fps (Mac)": len(rows) / span if span else float("nan"),
        "Mbps (Mac)": sum(int(r["bytes"]) for r in rows) * 8 / span / 1e6 if span else float("nan"),
        "capture→decoded p50 ms": statistics.median(latency),
        "capture→decoded p95 ms": pct(latency, 95),
        "capture→decoded p99 ms": pct(latency, 99),
        "decode p50 ms": statistics.median(decode),
        "decode p99 ms": pct(decode, 99),
        "capture→encoded ms (PC)": statistics.mean(r[2] for r in srv) if srv else float("nan"),
        "send ms (PC)": statistics.mean(r[3] for r in srv) if srv else float("nan"),
        "fps (PC)": statistics.mean(r[0] for r in srv) if srv else float("nan"),
        "keyframes": sum(r[4] for r in srv),
        "frames lost / dropped / errors": f"{lost} / {dropped} / {errors}",
    }


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--revs", nargs="+", default=["main", "v0.6.3", "main"])
    ap.add_argument("--pc", default="frag-pc", help="SSH host of the PC")
    ap.add_argument("--addr", default="192.168.1.171", help="the PC's address as the Mac app has it saved")
    ap.add_argument("--seconds", type=int, default=30, help="measured time per run")
    ap.add_argument("--warmup", type=int, default=8, help="seconds dropped at the start of each run")
    ap.add_argument("--restore", default="main", help="server to leave installed afterwards ('none' to keep the last one tested)")
    a = ap.parse_args()
    os.makedirs(OUT, exist_ok=True)
    toolchain = run(["rustc", "--version"]).split()[1]

    # The app's remembered monitor would pull the stream off the one the picture is on.
    remembered = os.path.join(STATE, "monitors")
    saved = remembered + ".perf-backup"
    if os.path.exists(remembered):
        shutil.move(remembered, saved)
    results, apps = [], {}
    try:
        motion(a.pc, True)
        for i, rev in enumerate(a.revs):
            name = f"{i + 1}-{rev.replace('/', '_')}"
            build_server(a.pc, rev, toolchain)
            if rev not in apps:
                apps[rev] = build_client(rev)
            since = server_log_lines(a.pc)
            log(f"run {name}: {a.warmup} s warm-up + {a.seconds} s")
            trace, stdout = measure(apps[rev], a.addr, a.seconds, a.warmup, name)
            results.append((name, summarise(trace, stdout, server_stats(a.pc, since), a.warmup)))
            log(f"run {name}: done")
    finally:
        motion(a.pc, False)
        if a.restore != "none" and a.revs[-1] != a.restore:
            log(f"putting the {a.restore} server back")
            build_server(a.pc, a.restore, toolchain)
        if os.path.exists(saved):
            shutil.move(saved, remembered)

    keys = list(results[0][1])
    table = ["| | " + " | ".join(n for n, _ in results) + " |", "|---|" + "---|" * len(results)]
    for k in keys:
        cells = [f"{r[k]:.2f}" if isinstance(r[k], float) else str(r[k]) for _, r in results]
        table.append(f"| {k} | " + " | ".join(cells) + " |")
    report = "\n".join([f"# Performance: {' vs '.join(a.revs)}", "", f"{datetime.datetime.now():%Y-%m-%d %H:%M}, {a.seconds} s per run after {a.warmup} s warm-up.", ""] + table) + "\n"
    path = os.path.join(OUT, f"report-{datetime.datetime.now():%Y%m%d-%H%M}.md")
    with open(path, "w") as f:
        f.write(report)
    print("\n" + report + f"\n(written to {path})")


if __name__ == "__main__":
    sys.exit(main())
