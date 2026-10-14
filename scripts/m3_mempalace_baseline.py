#!/usr/bin/env python3
"""Record MemPalace's top-10 for every query of the frozen synthetic gold corpus.

Local only. Needs the owner's installed MemPalace (/Users/icue/.venv). Run it as

    /Users/icue/.venv/bin/python -I scripts/m3_mempalace_baseline.py

It builds a THROWAWAY palace from crates/alice-daemon/tests/data/gold_corpus.json in a
temp dir, drives MemPalace through its Python API (not the CLI: the CLI routes writes
through the hub) on the sqlite_exact backend, and writes
crates/alice-daemon/tests/data/mempalace_baseline.json:

    {"mempalace_version": "...", "queries": {"q01": ["drawer_id", ...]}}

Safety, enforced here (exit code 3 when one fails):
  1. HOME, MEMPALACE_PALACE_PATH, MEMPALACE_HUB_FORWARD=0 and MEMPALACE_DAEMON_STATE_ROOT
     are set to the temp dir before MemPalace is imported, so no config or palace of the
     owner is found. Nothing is downloaded: the embedding model must already be on disk
     (the cached all-MiniLM-L6-v2 files are linked into the temp HOME, read only), any
     network use is pointed at a dead proxy, and HF_HUB_OFFLINE=1.
  2. ~/.mempalace/config.json is hashed (shasum) and the real palace file is stat'ed
     (size, mtime) before and after; at the end `lsof -p <this pid>` must show no file under
     ~/.mempalace or the owner's palace folder.
"""

import hashlib
import json
import os
import pwd
import subprocess
import sys
import tempfile
from pathlib import Path

REAL_HOME = Path(pwd.getpwuid(os.getuid()).pw_dir)
REAL_CONFIG = REAL_HOME / ".mempalace" / "config.json"
REAL_PALACE_DIRS = [REAL_HOME / ".mempalace", Path("/Users/icue/Code/AI/memory_vault")]
# Candidate palace files (stat only, never opened).
REAL_PALACE_FILES = [
    REAL_HOME / ".mempalace" / "palace" / "sqlite_exact.sqlite3",
    REAL_HOME / ".mempalace" / "palace" / "chroma.sqlite3",
]
VENV_PY = "/Users/icue/.venv/bin/python"
ROOT = Path(__file__).resolve().parent.parent
CORPUS = ROOT / "crates/alice-daemon/tests/data/gold_corpus.json"
OUT = ROOT / "crates/alice-daemon/tests/data/mempalace_baseline.json"
TOP = 10


def shasum(path):
    if not path.exists():
        return "absent"
    out = subprocess.run(["/usr/bin/shasum", "-a", "1", str(path)], capture_output=True, text=True, check=True)
    return out.stdout.split()[0]


def stat_of(path):
    try:
        st = os.stat(path)
    except OSError:
        return "absent"
    return f"size={st.st_size} mtime={st.st_mtime_ns}"


def snapshot():
    return {
        "config_sha1": shasum(REAL_CONFIG),
        "palace_stat": {str(p): stat_of(p) for p in REAL_PALACE_FILES + REAL_PALACE_DIRS},
    }


def lsof_violations():
    out = subprocess.run(["/usr/sbin/lsof", "-p", str(os.getpid())], capture_output=True, text=True)
    bad = []
    for line in out.stdout.splitlines()[1:]:
        for root in REAL_PALACE_DIRS:
            if str(root) in line:
                bad.append(line)
    return bad, out.stdout.count("\n") - 1


def main():
    if sys.flags.isolated != 1:
        print("run with python -I", file=sys.stderr)
        return 2
    before = snapshot()
    print("before:", json.dumps(before))
    tmp = Path(tempfile.mkdtemp(prefix="m3-mp-baseline-"))
    try:
        home = tmp / "home"
        palace = tmp / "palace"
        state = tmp / "state"
        for d in (home, palace, state):
            d.mkdir()
        model_src = REAL_HOME / ".cache/chroma/onnx_models/all-MiniLM-L6-v2"
        if not (model_src / "onnx" / "model.onnx").exists():
            print("STOP: the MiniLM embedding model is not on disk; nothing is downloaded.", file=sys.stderr)
            return 4
        link = home / ".cache/chroma/onnx_models"
        link.mkdir(parents=True)
        (link / "all-MiniLM-L6-v2").symlink_to(model_src)

        os.environ.update(
            {
                "HOME": str(home),
                "MEMPALACE_PALACE_PATH": str(palace),
                "MEMPALACE_HUB_FORWARD": "0",
                "MEMPALACE_DAEMON_STATE_ROOT": str(state),
                "MEMPALACE_BACKEND": "sqlite_exact",
                "HF_HUB_OFFLINE": "1",
                "TRANSFORMERS_OFFLINE": "1",
                "HTTP_PROXY": "http://127.0.0.1:9",
                "HTTPS_PROXY": "http://127.0.0.1:9",
                "ALL_PROXY": "http://127.0.0.1:9",
            }
        )
        for k in list(os.environ):
            if k.startswith("MEMPAL") and k not in (
                "MEMPALACE_PALACE_PATH",
                "MEMPALACE_HUB_FORWARD",
                "MEMPALACE_DAEMON_STATE_ROOT",
                "MEMPALACE_BACKEND",
            ):
                del os.environ[k]
        sys.path.insert(0, "/Users/icue/.venv/lib/python3.13/site-packages")

        from mempalace.embedding import current_model_name
        from mempalace.palace import get_collection
        from mempalace.searcher import search_memories
        from mempalace.version import __version__

        corpus = json.loads(CORPUS.read_text())
        col = get_collection(str(palace), create=True, backend="sqlite_exact")
        ids, docs, metas = [], [], []
        for d in corpus["drawers"]:
            m = d["meta"]
            meta = {"wing": m["wing"], "room": m["room"], "added_by": m["added_by"], "filed_at": m["created_at"].rstrip("Z")}
            if m.get("source_file"):
                meta["source_file"] = m["source_file"]
            if m.get("chunk_index") is not None:
                meta["chunk_index"] = m["chunk_index"]
            ids.append(d["id"])
            docs.append(d["text"])
            metas.append(meta)
        for i in range(0, len(ids), 32):
            col.upsert(documents=docs[i : i + 32], ids=ids[i : i + 32], metadatas=metas[i : i + 32])
        stored = col.count()
        assert stored == len(ids), (stored, len(ids))

        queries = {}
        for q in corpus["queries"]:
            res = search_memories(q["q"], str(palace), n_results=TOP, collection_name=None)
            if res.get("error"):
                print("search error:", res, file=sys.stderr)
                return 5
            queries[q["id"]] = [h["drawer_id"] for h in res["results"]][:TOP]

        bad, nfiles = lsof_violations()
        after = snapshot()
        print("after:", json.dumps(after))
        print(f"lsof: {nfiles} open files, {len(bad)} under the owner's palace or ~/.mempalace")
        if bad or after != before:
            print("ABORT: safety check failed", bad, file=sys.stderr)
            return 3
        OUT.write_text(
            json.dumps(
                {
                    "mempalace_version": __version__,
                    "embedding_model": current_model_name(),
                    "backend": "sqlite_exact",
                    "corpus_sha1": hashlib.sha1(CORPUS.read_bytes()).hexdigest(),
                    "queries": queries,
                },
                indent=1,
                ensure_ascii=False,
            )
            + "\n"
        )
        print(f"wrote {OUT} ({len(queries)} queries, {stored} drawers)")
        return 0
    finally:
        import shutil

        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
