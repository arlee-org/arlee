"""End-to-end memory-limits tests against a live Arlee cluster.

Implements docs/memory-limits.md §5 items 4–7. Auto-skips when
ARLEE_APISERVER / ARLEE_TOKEN are unset (see ../conftest.py). To run:

    cd deploy/terraform/gcp && terraform apply && \\
      eval "$(terraform output -raw env_setup)"
    cd python && pytest -m gcp tests/integration/test_memory_limits.py -v

Required Edge image: tests pull `polinux/stress` (a small image with the
`stress` utility) for the memory-pressure scenarios.

Assumptions about Edge hardware: tests are sized for the default
e2-standard-4 (16 GiB RAM, ~15.5 GiB after the system reserve subtraction
in EdgeCgroup::DEFAULT_SYSTEM_RESERVE_MB). If you change the Edge VM type,
the per-test memory numbers may need adjustment.
"""

from __future__ import annotations

import asyncio

import pytest_asyncio
import pytest

import arlee

pytestmark = pytest.mark.gcp

STRESS_IMAGE = "polinux/stress"


@pytest_asyncio.fixture
async def client():
    """Per-test Client with own connection pool. Module-level
    `arlee.create_sandbox` shares a default client across tests, which fights
    with pytest-asyncio's per-test event loops ("Event loop is closed" during
    httpx cleanup). Explicit per-test Client sidesteps this entirely.
    """
    async with arlee.Client.from_env() as c:
        yield c


# ---------------------------------------------------------------------------
# Item 4: own-max OOM, both on_oom modes
# ---------------------------------------------------------------------------


async def test_own_max_oom_kill_process_keeps_sandbox_alive(client):
    """Sandbox with memory_max_mb=512 + stress 1G:
    - exec returns terminated_by=oom (own-max breach)
    - stderr appended with arlee marker
    - follow-up exec succeeds (kill_process default → sandbox survives)
    """
    async with await client.create_sandbox(
        image=STRESS_IMAGE, memory_max_mb=512
    ) as sb:
        r = await sb.exec(
            "stress --vm 1 --vm-bytes 1G --vm-keep --timeout 5",
            timeout=30,
        )
        assert r.terminated_by == arlee.ExecTermination.OOM, (
            f"expected ExecTermination.OOM (own-max), got {r.terminated_by!r}; "
            f"stderr tail: {r.stderr[-300:]!r}"
        )
        assert r.exit_code != 0
        assert "OOM-killed" in r.stderr
        assert "memory_max_mb=512" in r.stderr, (
            f"stderr marker should cite the breached ceiling; got {r.stderr[-200:]!r}"
        )
        # The point of kill_process: sandbox PID 1 survived.
        r2 = await sb.exec("echo still_alive")
        assert r2.exit_code == 0
        assert "still_alive" in r2.stdout
        assert r2.terminated_by is None


async def test_own_max_oom_kill_sandbox_transitions_to_failed(client):
    """Same workload with on_oom=kill_sandbox: the exec reports OOM, the
    sandbox container is gone, follow-up operations fail.

    Validates that conditional oom_score_adj is correct: under kill_sandbox
    we deliberately leave PID 1 killable so memory.oom.group=1 takes the
    whole cgroup down. See docs/memory-limits.md §3.3 caveat 2.
    """
    sb = await client.create_sandbox(
        image=STRESS_IMAGE,
        memory_max_mb=512,
        on_oom=arlee.OnOom.KILL_SANDBOX,
    )
    try:
        r = await sb.exec(
            "stress --vm 1 --vm-bytes 1G --vm-keep --timeout 5",
            timeout=30,
        )
        assert r.terminated_by == arlee.ExecTermination.OOM
        # Sandbox must be unreachable now — follow-up exec errors out.
        with pytest.raises(Exception) as exc_info:
            await asyncio.wait_for(sb.exec("echo should_not_run"), timeout=15)
        # We don't assert a specific exception class: depending on timing the
        # apiserver may return 502 (bad gateway, container gone) or the SDK
        # may surface a different error. What matters is that it errors.
        assert exc_info.value is not None
    finally:
        # Best-effort cleanup — kill() against a dead sandbox is a no-op.
        try:
            await sb.kill()
        except Exception:
            pass


# ---------------------------------------------------------------------------
# Item 5: hard reservation — sandbox B's memory.min is honored even when
# sandbox A is pressuring the Edge.
#
# Per §8 item 5: "Asserts that memory.min is doing real work" — the
# Anthropic-quoted property that distinguishes our hard reservation from
# Docker's default --memory-reservation (which is soft, cgroup memory.low).
# ---------------------------------------------------------------------------


async def test_hard_reservation_protects_min_under_pressure(client):
    """Two sandboxes on the same Edge: A consuming near its max, B requesting
    its memory_min_mb worth of allocations. Hard memory.min on B means its
    reservation isn't reclaimed to satisfy A, so B's allocation completes.

    Tries to land both on the same Edge by creating sandboxes back-to-back;
    if scheduler spreads them we still validate end-to-end behavior but the
    test becomes weaker (no pressure between them). The assertion is on B's
    success — A's behavior is incidental.
    """
    # A: large min + max, will be pressured. B: smaller min, modest allocation.
    a = await client.create_sandbox(
        image=STRESS_IMAGE, memory_min_mb=4096, memory_max_mb=12288
    )
    b = await client.create_sandbox(
        image=STRESS_IMAGE, memory_min_mb=2048, memory_max_mb=6144
    )
    try:
        # Push A close to its max; this puts pressure on the Edge.
        a_task = asyncio.create_task(
            a.exec(
                "stress --vm 1 --vm-bytes 10G --vm-keep --timeout 20",
                timeout=60,
            )
        )
        # Give A a beat to ramp up.
        await asyncio.sleep(2)
        # B allocates within its memory_min_mb (2 GiB) — hard reservation must
        # let this complete even though A is using ~10 GiB on the same host.
        r_b = await b.exec(
            "stress --vm 1 --vm-bytes 1500M --vm-keep --timeout 5",
            timeout=30,
        )
        # B should succeed because its 1.5 GiB allocation fits inside the
        # 2 GiB memory.min reservation that the kernel must protect.
        assert r_b.exit_code == 0 or r_b.terminated_by is None, (
            f"B's allocation within its memory.min ({b.info.resources.memory_min_mb} MiB) "
            f"was killed; hard reservation did not protect it. "
            f"exit={r_b.exit_code} terminated_by={r_b.terminated_by} "
            f"stderr tail: {r_b.stderr[-200:]!r}"
        )
        # Drain A; we don't assert anything about A — its termination is
        # incidental to this test.
        await a_task
    finally:
        for sb in (a, b):
            try:
                await sb.kill()
            except Exception:
                pass


# ---------------------------------------------------------------------------
# Item 6: 2-Edge scheduling distribution
# ---------------------------------------------------------------------------


async def test_two_edge_spread_distribution(client):
    """Two healthy Edges, four sandboxes with non-trivial min: the spread
    scheduler should distribute 2-2, not 4-0. Asserts §5.2 ratio scheduling
    actually spreads.
    """
    edges = await client.list_edges()
    healthy = [e for e in edges if e.healthy]
    if len(healthy) < 2:
        pytest.skip(
            f"need at least 2 healthy Edges for spread distribution test; got {len(healthy)}"
        )

    # Four sandboxes with 2 GiB min each — sum = 8 GiB, fits even on a single
    # Edge but spread should still distribute them.
    sandboxes = []
    try:
        for _ in range(4):
            sb = await client.create_sandbox(
                image=STRESS_IMAGE, memory_min_mb=2048, memory_max_mb=4096
            )
            sandboxes.append(sb)
        per_edge = {}
        for sb in sandboxes:
            per_edge[sb.edge_id] = per_edge.get(sb.edge_id, 0) + 1
        # With 2 Edges and ratio-based spread, expect 2-2 (each pick lands on
        # the emptier Edge, and after each placement that Edge becomes the
        # less-empty one, so the scheduler bounces between them).
        max_on_one = max(per_edge.values())
        assert max_on_one <= 2, (
            f"spread scheduler clumped {max_on_one} sandboxes on one Edge; "
            f"per-edge distribution: {per_edge}"
        )
    finally:
        for sb in sandboxes:
            try:
                await sb.kill()
            except Exception:
                pass


# ---------------------------------------------------------------------------
# Item 7: Edge-pressure OOM (the judgment test for §5.4)
# ---------------------------------------------------------------------------


async def test_edge_pressure_classified_as_oom_edge(client):
    """The discriminator from §5.4: when system OOM killer picks a sandbox
    as victim under Edge memory pressure (oom_kill counter increments but
    sandbox's own memory.max counter does NOT), the result must report
    terminated_by=oom_edge — distinct from oom (own-max breach).

    Setup: 3 sandboxes, each min=2G max=12G. Concurrent stress 9G in each.
    With 2 Edges, scheduler spreads as 2-1; the Edge with 2 sandboxes ends
    up trying to use 18 GiB on a 15.5 GiB host, so the system OOM killer
    fires. The victim's usage (9 GiB) is well under its own 12 GiB max, so
    it must be classified as oom_edge.

    Inconclusive (not failed) if no Edge OOM fires — that just means the
    kernel managed reclaim before triggering OOM. We retry once with
    higher pressure before giving up.
    """

    async def run_round(per_sandbox_g: int) -> dict:
        sandboxes = []
        try:
            for _ in range(3):
                sb = await client.create_sandbox(
                    image=STRESS_IMAGE,
                    memory_min_mb=2048,
                    memory_max_mb=12288,
                )
                sandboxes.append(sb)
            cmd = f"stress --vm 1 --vm-bytes {per_sandbox_g}G --vm-keep --timeout 20"
            results = await asyncio.gather(
                *[sb.exec(cmd, timeout=60) for sb in sandboxes],
                return_exceptions=True,
            )
            return {
                "results": results,
                "sandboxes": sandboxes,
                "per_edge": {sb.edge_id: i for i, sb in enumerate(sandboxes)},
            }
        finally:
            for sb in sandboxes:
                try:
                    await sb.kill()
                except Exception:
                    pass

    edge_oom_total = 0
    own_oom_total = 0
    for pressure_g in (9, 11):
        round_data = await run_round(pressure_g)
        results = round_data["results"]
        for r in results:
            if isinstance(r, Exception):
                continue
            if r.terminated_by == arlee.ExecTermination.OOM_EDGE:
                edge_oom_total += 1
            elif r.terminated_by == arlee.ExecTermination.OOM:
                own_oom_total += 1
        if edge_oom_total >= 1:
            break

    # The judgment: at least one OomEdge proves the §5.4 discriminator works.
    # If we got plain Oom but no OomEdge, the discriminator is broken (or
    # the kernel did something we didn't expect).
    assert edge_oom_total >= 1, (
        f"expected at least one ExecTermination.OOM_EDGE across rounds, got 0; "
        f"own_oom={own_oom_total}. If both are 0, kernel managed reclaim — "
        f"try increasing per-sandbox stress and re-run. If own_oom>0 and "
        f"oom_edge=0, the §5.4 discriminator may be broken — check that "
        f"memory.events.max isn't ticking on the victim."
    )


# ---------------------------------------------------------------------------
# Smoke: default sandbox (no memory fields) lifecycle still works
# ---------------------------------------------------------------------------


async def test_default_create_no_memory_fields(client):
    """Backward-compat smoke: create with no memory args, exec, kill.
    Validates that the SDK + apiserver + edge path doesn't regress for
    callers that haven't adopted memory_min_mb / memory_max_mb yet.
    """
    async with await client.create_sandbox(image="ubuntu:22.04") as sb:
        assert sb.info.resources.memory_min_mb is None
        assert sb.info.resources.memory_max_mb is None
        assert sb.info.on_oom == arlee.OnOom.KILL_PROCESS
        r = await sb.exec("echo hello-world")
        assert r.exit_code == 0
        assert "hello-world" in r.stdout
        assert r.terminated_by is None


# ---------------------------------------------------------------------------
# Apiserver-level: validation + capacity errors surface correctly
# ---------------------------------------------------------------------------


async def test_no_capacity_503_when_min_exceeds_any_edge(client):
    """memory_min_mb larger than any Edge's available memory must produce
    503 NoCapacity, not silent placement. Validates §5.2 admission and that
    the NoCapacity variant is wired through error.rs.
    """
    import httpx

    edges = await client.list_edges()
    max_total = max((e.total_memory_mb for e in edges), default=0)
    if max_total == 0:
        pytest.skip("no Edges report total_memory_mb; can't construct over-capacity request")
    # Request 8 GiB above the largest Edge — guaranteed infeasible.
    over_capacity = max_total + 8192
    with pytest.raises(httpx.HTTPStatusError) as exc_info:
        await client.create_sandbox(
            image="ubuntu:22.04", memory_min_mb=over_capacity, memory_max_mb=over_capacity
        )
    assert exc_info.value.response.status_code == 503
    body = exc_info.value.response.json()
    assert "no edge has capacity" in body.get("detail", "").lower(), (
        f"503 message should cite NoCapacity; got {body!r}"
    )


async def test_min_greater_than_max_rejected(client):
    """Capability validation in api.rs::validate_create: min > max → 400."""
    import httpx

    with pytest.raises(httpx.HTTPStatusError) as exc_info:
        await client.create_sandbox(
            image="ubuntu:22.04", memory_min_mb=2048, memory_max_mb=1024
        )
    assert exc_info.value.response.status_code == 400
    body = exc_info.value.response.json()
    assert "memory_min_mb" in body.get("detail", "") and "memory_max_mb" in body.get(
        "detail", ""
    ), f"400 message should name the violated constraint; got {body!r}"


async def test_reserved_memory_visible_in_edge_info(client):
    """After creating a sandbox with memory_min_mb, the chosen Edge's
    reserved_memory_mb is at least our reservation. (Absolute bound, not
    a delta — earlier tests in the suite may have left their own reservations
    in heartbeat-flight, so a before/after delta is noisy.)
    """
    reservation_mb = 1024
    async with await client.create_sandbox(
        image="ubuntu:22.04", memory_min_mb=reservation_mb, memory_max_mb=2048
    ) as sb:
        chosen = sb.edge_id
        edges = {e.id: e for e in await client.list_edges()}
        assert edges[chosen].reserved_memory_mb >= reservation_mb, (
            f"Edge {chosen} should have reserved at least {reservation_mb} MiB "
            f"while our sandbox is alive; got reserved={edges[chosen].reserved_memory_mb}"
        )
