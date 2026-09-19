#!/usr/bin/env python3
"""Reproducible, recall-first jegrep experiments. Python standard library only."""
from __future__ import annotations

import argparse
import contextlib
import csv
import fcntl
import hashlib
import itertools
import json
import math
import os
from pathlib import Path
import statistics
import subprocess
import sys
import tempfile
import time

REPO = Path(__file__).resolve().parents[1]
KNOBS = {"parallel", "batch", "max_batch", "thresholds", "bytes", "ranges", "min_hits"}
DEFAULTS = dict(parallel=16, batch=64, max_batch=128, thresholds=[0.4, 0.2],
                bytes=32768, ranges=16, min_hits=1)


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True).encode()).hexdigest()


def read_json(path):
    with open(path) as f:
        return json.load(f)


def git(root, *args):
    result = subprocess.run(["git", "-C", str(root), *args], capture_output=True, text=True)
    return result.stdout.strip() if result.returncode == 0 else None


def relative_path(value):
    p = Path(value)
    if not value or p.is_absolute() or ".." in p.parts or "\\" in value:
        raise ValueError(f"expected a root-relative path, got {value!r}")
    return p.as_posix() + ("/" if value.endswith("/") else "")


def load_suite(source, root=None, allow_revision_mismatch=False):
    source = Path(source).resolve()
    tag = read_json(source / "tag.json") if source.is_dir() and (source / "tag.json").exists() else {}
    if source.is_dir():
        files = sorted(source.glob("query*.json"))
        if not files:
            raise ValueError(f"no query*.json files in {source}")
        raw = []
        for p in files:
            item = read_json(p)
            raw.append(dict(item, name=item.get("name", p.stem)))
    else:
        raw = read_json(source)
        if not isinstance(raw, list):
            raw = [dict(raw, name=raw.get("name", source.stem))]
    if root is None:
        if not tag.get("local"):
            raise ValueError("supply --root; this suite has no tag.json local checkout")
        root = Path(tag["local"])
        if not root.is_absolute():
            root = source / root
    root = Path(root).resolve()
    if not root.is_dir():
        raise ValueError(f"target checkout does not exist: {root}; supply --root")
    head = git(root, "rev-parse", "HEAD")
    dirty = git(root, "status", "--porcelain", "--untracked-files=normal")
    if tag.get("hash") and (head != tag["hash"] or dirty) and not allow_revision_mismatch:
        raise ValueError(f"target must be clean at {tag['hash']}; got {head}, dirty={bool(dirty)}. "
                         "Use --allow-revision-mismatch to explicitly score a different checkout.")
    cases, names = [], set()
    for item in raw:
        name = item.get("name")
        if not name or name in names or not item.get("query", "").strip():
            raise ValueError(f"missing/duplicate case name or empty query: {name!r}")
        names.add(name)
        matches = item.get("matches", [])
        expected = sorted({relative_path(m["file"]) for m in matches} |
                          {relative_path(p) for p in item.get("expect", [])})
        negative = sorted({relative_path(p) for p in item.get("irrelevant", [])})
        if set(expected) & set(negative):
            raise ValueError(f"{name}: a file is labeled both relevant and irrelevant")
        for p in expected + negative:
            target = (root / p).resolve()
            if not target.is_relative_to(root) or not target.exists():
                raise ValueError(f"{name}: missing or out-of-root ground truth {p}")
        spans = []
        for m in matches:
            p = relative_path(m["file"])
            start, end = m.get("start_line"), m.get("end_line")
            if start is None and end is None:
                continue
            lines = len((root / p).read_text(errors="replace").splitlines())
            if type(start) is not int or type(end) is not int or not 1 <= start <= end <= lines:
                raise ValueError(f"{name}: invalid span {p}:{start}-{end} ({lines} lines)")
            span = dict(file=p, start=start, end=end, symbol=m.get("symbol"))
            if span not in spans:
                spans.append(span)
        cases.append(dict(name=name, query=item["query"], expected=expected,
                          spans=spans, irrelevant=negative, keywords=item.get("keywords", []),
                          exhaustive=item.get("exhaustive", tag.get("exhaustive", False))))
    if not cases:
        raise ValueError("empty benchmark suite")
    return cases, root, dict(source=str(source), tag=tag, target_head=head,
                            target_dirty=bool(dirty), target_diff=digest(git(root, "diff", "HEAD")))


def matches_path(pattern, path):
    return path.startswith(pattern) if pattern.endswith("/") else path == pattern


def merge(intervals):
    result = []
    for start, end in sorted(intervals):
        if result and start <= result[-1][1] + 1:
            result[-1] = (result[-1][0], max(end, result[-1][1]))
        else:
            result.append((start, end))
    return result


def length(intervals):
    return sum(b - a + 1 for a, b in merge(intervals))


def intersection(a, b):
    return length([(max(x, u), min(y, v)) for x, y in merge(a) for u, v in merge(b)
                   if max(x, u) <= min(y, v)])


def score(case, result, top_ranges=3, closed_world=False):
    """No answer keywords enter search unless explicitly requested by the caller."""
    hits = {h["path"]: h for h in result.get("hits", [])}
    expected = case["expected"]
    relevant = [p for p in hits if any(matches_path(e, p) for e in expected)]
    missed = [e for e in expected if not any(matches_path(e, p) for p in hits)]
    extras = [p for p in hits if p not in relevant]
    closed = closed_world or case.get("exhaustive", False)
    false = [p for p in extras if closed or any(matches_path(e, p) for e in case["irrelevant"])]
    unknown = [p for p in extras if p not in false]
    recall = (len(expected) - len(missed)) / len(expected) if expected else 1.0
    lower_precision = len(relevant) / len(hits) if hits else (0.0 if expected else 1.0)
    precision = lower_precision if closed or not unknown else None
    f2 = 5 * precision * recall / (4 * precision + recall) if precision is not None and (precision or recall) else (0.0 if precision is not None else None)
    predictions = {}
    for p, h in hits.items():
        ranges = sorted(h.get("ranges", []), key=lambda r: (-r.get("p", 0), r["start"]))
        predictions[p] = merge([(r["start"], r["end"]) for r in ranges[:top_ranges]
                                if r.get("p", 0) > 0 and 1 <= r["start"] <= r["end"]])
    covered = []
    gold = {}
    for span in case["spans"]:
        p, start, end = span["file"], span["start"], span["end"]
        gold.setdefault(p, []).append((start, end))
        n = intersection(predictions.get(p, []), [(start, end)])
        covered.append(dict(**span, covered_lines=n, coverage=n / (end - start + 1)))
    gold_lines = sum(length(v) for v in gold.values())
    predicted_lines = sum(length(v) for v in predictions.values())
    overlap = sum(intersection(v, predictions.get(p, [])) for p, v in gold.items())
    span_recall = sum(c["covered_lines"] > 0 for c in covered) / len(covered) if covered else None
    ranks = [i + 1 for i, p in enumerate(hits) if p in relevant]
    return dict(tp=len(relevant), fp=len(false), fn=len(missed), unjudged=len(unknown),
                expected_files=len(expected), returned_files=len(hits), file_recall=recall,
                precision=precision, precision_lower_bound=lower_precision, f2=f2,
                query_success=bool(relevant) if expected else not hits,
                complete_files=not missed, reciprocal_rank=1 / ranks[0] if ranks else 0.0,
                span_recall=span_recall,
                mean_span_coverage=statistics.mean(c["coverage"] for c in covered) if covered else None,
                line_recall=overlap / gold_lines if gold_lines else None,
                line_precision_lower_bound=overlap / predicted_lines if predicted_lines and gold_lines else None,
                gold_lines=gold_lines, predicted_lines=predicted_lines, overlapping_lines=overlap,
                complete_spans=all(c["covered_lines"] > 0 for c in covered) if covered else None,
                relevant_files=relevant, false_positive_files=false, missed_files=missed,
                unjudged_files=unknown, spans=covered, closed_world=closed)


def configurations(args, available):
    strategies = available if args.strategies == ["all"] else args.strategies
    base = {k: getattr(args, k) for k in KNOBS}
    variants = read_json(args.configs) if args.configs else [dict(strategy=s) for s in strategies]
    if not isinstance(variants, list) or not variants:
        raise ValueError("--configs must contain a nonempty array")
    grid = read_json(args.grid) if args.grid else {}
    if any(k not in KNOBS or not isinstance(v, list) or not v for k, v in grid.items()):
        raise ValueError(f"grid keys must be tuning knobs {sorted(KNOBS)} with nonempty arrays")
    result, names = [], set()
    env_base = {k: v for k, v in os.environ.items() if k.startswith("JEGREP_") and k != "JEGREP_DUMP"}
    for variant in variants:
        if set(variant) - KNOBS - {"name", "strategy", "env"}:
            raise ValueError(f"unknown configuration fields: {set(variant) - KNOBS - {'name', 'strategy', 'env'}}")
        strategy = variant.get("strategy", strategies[0])
        if strategy not in available:
            raise ValueError(f"unknown strategy {strategy!r}; available: {', '.join(available)}")
        env = dict(env_base, **variant.get("env", {}))
        if any(not k.startswith("JEGREP_") or k == "JEGREP_DUMP" for k in env):
            raise ValueError("configuration env may only set JEGREP_* tuning variables, excluding JEGREP_DUMP")
        env = {k: str(v) for k, v in env.items()}
        for values in itertools.product(*grid.values()):
            knobs = dict(base, **{k: v for k, v in variant.items() if k in KNOBS})
            knobs.update(zip(grid, values))
            for k in KNOBS - {"thresholds"}:
                if type(knobs[k]) is not int or knobs[k] < 1:
                    raise ValueError(f"{k} must be a positive integer")
            if not 1 <= knobs["batch"] <= knobs["max_batch"] <= 255 or not 1 <= knobs["ranges"] <= 255 or knobs["bytes"] < 256:
                raise ValueError("require 1 <= batch <= max_batch <= 255, ranges <= 255, bytes >= 256")
            if not isinstance(knobs["thresholds"], list) or not knobs["thresholds"] or any(not 0 <= t <= 1 for t in knobs["thresholds"]):
                raise ValueError("thresholds must be a nonempty array of probabilities")
            name = variant.get("name", strategy)
            if grid:
                name += "-" + "-".join(f"{k}={v}" for k, v in zip(grid, values))
            if name in names:
                raise ValueError(f"duplicate configuration name: {name}")
            names.add(name)
            result.append(dict(name=name, strategy=strategy, knobs=knobs, env=env))
    return result


@contextlib.contextmanager
def request_lock(enabled):
    """Serialize separate experiment processes so latency comparisons are meaningful."""
    if not enabled:
        yield
        return
    path = Path(tempfile.gettempdir()) / f"jegrep-benchmark-{os.getuid()}.lock"
    with open(path, "a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        try:
            yield
        finally:
            fcntl.flock(lock, fcntl.LOCK_UN)


def command_for(binary, root, case, config, args):
    command = [str(binary), "--json", "--quiet", "--strategy", config["strategy"], "--model", args.model]
    for k, v in config["knobs"].items():
        command += ["--" + k.replace("_", "-"), ",".join(map(str, v)) if isinstance(v, list) else str(v)]
    if args.endpoint:
        command += ["--endpoint", args.endpoint]
    if args.hidden:
        command.append("--hidden")
    if args.use_keywords and case["keywords"]:
        command += ["--keywords", ",".join(case["keywords"])]
    return command + ["--", case["query"], str(root)]


def run_one(binary, root, case, config, args):
    command = command_for(binary, root, case, config, args)
    env = dict(os.environ, **config["env"])
    env.pop("JEGREP_DUMP", None)
    with request_lock(not args.allow_concurrent):
        started = time.monotonic()
        try:
            proc = subprocess.run(command, capture_output=True, text=True, env=env, timeout=args.timeout)
            wall = time.monotonic() - started
            if proc.returncode:
                return dict(status="failed", error=proc.stderr[-4000:], wall_seconds=wall, result=None)
            result = json.loads(proc.stdout)
            if not isinstance(result.get("hits"), list) or not isinstance(result.get("stats"), dict):
                raise ValueError("search returned no hits/stats arrays")
            return dict(status="api_errors" if result["stats"].get("errors") else "ok",
                        wall_seconds=wall, result=result, stderr=proc.stderr[-4000:])
        except subprocess.TimeoutExpired:
            return dict(status="timeout", error=f"exceeded {args.timeout}s", wall_seconds=time.monotonic() - started, result=None)
        except (ValueError, OSError) as e:
            return dict(status="failed", error=str(e), wall_seconds=time.monotonic() - started, result=None)


def aggregate(rows, planned):
    groups = {}
    for row in rows:
        key = row["config"]["name"]
        groups.setdefault(key, []).append(row)
    summaries = []
    for name, rs in groups.items():
        scores = [r["score"] for r in rs]
        def mean(key):
            vals = [s[key] for s in scores if s.get(key) is not None]
            return statistics.mean(vals) if vals else None
        wall = sorted(r["wall_seconds"] for r in rs)
        stats = [(r.get("result") or {}).get("stats", {}) for r in rs]
        target_files = sum(s["tp"] + s["fn"] for s in scores)
        regions = [span for s in scores for span in s.get("spans", [])]
        found_regions = sum(span["covered_lines"] > 0 for span in regions)
        errors = sum(r["status"] != "ok" for r in rs)
        budget_known = all(r.get("result") is not None and not r["result"]["stats"].get("errors") for r in rs)
        summaries.append(dict(name=name, strategy=rs[0]["config"]["strategy"], runs=len(rs),
                              planned_runs=planned, completed=len(rs) == planned,
                              failures=errors, query_success=mean("query_success"),
                              file_recall=mean("file_recall"), worst_file_recall=min(s["file_recall"] for s in scores),
                              micro_file_recall=sum(s["tp"] for s in scores) / target_files if target_files else 1.0,
                              micro_span_recall=found_regions / len(regions) if regions else None,
                              span_tp=found_regions, span_fn=len(regions) - found_regions,
                              complete_files=mean("complete_files"), span_recall=mean("span_recall"),
                              mean_span_coverage=mean("mean_span_coverage"), line_recall=mean("line_recall"),
                              line_precision_lower_bound=mean("line_precision_lower_bound"),
                              precision=mean("precision") if all(s["precision"] is not None for s in scores) else None,
                              precision_lower_bound=mean("precision_lower_bound"), f2=mean("f2") if all(s["f2"] is not None for s in scores) else None,
                              tp=sum(s["tp"] for s in scores), fp=sum(s["fp"] for s in scores),
                              fn=sum(s["fn"] for s in scores), unjudged=sum(s["unjudged"] for s in scores),
                              median_seconds=statistics.median(wall), p95_seconds=wall[math.ceil(len(wall)*.95)-1],
                              total_seconds=sum(wall), usd=sum(s.get("usd", 0) for s in stats),
                              input_tokens=sum(s.get("input_tokens", 0) for s in stats),
                              requests=sum(s.get("requests", 0) for s in stats),
                              http_attempts=sum(s["http_attempts"] for s in stats) if all("http_attempts" in s for s in stats) else None,
                              retries=sum(s.get("retries", 0) for s in stats), cost_complete=budget_known))
    # Partial/failed experiments cannot win by doing less work. No scalar blends quality with cost.
    eligible = [s for s in summaries if s["completed"] and not s["failures"] and s["cost_complete"]]
    def objectives(s):
        return (s["file_recall"], s["micro_file_recall"], s["span_recall"] or 0,
                s["micro_span_recall"] or 0, s["line_recall"] or 0,
                s["precision_lower_bound"], s["line_precision_lower_bound"] or 0,
                -s["usd"], -s["median_seconds"])
    for s in summaries:
        a = objectives(s)
        s["pareto"] = s in eligible and not any(
            all(x >= y for x, y in zip(objectives(other), a)) and any(x > y for x, y in zip(objectives(other), a))
            for other in eligible)
    return sorted(summaries, key=lambda s: (not s["completed"], s["failures"], -s["file_recall"],
                                          -(s["span_recall"] or 0), -(s["line_recall"] or 0), s["usd"], s["median_seconds"]))


def percent(value):
    return "—" if value is None else f"{100 * value:.1f}%"


def write_summary(out, rows, manifest):
    summaries = aggregate(rows, len(manifest["cases"]) * manifest["repeat"])
    (out / "summary.json").write_text(json.dumps(summaries, indent=2) + "\n")
    if summaries:
        with open(out / "summary.csv", "w") as f:
            writer = csv.DictWriter(f, fieldnames=list(summaries[0]))
            writer.writeheader()
            writer.writerows(summaries)
    lines = ["# jegrep benchmark", "", f"Suite: `{manifest['suite']['source']}`. Keywords: **{'oracle-assisted' if manifest['use_keywords'] else 'query only'}**.",
             "", "Recall and region coverage are primary objectives. Cost and latency stay separate. "
             "★ marks a Pareto candidate among complete, error-free experiments. Costs use jegrep's configured estimate; failed calls may incur unreported costs.",
             "", "P≥ is the lower bound on precision against positive labels. Extra hits are unjudged unless labeled irrelevant or evaluated in closed-world mode. "
             "Region recall means any overlap in the top-ranked ranges; line recall measures how much annotated code was covered. All quality percentages below are macro averages across cases/repeats.",
             "", "| Config | Runs | File recall | Region recall | Line recall | P≥ | FP / FN / ? | Median s | p95 s | Est. $ | Errors | Pareto |",
             "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|"]
    for s in summaries:
        lines.append(f"| {s['name']} | {s['runs']}/{s['planned_runs']} | {percent(s['file_recall'])} | {percent(s['span_recall'])} | {percent(s['line_recall'])} | {percent(s['precision_lower_bound'])} | {s['fp']} / {s['fn']} / {s['unjudged']} | {s['median_seconds']:.2f} | {s['p95_seconds']:.2f} | {s['usd']:.5f} | {s['failures']} | {'★' if s['pareto'] else ''} |")
    (out / "summary.md").write_text("\n".join(lines) + "\n")
    return summaries


def parser():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.ArgumentDefaultsHelpFormatter)
    p.add_argument("suite", type=Path, help="folder of query*.json + tag.json, or legacy cases JSON")
    p.add_argument("--root", type=Path, help="override tag.json local checkout")
    p.add_argument("--strategies", nargs="+", default=["cascade"], help="strategy names, or all")
    p.add_argument("--configs", type=Path, help="JSON array of named strategy/parameter/env configurations")
    p.add_argument("--grid", type=Path, help="JSON object of parameter value arrays; Cartesian product")
    p.add_argument("--binary", type=Path, default=REPO / "target/release/jegrep")
    p.add_argument("--out", type=Path, default=REPO / "bench/runs" / time.strftime("%Y%m%d-%H%M%S"))
    p.add_argument("--repeat", type=int, default=1)
    p.add_argument("--cases", nargs="+", help="case names to run (recorded in manifest)")
    p.add_argument("--timeout", type=float, default=180, help="per query process timeout in seconds")
    p.add_argument("--max-usd", type=float, help="stop between queries once recorded cost reaches this amount")
    p.add_argument("--resume", action="store_true", help="resume an identical manifest; completed rows are never silently replaced")
    p.add_argument("--dry-run", action="store_true", help="validate suite, checkout and plan without API calls")
    p.add_argument("--closed-world", action="store_true", help="assume all non-gold files are false positives")
    p.add_argument("--use-keywords", action="store_true", help="oracle-assisted track using annotated keywords; off for honest query-only search")
    p.add_argument("--top-ranges", type=int, default=3, help="top positive-probability ranges per returned file to score")
    p.add_argument("--allow-concurrent", action="store_true", help="allow API overlap with other runners; makes timing less comparable")
    p.add_argument("--allow-revision-mismatch", action="store_true")
    p.add_argument("--model", default="jev-latest")
    p.add_argument("--endpoint", choices=["openrouter", "typesafe"])
    p.add_argument("--hidden", action="store_true")
    for key, default in DEFAULTS.items():
        p.add_argument("--" + key.replace("_", "-"), default=default,
                       type=(lambda s: [float(v) for v in s.split(",")]) if key == "thresholds" else int)
    return p


def main(argv=None):
    args = parser().parse_args(argv)
    if args.repeat < 1 or args.timeout <= 0 or args.top_ranges < 1 or (args.max_usd is not None and args.max_usd <= 0):
        raise ValueError("repeat, timeout, top-ranges, and max-usd must be positive")
    cases, root, suite = load_suite(args.suite, args.root, args.allow_revision_mismatch)
    if args.cases:
        missing = set(args.cases) - {c["name"] for c in cases}
        if missing:
            raise ValueError(f"unknown cases: {sorted(missing)}")
        cases = [c for c in cases if c["name"] in args.cases]
    binary = args.binary.resolve()
    if not binary.is_file():
        raise ValueError(f"missing {binary}; run cargo build --release first")
    available = subprocess.check_output([str(binary), "--list-strategies"], text=True).split()
    configs = configurations(args, available)
    manifest = dict(schema=1, suite=suite, root=str(root), cases=cases, configs=configs,
                    repeat=args.repeat, binary=str(binary), binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
                    model=args.model, endpoint=args.endpoint, hidden=args.hidden, use_keywords=args.use_keywords,
                    closed_world=args.closed_world, top_ranges=args.top_ranges, timeout=args.timeout,
                    allow_concurrent=args.allow_concurrent, runner_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest())
    run_id = digest(manifest)
    if args.dry_run:
        print(json.dumps(dict(run_id=run_id, queries=len(cases), configurations=len(configs),
                              invocations=len(cases)*len(configs)*args.repeat, manifest=manifest), indent=2))
        return 0
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    # Refuse two writers to the same experiment even while either waits for the API lock.
    with open(out / ".writer.lock", "a") as writer_lock:
        try:
            fcntl.flock(writer_lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise ValueError(f"another runner is writing {out}") from None
        manifest_path, rows_path = out / "manifest.json", out / "rows.jsonl"
        if manifest_path.exists():
            if not args.resume or read_json(manifest_path) != manifest:
                raise ValueError(f"{out} already contains an experiment; use a new --out or --resume with identical settings/binary")
        elif rows_path.exists():
            raise ValueError("rows.jsonl exists without a manifest; use a new output directory")
        manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
        rows = []
        if rows_path.exists():
            for n, line in enumerate(rows_path.read_text().splitlines(), 1):
                try:
                    row = json.loads(line)
                    if row["run_id"] != run_id:
                        raise ValueError("foreign run id")
                    rows.append(row)
                except (ValueError, KeyError) as e:
                    raise ValueError(f"invalid saved row {n}; preserve and repair it before resuming: {e}") from e
        seen = {(r["config"]["name"], r["case"], r["repeat"]) for r in rows}
        if len(seen) != len(rows):
            raise ValueError("duplicate saved rows; refusing biased aggregation")
        spent = sum((r.get("result") or {}).get("stats", {}).get("usd", 0) for r in rows)
        total = len(cases) * len(configs) * args.repeat
        try:
            with open(rows_path, "a") as output:
                for repeat in range(1, args.repeat + 1):
                    # Rotate configuration order between cases/repeats to reduce drift bias.
                    for ci, case in enumerate(cases):
                        offset = (ci + repeat - 1) % len(configs)
                        for config in configs[offset:] + configs[:offset]:
                            key = (config["name"], case["name"], repeat)
                            if key in seen:
                                continue
                            if args.max_usd is not None and spent >= args.max_usd:
                                print(f"Recorded cost ${spent:.5f} reached budget; remaining runs skipped.", file=sys.stderr)
                                return 3
                            print(f"[{len(rows)+1}/{total}] {config['name']} · {case['name']} · repeat {repeat}", file=sys.stderr, flush=True)
                            outcome = run_one(binary, root, case, config, args)
                            row = dict(run_id=run_id, config=config, case=case["name"], repeat=repeat,
                                       timestamp=time.time(), **outcome,
                                       score=score(case, outcome["result"] or {"hits": []}, args.top_ranges, args.closed_world))
                            output.write(json.dumps(row) + "\n")
                            output.flush()
                            os.fsync(output.fileno())
                            rows.append(row)
                            seen.add(key)
                            spent += (row.get("result") or {}).get("stats", {}).get("usd", 0)
                            print(f"  {row['status']}: recall {percent(row['score']['file_recall'])}, regions {percent(row['score']['span_recall'])}, {row['wall_seconds']:.2f}s, cumulative ${spent:.5f}", file=sys.stderr, flush=True)
                            write_summary(out, rows, manifest)
        finally:
            write_summary(out, rows, manifest)
        print((out / "summary.md").read_text())
        return 1 if any(r["status"] != "ok" for r in rows) else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        print(f"bench: {error}", file=sys.stderr)
        sys.exit(2)
    except KeyboardInterrupt:
        print("Interrupted; completed rows are saved. Resume with identical options and --resume.", file=sys.stderr)
        sys.exit(130)
