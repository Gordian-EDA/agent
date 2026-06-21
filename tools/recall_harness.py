#!/usr/bin/env python3
"""Ground-truth recall harness for the design reviewer + an ENSEMBLE-size sweep.

For each known-good base design and each defect type, inject a KNOWN defect (we know the answer),
run the reviewer N times, and measure recall when taking the UNION of high-confidence critical/major
defects over the first 1, 2, 3 ... runs. This (a) measures true recall against ground truth — no
LLM-grading-LLM circularity — and (b) quantifies how much an ENSEMBLE (union of independent runs)
beats a single run, so the production ensemble size is chosen by data.

  set -a; . ./.env; set +a
  python3 tools/recall_harness.py --bases docs/validation/idiom-stm32.circuit.yaml ... \
          --types pinswap value disconnect --runs 3
"""
import argparse, json, subprocess, sys, tempfile, os, collections


def critic_defects(yaml_path, intent, focus=""):
    """One reviewer run → list of high-confidence critical/major defect refdeses (lowercased).
    `focus` makes this run a diverse-lens ensemble member (extra emphasis on a fault class)."""
    cmd = [sys.executable, "tools/design_critic.py", yaml_path, "--intent", intent,
           "--samples", "1", "--json-only"]
    if focus:
        cmd += ["--focus", focus]
    return _critic_run(cmd)


def run_erc(yaml_path, erc_bin):
    """Deterministic ERC findings for one design (lowercased stdout) — the exact-math layer."""
    out = subprocess.run([erc_bin, yaml_path], capture_output=True, text=True)
    return out.stdout.lower()


def _critic_run(cmd):
    out = subprocess.run(cmd, capture_output=True, text=True)
    try:
        v = json.loads(out.stdout.strip().splitlines()[-1])
    except Exception:
        return None  # parse/gateway failure
    hits = []
    for d in v.get("defects", []):
        if d.get("severity") in ("critical", "major") and d.get("confidence") == "high":
            hits.append(str(d.get("refdes", "")).strip().lower())
    return hits


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bases", nargs="+", required=True)
    ap.add_argument("--types", nargs="+", default=["pinswap", "value", "disconnect"])
    ap.add_argument("--seeds", nargs="+", type=int, default=[1])
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--focuses", nargs="+", default=[],
                    help="diverse-lens ensemble: each 'run' uses one of these focus emphases instead "
                    "of N identical runs (proves whether DIVERSE lenses beat repeated same-prompt runs)")
    ap.add_argument("--erc-bin", help="path to the erc_check binary; if set, union the DETERMINISTIC "
                    "ERC findings and report combined recall (LLM ensemble ∪ exact-math ERC)")
    args = ap.parse_args()
    lenses = args.focuses if args.focuses else None
    nruns = len(lenses) if lenses else args.runs

    cases = []  # (base, type, gt_refdes, detail, [hits_run0, hits_run1, ...])
    for base in args.bases:
        name = os.path.basename(base).replace(".circuit.yaml", "").replace(".yaml", "")
        seen = set()  # (dtype, refdes) — different seeds can hit the same target; dedup
        for dtype in args.types:
            for seed in args.seeds:
                with tempfile.NamedTemporaryFile("w", suffix=".yaml", delete=False) as tf:
                    mut = tf.name
                inj = subprocess.run(
                    [sys.executable, "tools/inject_defect.py", base, dtype, mut, "--seed", str(seed)],
                    capture_output=True, text=True)
                try:
                    gt = json.loads(inj.stdout.strip())
                except Exception:
                    os.unlink(mut)
                    continue
                if "error" in gt:
                    os.unlink(mut)
                    continue
                gt_ref = gt["refdes"].lower()
                if (dtype, gt_ref) in seen:
                    os.unlink(mut)
                    continue
                seen.add((dtype, gt_ref))
                if lenses:
                    runs = [critic_defects(mut, f"{name} circuit", f) for f in lenses]
                else:
                    runs = [critic_defects(mut, f"{name} circuit") for _ in range(args.runs)]
                erc_out = run_erc(mut, args.erc_bin) if args.erc_bin else ""
                os.unlink(mut)
                cases.append((name, dtype, gt_ref, gt["detail"], runs, erc_out))
                caught_each = ["Y" if (h is not None and gt_ref in h) else "." for h in runs]
                erc_mark = " erc:E" if (args.erc_bin and gt_ref in erc_out) else ""
                print(f"  {name:24} {dtype:10} {gt_ref:6} runs[{''.join(caught_each)}]{erc_mark}  {gt['detail'][:50]}")

    # Recall at ensemble size k = union of the first k runs catches the injected refdes.
    label = "diverse lenses" if lenses else "repeated runs"
    print(f"\n=== ensemble recall (union of first k {label}) ===")
    total = len(cases)
    for k in range(1, nruns + 1):
        caught = 0
        for _, _, gt_ref, _, runs, _erc in cases:
            union = set()
            for h in runs[:k]:
                if h:
                    union.update(h)
            if gt_ref in union:
                caught += 1
        print(f"  N={k}: recall {caught}/{total} = {caught/total*100:.0f}%" if total else "  (no cases)")

    # Per-type breakdown at the max ensemble size.
    print("\n=== per-defect-type recall (N=%d) ===" % nruns)
    by_type = collections.defaultdict(lambda: [0, 0])
    for _, dtype, gt_ref, _, runs, _erc in cases:
        union = set()
        for h in runs:
            if h:
                union.update(h)
        by_type[dtype][1] += 1
        by_type[dtype][0] += 1 if gt_ref in union else 0
    for dtype, (c, t) in by_type.items():
        print(f"  {dtype:11}: {c}/{t}")

    # Combined recall: the diverse-lens LLM ensemble UNIONED with the deterministic exact-math ERC.
    if args.erc_bin and total:
        print("\n=== combined recall: LLM ensemble ∪ deterministic ERC ===")
        caught, erc_only = 0, 0
        for _, _, gt_ref, _, runs, erc_out in cases:
            llm = any(h and gt_ref in h for h in runs)
            erc = gt_ref in erc_out
            if llm or erc:
                caught += 1
            if erc and not llm:
                erc_only += 1
        print(f"  combined: {caught}/{total} = {caught/total*100:.0f}%  "
              f"(deterministic ERC recovered {erc_only} that the LLM ensemble missed)")


if __name__ == "__main__":
    main()
