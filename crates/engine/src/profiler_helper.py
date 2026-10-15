"""CodeCraft profiler helper (run by the project's Python interpreter; standard library only).

Subcommands:

  pstats IN OUT
      Convert a cProfile statistics file (`python -m cProfile -o IN ...`) to JSON:
      {"codecraft": "cprofile", "totalTime": s, "functions": [{name, file, line, calls,
      primcalls, tt, ct, callers: [[index, calls, primcalls, tt, ct], ...]}]}

  tracemalloc OUT [--frames N] -- (SCRIPT | -m MODULE) ARGS...
      Run a program with tracemalloc on and, when it ends (normally, by an exception or
      sys.exit), write the live allocations grouped by traceback (root first) to OUT:
      {"codecraft": "tracemalloc", "current": bytes, "peak": bytes, "when": "...",
       "traces": [{size, count, frames: [{name, file, line}]}]}
"""

import ast
import json
import os
import sys

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


def dump_tracemalloc(out, when):
    import tracemalloc

    if not tracemalloc.is_tracing():
        return
    current, peak = tracemalloc.get_traced_memory()
    snapshot = tracemalloc.take_snapshot()
    tracemalloc.stop()
    snapshot = snapshot.filter_traces(
        [tracemalloc.Filter(False, tracemalloc.__file__), tracemalloc.Filter(False, HELPER)]
    )
    names = Names()
    traces = []
    for stat in snapshot.statistics("traceback")[:20000]:
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
    doc = {"codecraft": "tracemalloc", "current": current, "peak": peak, "when": when, "traces": traces}
    with open(out, "w", encoding="utf-8") as f:
        json.dump(doc, f)


def run_tracemalloc(argv):
    import runpy
    import tracemalloc

    out = argv[0]
    rest = argv[1:]
    frames = 25
    if rest[:1] == ["--frames"] and len(rest) > 1:
        frames = max(1, min(100, int(rest[1])))
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
        dump_tracemalloc(out, when)


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
