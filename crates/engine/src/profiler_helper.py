"""CodeCraft profiler helper (run by the project's Python interpreter; standard library only).

Subcommands:

  pstats IN OUT
      Convert a cProfile statistics file (`python -m cProfile -o IN ...`) to JSON:
      {"codecraft": "cprofile", "totalTime": s, "functions": [{name, file, line, calls,
      primcalls, tt, ct, callers: [[index, calls, primcalls, tt, ct], ...]}]}

  tracemalloc OUT [--frames N] [--interval S] -- (SCRIPT | -m MODULE) ARGS...
      Run a program with tracemalloc on and, when it ends (normally, by an exception or
      sys.exit), write the live allocations grouped by traceback (root first) to OUT:
      {"codecraft": "tracemalloc", "current": bytes, "peak": bytes, "when": "...",
       "traces": [{size, count, frames: [{name, file, line}]}]}
      With --interval S (> 0), a background thread also records the traced memory every S
      seconds and takes a snapshot whenever the live memory reaches a new high (10% above the
      last one), so the snapshot nearest the peak is kept:
       "interval": S, "timeline": [[ms, current, peak since the previous point], ...],
       "peakSnapshot": {"time": ms, "current": bytes, "file": "<OUT>.peak.json",
                        "top": [{name, file, line, size, count, callers: [...]}]}
      and that snapshot is written whole (same format) to the "file" next to OUT. The memory
      the held snapshot itself takes is subtracted from the numbers.
"""

import array
import ast
import json
import os
import sys
import threading
import time

HELPER = os.path.abspath(__file__)


def norm(path):
    """An absolute path for a real source file (so the IDE can open it); others as they are."""
    if path and not path.startswith("<") and path != "~" and os.path.exists(path):
        return os.path.abspath(path)
    return path


def pstats_to_json(src, dst):
    import pstats

    st = pstats.Stats(src)
    keys = list(st.stats.keys())
    index = {k: i for i, k in enumerate(keys)}
    functions = []
    for k in keys:
        cc, nc, tt, ct, callers = st.stats[k]
        edges = []
        for caller, v in callers.items():
            if caller not in index:
                continue
            # (cc, nc, tt, ct) per edge for cProfile data; older profile data has a call count.
            if isinstance(v, tuple) and len(v) >= 4:
                edges.append([index[caller], v[1], v[0], v[2], v[3]])
            else:
                edges.append([index[caller], int(v), int(v), 0.0, 0.0])
        functions.append(
            {
                "file": norm(k[0]),
                "line": k[1],
                "name": k[2],
                "calls": nc,
                "primcalls": cc,
                "tt": tt,
                "ct": ct,
                "callers": edges,
            }
        )
    doc = {"codecraft": "cprofile", "totalTime": st.total_tt, "functions": functions}
    with open(dst, "w", encoding="utf-8") as f:
        json.dump(doc, f)


class Names:
    """The function (qualified name) around a line of a source file, from its AST."""

    def __init__(self):
        self.cache = {}

    def spans(self, path):
        if path in self.cache:
            return self.cache[path]
        spans = []
        try:
            with open(path, "rb") as f:
                tree = ast.parse(f.read(), path)

            def walk(node, prefix):
                for child in ast.iter_child_nodes(node):
                    if isinstance(child, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
                        name = prefix + child.name
                        end = getattr(child, "end_lineno", None) or child.lineno
                        start = min([child.lineno] + [d.lineno for d in child.decorator_list])
                        spans.append((start, end, name))
                        walk(child, name + ".")
                    else:
                        walk(child, prefix)

            walk(tree, "")
        except Exception:
            spans = []
        self.cache[path] = spans
        return spans

    def name(self, path, line):
        best = None
        for start, end, name in self.spans(path):
            if start <= line <= end and (best is None or start >= best[0]):
                best = (start, name)
        return best[1] if best else "<module>"


def internal(filename):
    base = os.path.basename(filename)
    return (
        os.path.abspath(filename) == HELPER
        or filename in ("<frozen runpy>",)
        or base == "runpy.py"
        or filename.startswith("<frozen importlib")
    )


MAX_POINTS = 5000


class Sampler:
    """Traced memory every `interval` seconds, and the snapshot nearest the peak."""

    def __init__(self, interval):
        self.interval = interval
        self.start = time.monotonic()
        # Flat [ms, current, peak, ms, current, peak, ...] (small: it is traced too).
        self.points = array.array("d")
        self.best = None  # (snapshot, ms, current)
        self.best_current = 0
        # Memory the held snapshot takes (subtracted from readings).
        self.overhead = 0
        self.peak = 0
        self.lock = threading.Lock()
        self.done = threading.Event()
        self.thread = threading.Thread(target=self.run, name="codecraft-tracemalloc", daemon=True)

    def begin(self):
        self.thread.start()

    def run(self):
        while not self.done.wait(self.interval):
            try:
                self.tick()
            except Exception:
                return

    def tick(self):
        import tracemalloc

        with self.lock:
            if not tracemalloc.is_tracing():
                return
            current, peak = tracemalloc.get_traced_memory()
            if hasattr(tracemalloc, "reset_peak"):
                tracemalloc.reset_peak()
            current = max(0, current - self.overhead)
            peak = max(current, peak - self.overhead)
            self.peak = max(self.peak, peak)
            ms = (time.monotonic() - self.start) * 1000.0
            self.points.extend((round(ms, 1), current, peak))
            if len(self.points) >= 3 * MAX_POINTS:
                self.halve()
            if current > self.best_current * 1.1 + 65536:
                # Release the previous snapshot first, then measure what the new one takes.
                self.best = None
                before = tracemalloc.get_traced_memory()[0]
                snapshot = tracemalloc.take_snapshot()
                after = tracemalloc.get_traced_memory()[0]
                self.overhead = max(0, after - before)
                self.best = (snapshot, ms, current)
                self.best_current = current
                if hasattr(tracemalloc, "reset_peak"):
                    tracemalloc.reset_peak()

    def halve(self):
        """Merge pairs of points (keeping the higher peak) when there are too many."""
        p = self.points
        merged = array.array("d")
        n = len(p) // 3
        for i in range(0, 3 * (n - n % 2), 6):
            merged.extend((p[i + 3], p[i + 4], max(p[i + 2], p[i + 5])))
        if n % 2:
            merged.extend(p[3 * (n - 1):3 * n])
        self.points = merged
        self.interval *= 2

    def finish(self):
        self.done.set()
        if self.thread.is_alive() and self.thread is not threading.current_thread():
            self.thread.join(timeout=max(1.0, self.interval * 2))
        try:
            self.tick()
        except Exception:
            pass

    def timeline(self):
        p = self.points
        return [[p[i], int(p[i + 1]), int(p[i + 2])] for i in range(0, len(p) - 2, 3)]


def trace_list(snapshot, names, limit=20000):
    import tracemalloc

    snapshot = snapshot.filter_traces(
        [tracemalloc.Filter(False, tracemalloc.__file__), tracemalloc.Filter(False, HELPER)]
    )
    traces = []
    for stat in snapshot.statistics("traceback")[:limit]:
        frames = list(stat.traceback)  # oldest (root) first on Python 3.7+
        while frames and internal(frames[0].filename):
            frames.pop(0)
        traces.append(
            {
                "size": stat.size,
                "count": stat.count,
                "frames": [
                    {"name": names.name(f.filename, f.lineno), "file": norm(f.filename), "line": f.lineno}
                    for f in frames
                ],
            }
        )
    return traces


def top_sites(traces, limit=200):
    """Allocation sites (the innermost frame) by size, with the callers of the heaviest trace."""
    sites = {}
    for t in traces:
        frames = t["frames"]
        if not frames:
            continue
        leaf = frames[-1]
        key = (leaf["file"], leaf["line"])
        site = sites.get(key)
        if site is None:
            site = {"name": leaf["name"], "file": leaf["file"], "line": leaf["line"], "size": 0, "count": 0,
                    "callers": [], "_best": -1}
            sites[key] = site
        site["size"] += t["size"]
        site["count"] += t["count"]
        if t["size"] > site["_best"]:
            site["_best"] = t["size"]
            site["callers"] = ["%s (%s:%s)" % (f["name"], f["file"], f["line"]) for f in reversed(frames[:-1])][:5]
    out = sorted(sites.values(), key=lambda s: (-s["size"], s["file"] or "", s["line"]))[:limit]
    for s in out:
        del s["_best"]
    return out


def peak_path(out):
    base = out[:-5] if out.endswith(".json") else out
    return base + ".peak.json"


def dump_tracemalloc(out, when, sampler=None):
    import tracemalloc

    if not tracemalloc.is_tracing():
        return
    if sampler is not None:
        sampler.finish()
    overhead = sampler.overhead if sampler is not None else 0
    current, peak = tracemalloc.get_traced_memory()
    current = max(0, current - overhead)
    peak = max(current, peak - overhead, sampler.peak if sampler is not None else 0)
    snapshot = tracemalloc.take_snapshot()
    best = sampler.best if sampler is not None else None
    tracemalloc.stop()
    names = Names()
    doc = {"codecraft": "tracemalloc", "current": current, "peak": peak, "when": when,
           "traces": trace_list(snapshot, names)}
    if sampler is not None:
        doc["interval"] = sampler.interval
        doc["timeline"] = sampler.timeline()
        if best is not None:
            snap, ms, live = best
            peak_traces = trace_list(snap, names)
            info = {"time": round(ms, 1), "current": live, "file": os.path.basename(peak_path(out)),
                    "top": top_sites(peak_traces)}
            doc["peakSnapshot"] = info
            peak_doc = {"codecraft": "tracemalloc", "current": live, "peak": peak,
                        "when": "at the peak (%.1f s after the start)" % (ms / 1000.0),
                        "traces": peak_traces, "interval": sampler.interval, "timeline": doc["timeline"],
                        "peakSnapshot": dict(info, file=None)}
            with open(peak_path(out), "w", encoding="utf-8") as f:
                json.dump(peak_doc, f)
    with open(out, "w", encoding="utf-8") as f:
        json.dump(doc, f)


def run_tracemalloc(argv):
    import runpy
    import tracemalloc

    out = argv[0]
    rest = argv[1:]
    frames = 25
    interval = 0.0
    while rest[:1] in (["--frames"], ["--interval"]) and len(rest) > 1:
        if rest[0] == "--frames":
            frames = max(1, min(100, int(rest[1])))
        else:
            interval = max(0.0, min(3600.0, float(rest[1])))
        rest = rest[2:]
    if rest[:1] == ["--"]:
        rest = rest[1:]
    if not rest:
        sys.exit("tracemalloc: nothing to run")
    module = None
    if rest[0] == "-m" and len(rest) > 1:
        module, args = rest[1], rest[2:]
        sys.argv = [module] + args
        sys.path[0] = os.getcwd()
    else:
        script, args = rest[0], rest[1:]
        sys.argv = [script] + args
        sys.path[0] = os.path.dirname(os.path.abspath(script))
    tracemalloc.start(frames)
    sampler = None
    if interval > 0:
        sampler = Sampler(max(0.005, interval))
        sampler.begin()
    when = "when the program ended"
    try:
        if module:
            runpy.run_module(module, run_name="__main__", alter_sys=True)
        else:
            runpy.run_path(sys.argv[0], run_name="__main__")
    except SystemExit as e:
        when = "at sys.exit(%s)" % (e.code,)
        raise
    except BaseException as e:
        when = "when the program raised %s" % type(e).__name__
        raise
    finally:
        dump_tracemalloc(out, when, sampler)


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    cmd, argv = sys.argv[1], sys.argv[2:]
    if cmd == "pstats" and len(argv) == 2:
        pstats_to_json(argv[0], argv[1])
    elif cmd == "tracemalloc" and argv:
        # Drop the helper's own arguments before running the program.
        del sys.argv[1:]
        run_tracemalloc(argv)
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
