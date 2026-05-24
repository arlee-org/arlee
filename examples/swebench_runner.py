"""Drive 3 SWE-bench Verified gold patches end-to-end through Arlee.

Acceptance criterion for Arlee v0: all 3 tasks should produce a `PASS` —
the SWE-bench grader sees every test in FAIL_TO_PASS go green and every test
in PASS_TO_PASS still green.

Run from the Apiserver VM (where the Python SDK + swebench package are
installed), with ARLEE_APISERVER and ARLEE_TOKEN exported:

    python examples/swebench_runner.py --gold

By default it runs 3 hardcoded instance IDs concurrently so the Apiserver
spreads them across the available Edges. Each instance pulls a ~1-3 GB
docker image on first use, so first run is slow.
"""

from __future__ import annotations

import argparse
import asyncio
import os
import re
import sys
from dataclasses import dataclass
from typing import Any

import arlee  # noqa: F401  — for side-effect imports / version
from arlee import Client

# Three SWE-bench Verified instance IDs chosen for smallest combined test count.
# Picked from `princeton-nlp/SWE-bench_Verified`; all have small patches and
# small repos by SWE-bench standards.
DEFAULT_INSTANCE_IDS = [
    "sympy__sympy-14711",                      # F2P=15, P2P=47
    "django__django-12419",                    # F2P=85, P2P=2
    "scikit-learn__scikit-learn-14141",        # F2P=65, P2P=139
]

# Long client timeout: SWE-bench images are 1-3 GB so first-time pull alone can
# take 1-5 min, and the eval pytest run can also be multi-minute.
CLIENT_TIMEOUT_SECONDS = 1200.0


@dataclass
class InstanceResult:
    instance_id: str
    passed: bool
    detail: str
    sandbox_id: str | None = None
    exit_code: int | None = None


async def _exec_or_fail(
    client: Client, sandbox_id: str, command: str, label: str, timeout: float | None = None
) -> str:
    """Exec a command; return stdout on success, raise RuntimeError otherwise."""
    r = await client.exec(sandbox_id, command, timeout=timeout)
    if r.exit_code != 0:
        raise RuntimeError(
            f"{label} failed (exit={r.exit_code})\nstdout:\n{r.stdout}\nstderr:\n{r.stderr}"
        )
    return r.stdout


def _parse_test_log(log: str, fail_to_pass: list[str], pass_to_pass: list[str]) -> tuple[bool, str]:
    """Return (passed, detail). passed = every FAIL_TO_PASS passes AND every PASS_TO_PASS still passes."""
    # The SWE-bench eval log format is pytest-y: lines like "PASSED tests/x.py::test_y"
    # or "FAILED tests/x.py::test_y - ...".
    statuses: dict[str, str] = {}
    for line in log.splitlines():
        m = re.match(r"^(PASSED|FAILED|ERROR|SKIPPED)\s+(\S+)", line.strip())
        if m:
            statuses[m.group(2)] = m.group(1)

    missing_f2p = [t for t in fail_to_pass if t not in statuses]
    failed_f2p = [t for t in fail_to_pass if statuses.get(t) != "PASSED"]
    failed_p2p = [t for t in pass_to_pass if statuses.get(t) not in (None, "PASSED")]

    if missing_f2p:
        return False, f"missing FAIL_TO_PASS results: {missing_f2p[:5]}"
    if failed_f2p:
        return False, f"FAIL_TO_PASS still failing: {failed_f2p[:5]}"
    if failed_p2p:
        return False, f"PASS_TO_PASS regressed: {failed_p2p[:5]}"
    return True, f"all {len(fail_to_pass)} FAIL_TO_PASS + {len(pass_to_pass)} PASS_TO_PASS green"


async def run_instance(client: Client, instance: dict[str, Any]) -> InstanceResult:
    from swebench.harness.test_spec.test_spec import make_test_spec

    inst_id = instance["instance_id"]
    image = f"swebench/sweb.eval.x86_64.{inst_id}:latest"
    test_spec = make_test_spec(instance)

    sb = await client.create_sandbox(image=image, timeout=600.0)
    try:
        # Apply the test_patch first (adds the new tests SWE-bench checks against).
        await client.write_file(sb.id, "/tmp/test.patch", test_spec.test_patch.encode())
        await _exec_or_fail(
            client,
            sb.id,
            "cd /testbed && git apply --allow-empty /tmp/test.patch",
            "apply test_patch",
            timeout=60,
        )

        # Apply the gold patch (the "model output" we're evaluating).
        gold = instance["patch"]
        await client.write_file(sb.id, "/tmp/gold.patch", gold.encode())
        await _exec_or_fail(
            client,
            sb.id,
            "cd /testbed && git apply --allow-empty /tmp/gold.patch",
            "apply gold patch",
            timeout=60,
        )

        # Drop the per-instance eval script and run it. SWE-bench's eval_script
        # already activates the conda env, sets cwd, and invokes pytest.
        await client.write_file(sb.id, "/eval.sh", test_spec.eval_script.encode())
        await client.exec(sb.id, "chmod +x /eval.sh")
        r = await client.exec(sb.id, "/eval.sh", timeout=1200.0)

        passed, detail = _parse_test_log(
            r.stdout + "\n" + r.stderr,
            instance["FAIL_TO_PASS"],
            instance["PASS_TO_PASS"],
        )
        return InstanceResult(
            instance_id=inst_id,
            passed=passed,
            detail=detail,
            sandbox_id=sb.id,
            exit_code=r.exit_code,
        )
    except Exception as e:
        return InstanceResult(
            instance_id=inst_id,
            passed=False,
            detail=f"runner error: {e}",
            sandbox_id=sb.id if "sb" in locals() else None,
        )
    finally:
        try:
            await client.kill_sandbox(sb.id)
        except Exception:
            pass


def load_instances(instance_ids: list[str]) -> list[dict[str, Any]]:
    """Load instance metadata from HuggingFace SWE-bench Verified."""
    from datasets import load_dataset

    ds = load_dataset("princeton-nlp/SWE-bench_Verified", split="test")
    by_id = {row["instance_id"]: dict(row) for row in ds if row["instance_id"] in set(instance_ids)}
    missing = [i for i in instance_ids if i not in by_id]
    if missing:
        raise SystemExit(f"instance(s) not found in SWE-bench Verified: {missing}")
    return [by_id[i] for i in instance_ids]


async def main_async(instance_ids: list[str], gold_only: bool) -> int:
    if not gold_only:
        raise SystemExit("v0 only supports --gold mode (the agent IS the gold patch)")

    instances = load_instances(instance_ids)
    apiserver = os.environ.get("ARLEE_APISERVER")
    token = os.environ.get("ARLEE_TOKEN")
    if not apiserver or not token:
        raise SystemExit("set ARLEE_APISERVER and ARLEE_TOKEN")

    async with Client(apiserver=apiserver, token=token, timeout=CLIENT_TIMEOUT_SECONDS) as client:
        results = await asyncio.gather(*(run_instance(client, i) for i in instances))

    n_passed = sum(1 for r in results if r.passed)
    for r in results:
        mark = "PASS" if r.passed else "FAIL"
        print(f"[{mark}] {r.instance_id} — {r.detail}")
        if r.sandbox_id:
            print(f"        sandbox_id={r.sandbox_id}  exit_code={r.exit_code}")
    print(f"\n=== {n_passed}/{len(results)} PASS ===")
    return 0 if n_passed == len(results) else 1


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
    args = p.parse_args()
    ids = args.instance_id or DEFAULT_INSTANCE_IDS
    return asyncio.run(main_async(ids, args.gold))


if __name__ == "__main__":
    sys.exit(main())
