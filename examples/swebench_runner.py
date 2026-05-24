"""Drive SWE-bench Verified gold patches end-to-end through Arlee.

Serves as an infrastructure regression test: each task's published gold
patch is the "agent output", so all 3 tasks should produce `resolved=True`
as reported by `swebench.harness.grading.get_eval_report` (every test in
FAIL_TO_PASS goes green and every test in PASS_TO_PASS stays green).

Run from the Apiserver VM (where the Python SDK + swebench package are
installed), with ARLEE_APISERVER and ARLEE_TOKEN exported:

    sudo -E /opt/arlee-venv/bin/python /opt/arlee/examples/swebench_runner.py --gold

By default it runs 3 hardcoded instance IDs concurrently so the Apiserver
spreads them across the available Edges. Each instance pulls a ~1-3 GB
docker image on first use, so first run is slow.
"""

from __future__ import annotations

import argparse
import asyncio
import os
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import arlee
from arlee import Sandbox

# Three SWE-bench Verified instance IDs chosen for smallest combined test count.
DEFAULT_INSTANCE_IDS = [
    "sympy__sympy-14711",                      # F2P=1, P2P=2
    "django__django-12419",                    # F2P=1, P2P=0
    "scikit-learn__scikit-learn-14141",        # F2P=1, P2P=2
]

# Long client timeout: SWE-bench images are 1-3 GB so first-time pull alone can
# take 1-5 min, and the eval pytest run can also be multi-minute.
CLIENT_TIMEOUT_SECONDS = 1800.0


def docker_hub_image(instance_id: str) -> str:
    """SWE-bench publishes images at swebench/sweb.eval.x86_64.<munged>:latest
    where every `__` in the instance_id is replaced with `_1776_` (Docker
    image name conventions / SWE-bench internal marker)."""
    munged = instance_id.replace("__", "_1776_")
    return f"swebench/sweb.eval.x86_64.{munged}:latest"


@dataclass
class InstanceResult:
    instance_id: str
    resolved: bool
    detail: str
    sandbox_id: str | None = None
    edge_id: str | None = None
    eval_log_path: Path | None = None


async def run_instance(instance: dict[str, Any], log_dir: Path) -> InstanceResult:
    from swebench.harness.grading import get_eval_report
    from swebench.harness.test_spec.test_spec import make_test_spec

    inst_id = instance["instance_id"]
    image = docker_hub_image(inst_id)
    test_spec = make_test_spec(instance)
    gold_patch = instance["patch"]

    async with await arlee.create_sandbox(image=image, timeout=1800.0) as sb:
        try:
            # Apply the gold patch (the "model output" we're evaluating).
            # eval_script applies test_patch but NOT the model patch.
            await sb.write_file("/tmp/gold.patch", gold_patch.encode())
            gold_apply = await sb.exec(
                "git apply /tmp/gold.patch", cwd="/testbed", timeout=120
            )
            if gold_apply.exit_code != 0:
                return InstanceResult(
                    instance_id=inst_id,
                    resolved=False,
                    detail=f"gold patch apply failed (exit={gold_apply.exit_code}): "
                    f"{gold_apply.stderr[:500]}",
                    sandbox_id=sb.id,
                    edge_id=sb.edge_id,
                )

            # Drop eval_script into the sandbox and run it. eval_script handles
            # conda activation, test_patch application, and the test command.
            # It uses `set -uxo pipefail` (no -e), so its exit code does not
            # reflect test pass/fail — we have to parse the log.
            await sb.write_file("/eval.sh", test_spec.eval_script.encode())
            await sb.exec("chmod +x /eval.sh")
            eval_run = await sb.exec("/eval.sh", timeout=1500.0)
            log_content = eval_run.stdout + eval_run.stderr

            # Persist log locally so get_eval_report can read it from a path.
            log_path = log_dir / f"{inst_id}.log"
            log_path.write_text(log_content)

            # SWE-bench grading.
            report = get_eval_report(
                test_spec=test_spec,
                prediction={
                    "instance_id": inst_id,
                    "model_patch": gold_patch,
                },
                test_log_path=str(log_path),
                include_tests_status=True,
            )
            ir = report.get(inst_id, {})
            resolved = bool(ir.get("resolved", False))
            ts = ir.get("tests_status", {}) or {}
            f2p = ts.get("FAIL_TO_PASS", {}) or {}
            p2p = ts.get("PASS_TO_PASS", {}) or {}
            if resolved:
                detail = (
                    f"FAIL_TO_PASS {len(f2p.get('success', []))} pass / "
                    f"{len(f2p.get('failure', []))} fail; "
                    f"PASS_TO_PASS {len(p2p.get('success', []))} pass / "
                    f"{len(p2p.get('failure', []))} fail"
                )
            else:
                f2p_fail = f2p.get("failure", [])[:3]
                p2p_fail = p2p.get("failure", [])[:3]
                detail = (
                    f"not resolved (eval exit={eval_run.exit_code}); "
                    f"F2P fails={f2p_fail}; P2P fails={p2p_fail}"
                )
            return InstanceResult(
                instance_id=inst_id,
                resolved=resolved,
                detail=detail,
                sandbox_id=sb.id,
                edge_id=sb.edge_id,
                eval_log_path=log_path,
            )
        except Exception as e:
            return InstanceResult(
                instance_id=inst_id,
                resolved=False,
                detail=f"runner error: {e!r}",
                sandbox_id=sb.id,
                edge_id=sb.edge_id,
            )


def load_instances(instance_ids: list[str]) -> list[dict[str, Any]]:
    from datasets import load_dataset

    ds = load_dataset("princeton-nlp/SWE-bench_Verified", split="test")
    by_id = {
        row["instance_id"]: dict(row)
        for row in ds
        if row["instance_id"] in set(instance_ids)
    }
    missing = [i for i in instance_ids if i not in by_id]
    if missing:
        raise SystemExit(f"instance(s) not found in SWE-bench Verified: {missing}")
    return [by_id[i] for i in instance_ids]


async def main_async(instance_ids: list[str], gold_only: bool, log_dir: Path) -> int:
    if not gold_only:
        raise SystemExit("only --gold mode is currently supported (the agent IS the gold patch)")

    if not os.environ.get("ARLEE_APISERVER") or not os.environ.get("ARLEE_TOKEN"):
        raise SystemExit("set ARLEE_APISERVER and ARLEE_TOKEN")

    log_dir.mkdir(parents=True, exist_ok=True)

    # Bump the default client timeout for SWE-bench's slow image pulls + long evals.
    arlee.configure(timeout=CLIENT_TIMEOUT_SECONDS)
    instances = load_instances(instance_ids)
    results = await asyncio.gather(*(run_instance(i, log_dir) for i in instances))

    n_resolved = sum(1 for r in results if r.resolved)
    print()
    for r in results:
        mark = "PASS" if r.resolved else "FAIL"
        print(f"[{mark}] {r.instance_id} — {r.detail}")
        if r.sandbox_id:
            print(
                f"        sandbox_id={r.sandbox_id}  edge={r.edge_id}  "
                f"eval_log={r.eval_log_path}"
            )
    edges_used = sorted({r.edge_id for r in results if r.edge_id})
    print(f"\n=== {n_resolved}/{len(results)} RESOLVED  (edges used: {edges_used}) ===")
    return 0 if n_resolved == len(results) else 1


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument(
        "--instance-id",
        action="append",
        help="Specific instance(s) to run; repeatable. Defaults to 3 hardcoded easy ones.",
    )
    p.add_argument(
        "--gold",
        action="store_true",
        help="Use the instance's gold patch as the agent output (currently the only mode).",
    )
    p.add_argument(
        "--log-dir",
        type=Path,
        default=Path(tempfile.gettempdir()) / "arlee-swebench-logs",
        help="Where to write per-instance eval logs.",
    )
    args = p.parse_args()
    ids = args.instance_id or DEFAULT_INSTANCE_IDS
    return asyncio.run(main_async(ids, args.gold, args.log_dir))


if __name__ == "__main__":
    sys.exit(main())
