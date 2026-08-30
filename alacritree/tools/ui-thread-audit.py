"""Report which functions reachable from `AlacritreeApp::update` can block.

Blocking work on the UI thread is what makes a dialog take seconds to appear
under load: the handler gathers its content before it sets the state that draws
the prompt, so the prompt waits on a subprocess or a repository walk.

    python3 alacritree/tools/ui-thread-audit.py .        # from the repo root
    python3 alacritree/tools/ui-thread-audit.py . NAME   # status of named fns

Requires `ast-grep` on PATH.  Each finding prints the primitive, its line, and
the call chain from `update` that reaches it.  The exit status is 1 while any
finding stands, so CI can gate on it.

## How it decides

ast-grep supplies the structure: function extents, `impl` blocks, background-job
extents (`thread::spawn` and `jobs::pool().spawn`), test modules.  Call
resolution is textual but scoped, and the scoping is what separates a readable
answer from noise:

  * `Type::name` resolves against the functions inside `impl Type`, so two
    `new()` in one file stay distinct,
  * `module::name` against that module's file, following `self as alias` uses,
  * a bare or `self.` name against the callers own impl block first, then the
    free functions of its file.

Resolving bare names crate-wide instead turns `get`, `new` and `push` into
edges, and every function reaches every other one.

Lines inside a background-job closure do not run on the calling thread, so they
are excluded from both the primitive scan and the call graph.  Work that is
already correctly backgrounded therefore does not appear.

## What it cannot see

Startup is out of scope: the root is `update`, so `AlacritreeApp::new` and
everything only it reaches are excluded by construction.  Calls through trait
objects, function pointers, and closures stored in fields produce no edge.
`PRIMS` covers process spawn and wait, git2 status and diff walks, channel
receives, and directory walks; plain file reads, lock contention and pure CPU
work are not counted.  A reachable site is not proof that it blocks in
practice, only that nothing structural stops it.
"""
import collections
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(sys.argv[1])
SRC = "alacritree/src"

# Work that can hold the caller for an unbounded time.  Deliberately narrow: a
# config file read blocks too, but it is not what stalls a dialog for seconds
# under load.
PRIMS = [
    (re.compile(r"Command::new\("), "spawns a process"),
    (re.compile(r"\.output\(\)|wait_with_output\("), "waits on a process"),
    (re.compile(r"\.statuses\("), "walks the repository status"),
    (re.compile(r"diff_tree_to_workdir|diff_tree_to_index|diff_index_to_workdir"), "diffs the working tree"),
    (re.compile(r"Repository::open|Repository::discover"), "opens a repository"),
    (re.compile(r"\.revwalk\("), "walks history"),
    (re.compile(r"\.recv\(\)|\.recv_timeout\("), "blocks on a channel"),
    (re.compile(r"fs::read_dir\(|WalkDir::"), "walks a directory"),
]

QUALIFIED = re.compile(r"\b([A-Za-z_]\w*)::([a-z_]\w*)\s*\(")
SELFCALL = re.compile(r"\bself\.([a-z_]\w*)\s*\(")
BARE = re.compile(r"(?<![.:\w])([a-z_]\w*)\s*\(")
ALIAS = re.compile(r"use\s+crate::([a-z_]\w*)::\{\s*self\s+as\s+([a-z_]\w*)")
FNNAME = re.compile(r"\bfn\s+([A-Za-z_]\w*)")
# The implementing type is what follows `for` in a trait impl and what
# follows `impl` otherwise.  Taking every capitalised token instead pulls in
# trait names and generic parameters, and `T::new()` then links everywhere.
IMPL_FOR = re.compile(r"\bfor\s+(?:&\s*)?([A-Z]\w*)")
IMPL_TY = re.compile(r"\bimpl\b(?:\s*<[^>]*>)?\s+(?:&\s*)?([A-Z]\w*)")


def sg(args):
    out = subprocess.run(args, cwd=ROOT, capture_output=True, text=True,
                         encoding="utf-8", errors="replace")
    if out.returncode != 0 and not out.stdout:
        sys.exit("ast-grep failed: " + (out.stderr or "")[:400])
    return json.loads(out.stdout or "[]")


def by_kind(kind):
    rule = "id: k\nlanguage: rust\nrule:\n  kind: " + kind
    return sg(["ast-grep", "scan", "--inline-rules", rule, SRC, "--json=compact"])


def by_pattern(pat):
    return sg(["ast-grep", "-p", pat, "-l", "rust", SRC, "--json=compact"])


def extent(m):
    return (m["file"].replace("\\", "/"),
            m["range"]["start"]["line"], m["range"]["end"]["line"])


spawn_regions = collections.defaultdict(list)
for pat in ("thread::spawn($$$)", "std::thread::spawn($$$)", "jobs::pool().spawn($$$)"):
    for m in by_pattern(pat):
        f, s, e = extent(m)
        spawn_regions[f].append((s, e))

test_regions = collections.defaultdict(list)
for m in by_kind("mod_item"):
    head = m["text"][:80]
    if re.search(r"\bmod\s+tests?\b", head) or "#[cfg(test)]" in head:
        f, s, e = extent(m)
        test_regions[f].append((s, e))

impl_regions = collections.defaultdict(list)
for m in by_kind("impl_item"):
    f, s_, e_ = extent(m)
    head = m["text"][:200].split("{")[0]
    hit = IMPL_FOR.search(head) or IMPL_TY.search(head)
    if hit:
        impl_regions[f].append((s_, e_, hit.group(1)))

aliases = collections.defaultdict(dict)
for path in (ROOT / SRC).rglob("*.rs"):
    rel = str(path.relative_to(ROOT)).replace("\\", "/")
    text = path.read_text(encoding="utf-8", errors="replace")
    for target, alias in ALIAS.findall(text):
        aliases[rel][alias] = target


def inside(regions, f, line):
    return any(s <= line <= e for s, e in regions.get(f, []))


def impl_type_at(f, line):
    best = None
    for s_, e_, ty in impl_regions.get(f, []):
        if s_ <= line <= e_ and (best is None or s_ > best[0]):
            best = (s_, ty)
    return best[1] if best else None


class Fn:
    __slots__ = ("file", "start", "name", "key", "ty", "is_test", "raw", "calls", "prims")


fns = []
for m in by_kind("function_item"):
    f, s, _ = extent(m)
    nm = FNNAME.search(m["text"])
    if not nm:
        continue
    fn = Fn()
    fn.file, fn.start, fn.name = f, s, nm.group(1)
    fn.key = (f, nm.group(1), s)
    fn.ty = impl_type_at(f, s)
    fn.is_test = inside(test_regions, f, s) or "#[test]" in m["text"][:200]
    fn.raw, fn.prims, fn.calls = [], [], set()
    for i, line in enumerate(m["text"].split("\n")):
        ln = s + i
        stripped = line.lstrip()
        if stripped.startswith("//") or inside(spawn_regions, f, ln):
            continue
        for rx, why in PRIMS:
            if rx.search(line):
                fn.prims.append((ln, why, stripped[:110]))
                break
        fn.raw += [("mod", mod, name) for mod, name in QUALIFIED.findall(line)]
        fn.raw += [("local", None, name) for name in SELFCALL.findall(line)]
        fn.raw += [("local", None, name) for name in BARE.findall(line)]
    fns.append(fn)

live = [f for f in fns if not f.is_test]
by_key = {f.key: f for f in live}
by_name = collections.defaultdict(list)
in_file = collections.defaultdict(lambda: collections.defaultdict(list))
by_stem = collections.defaultdict(set)
in_type = collections.defaultdict(list)
for f in live:
    by_name[f.name].append(f)
    in_file[f.file][f.name].append(f)
    by_stem[Path(f.file).stem].add(f.file)
    if f.ty:
        in_type[(f.ty, f.name)].append(f)


def resolve(fn, call):
    kind, qualifier, name = call
    if kind == "local":
        # The callers own impl block wins over the rest of the file, so a
        # sibling method named `new` does not shadow the types own.
        own = [g for g in in_type.get((fn.ty, name), []) if g.file == fn.file]
        return own or [g for g in in_file[fn.file].get(name, []) if g.ty is None]
    stem = aliases[fn.file].get(qualifier, qualifier)
    hits = [g for path in by_stem.get(stem, ())
            for g in in_file[path].get(name, []) if g.ty is None]
    if hits or not qualifier[:1].isupper():
        return hits
    return in_type.get((qualifier, name), [])


for f in live:
    f.calls = {g.key for c in f.raw for g in resolve(f, c)} - {f.key}

blocking = {f.key: f.prims[0][1] for f in live if f.prims}
callers_of = collections.defaultdict(set)
for f in live:
    for c in f.calls:
        callers_of[c].add(f.key)

queue = list(blocking)
while queue:
    cur = queue.pop()
    for caller in callers_of.get(cur, ()):
        if caller not in blocking:
            blocking[caller] = "calls " + by_key[cur].name + "()"
            queue.append(caller)

roots = [f.key for f in live if f.name == "update"
         and f.file.endswith("app.rs") and f.ty == "AlacritreeApp"]
parent = {k: None for k in roots}
queue = collections.deque(roots)
while queue:
    cur = queue.popleft()
    for c in by_key[cur].calls:
        if c not in parent:
            parent[c] = cur
            queue.append(c)
reach = set(parent)


def chain(k):
    out = []
    while k is not None:
        out.append(by_key[k].name + "()")
        k = parent[k]
    return " -> ".join(reversed(out))


leaves = sorted((k for k in reach if by_key[k].prims), key=lambda k: (k[0], k[2]))
print("functions %d live / %d total | blocking %d | reachable from update %d"
      % (len(live), len(fns), len(blocking), len(reach)))
print("blocking leaves reachable from update: %d\n" % len(leaves))
for k in leaves:
    f = by_key[k]
    print("%s:%d  %s()" % (f.file, f.start, f.name))
    for ln, why, src in f.prims[:3]:
        print("    L%d  %s  |  %s" % (ln, why, src))
    print("    " + chain(k) + "\n")

if len(sys.argv) > 2:
    want = set(sys.argv[2:])
    print("--- named ---")
    for f in sorted(live, key=lambda f: f.file):
        if f.name in want:
            print("%s:%d %s()  blocking=%r  reachable=%s"
                  % (f.file, f.start, f.name, blocking.get(f.key), f.key in reach))

if "--trace-new" in sys.argv:
    newkeys = {f.key for f in live if f.name == "new" and f.file.endswith("app.rs")}
    for f in live:
        if f.key in reach and newkeys & f.calls:
            hits = [c for c in f.raw if any(g.key in newkeys for g in resolve(f, c))]
            print("%s:%d %s() -> app.rs new() via %r" % (f.file, f.start, f.name, hits[:4]))

# Non-zero on a finding so CI fails on it.  The listing above is printed first,
# so the failing run names the call paths rather than only a count.
sys.exit(1 if leaves else 0)
