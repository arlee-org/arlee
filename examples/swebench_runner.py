"""Drive 3 SWE-bench Verified gold patches end-to-end through Arlee.

Acceptance criterion for Arlee v0: all 3 tasks should produce `resolved=True`
as reported by `swebench.harness.grading.get_eval_report` — i.e. every test
in FAIL_TO_PASS goes green and every test in PASS_TO_PASS stays green.

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
import json
import os
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import arlee  # noqa: F401  — for version
from arlee import Client

# Three SWE-bench Verified instance IDs chosen for smallest combined test count.
DEFAULT_INSTANCE_IDS = [
    "sympy__sympy-14711",                      # F2P=15, P2P=47
    "django__django-12419",                    # F2P=85, P2P=2
    "scikit-learn__scikit-learn-14141",        # F2P=65, P2P=139
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
    eval_log_path: Path | None = None


async def run_instance(
    client: Client, instance: dict[str, Any], log_dir: Path
) -> InstanceResult:
    from swebench.harness.grading import get_eval_report
    from swebench.harness.test_spec.test_spec import make_test_spec

    inst_id = instance["instance_id"]
    image = docker_hub_image(inst_id)
    test_spec = make_test_spec(instance)
    gold_patch = instance["patch"]

    sb = await client.create_sandbox(image=image, timeout=1800.0)
    try:
        # Apply the gold patch (the "model output" we're evaluating).
        # eval_script does NOT apply the model patch; it only applies test_patch.
        await client.write_file(sb.id, "/tmp/gold.patch", gold_patch.encode())
        gold_apply = await client.exec(
            sb.id,
            "cd /testbed && git apply /tmp/gold.patch",
            timeout=120,
        )
        if gold_apply.exit_code != 0:
            return InstanceResult(
                instance_id=inst_id,
                resolved=False,
                detail=f"gold patch apply failed (exit={gold_apply.exit_code}): "
                f"{gold_apply.stderr[:500]}",
                sandbox_id=sb.id,
            )

        # Write eval_script and run it. eval_script handles conda activation,
        # test_patch application, and runs the test command. Per swebench, it
        # uses `set -uxo pipefail` (no -e), so its exit code does not reflect
        # test pass/fail — we have to parse the log.
        await client.write_file(sb.id, "/eval.sh", test_spec.eval_script.encode())
        await client.exec(sb.id, "chmod +x /eval.sh")
        eval_run = await client.exec(sb.id, "/eval.sh", timeout=1500.0)
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
        # report is keyed by instance_id.
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
            eval_log_path=log_path,
        )
    except Exception as e:
        return InstanceResult(
            instance_id=inst_id,
            resolved=False,
            detail=f"runner error: {e!r}",
            sandbox_id=sb.id if "sb" in locals() else None,
        )
    finally:
        try:
            await client.kill_sandbox(sb.id)
        except Exception:
            pass


def load_instances(instance_ids: list[str]) -> list[dict[str, Any]]:
    from datasets import load_dataset

    ds = load_dataset("princeton-nlp/SWE-bench_Verified", split="test")
    by_id = {row["instance_id"]: dict(row) for row in ds if row["instance_id"] in set(instance_ids)}
    missing = [i for i in instance_ids if i not in by_id]
    if missing:
        raise SystemExit(f"instance(s) not found in SWE-bench Verified: {missing}")
    return [by_id[i] for i in instance_ids]


async def main_async(instance_ids: list[str], gold_only: bool, log_dir: Path) -> int:
    if not gold_only:
        raise SystemExit("v0 only supports --gold mode (the agent IS the gold patch)")

    instances = load_instances(instance_ids)
    apiserver = os.environ.get("ARLEE_APISERVER")
    token = os.environ.get("ARLEE_TOKEN")
    if not apiserver or not token:
        raise SystemExit("set ARLEE_APISERVER and ARLEE_TOKEN")

    log_dir.mkdir(parents=True, exist_ok=True)

    async with Client(apiserver=apiserver, token=token, timeout=CLIENT_TIMEOUT_SECONDS) as client:
        results = await asyncio.gather(*(run_instance(client, i, log_dir) for i in instances))

    n_resolved = sum(1 for r in results if r.resolved)
    print()
    for r in results:
        mark = "PASS" if r.resolved else "FAIL"
        print(f"[{mark}] {r.instance_id} — {r.detail}")
        if r.sandbox_id:
            print(f"        sandbox_id={r.sandbox_id}  eval_log={r.eval_log_path}")
    print(f"\n=== {n_resolved}/{len(results)} RESOLVED ===")
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
        help="Use the instance's gold patch as the agent output (v0 mode).",
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
