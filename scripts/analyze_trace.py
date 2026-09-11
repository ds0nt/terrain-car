#!/usr/bin/env python3
"""Summarizes a bevy `trace_chrome` JSON into a small text report: total time
per system (top offenders) plus a per-second frame-time breakdown. Reads the
file once, line-by-line (it's one JSON object per line), so it never needs to
hold the whole multi-GB file in memory at once.

Usage: analyze_trace.py <trace-*.json> [out.txt]
"""
import json
import sys
from collections import defaultdict

def main():
    if len(sys.argv) < 2:
        print("usage: analyze_trace.py <trace-*.json> [out.txt]", file=sys.stderr)
        sys.exit(1)

    path = sys.argv[1]
    out_path = sys.argv[2] if len(sys.argv) > 2 else "trace-summary.txt"

    stacks = defaultdict(list)
    total_dur = defaultdict(float)
    count = defaultdict(int)
    max_dur = defaultdict(float)

    update_stack = {}
    frames = []  # (start_ts, dur) for the top-level "update: " span

    n = 0
    with open(path, "r") as f:
        for line in f:
            line = line.strip()
            if line in ("[", "]") or not line:
                continue
            if line.endswith(","):
                line = line[:-1]
            try:
                ev = json.loads(line)
            except json.JSONDecodeError:
                continue
            n += 1
            ph = ev.get("ph")
            if ph not in ("B", "E"):
                continue
            key = (ev.get("pid"), ev.get("tid"))
            name = ev.get("name")
            ts = ev.get("ts")

            if name == "update: ":
                if ph == "B":
                    update_stack[key] = ts
                else:
                    bts = update_stack.pop(key, None)
                    if bts is not None:
                        frames.append((bts, ts - bts))

            if ph == "B":
                stacks[key].append((name, ts))
            else:
                st = stacks[key]
                if not st:
                    continue
                bname, bts = st.pop()
                dur = ts - bts
                total_dur[bname] += dur
                count[bname] += 1
                if dur > max_dur[bname]:
                    max_dur[bname] = dur

    frames.sort()
    buckets = defaultdict(list)
    for start, dur in frames:
        buckets[int(start // 1_000_000)].append(dur)

    with open(out_path, "w") as out:
        out.write(f"parsed {n} events, {len(frames)} frames\n\n")

        out.write("=== per-second frame times ===\n")
        out.write(f"{'sec':>4} {'frames':>7} {'avg_ms':>8} {'min_ms':>8} {'max_ms':>8} {'avg_fps':>8}\n")
        for sec in sorted(buckets):
            durs = buckets[sec]
            avg = sum(durs) / len(durs) / 1000
            out.write(
                f"{sec:4d} {len(durs):7d} {avg:8.2f} {min(durs)/1000:8.2f} "
                f"{max(durs)/1000:8.2f} {1000/avg if avg else 0:8.1f}\n"
            )

        out.write("\n=== 20 slowest individual frames ===\n")
        for start, dur in sorted(frames, key=lambda x: -x[1])[:20]:
            out.write(f"  t={start/1_000_000:8.3f}s  dur={dur/1000:8.2f}ms\n")

        out.write("\n=== top 60 systems by total time ===\n")
        out.write(f"{'total_ms':>12} {'count':>10} {'avg_us':>10} {'max_us':>10}  name\n")
        items = sorted(total_dur.items(), key=lambda kv: -kv[1])
        for name, tot in items[:60]:
            c = count[name]
            avg = tot / c if c else 0
            out.write(f"{tot/1000:12.1f} {c:10d} {avg:10.1f} {max_dur[name]:10.1f}  {name}\n")

    print(f"wrote {out_path}", file=sys.stderr)


if __name__ == "__main__":
    main()
