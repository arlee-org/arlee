"""Shared pytest fixtures.

Integration tests against a live Arlee cluster are tagged with the
`gcp` marker (registered in pyproject.toml). They auto-skip when
`ARLEE_APISERVER` is unset — a developer without a live cluster sees
no failure, just skipped tests. To exercise them: `terraform apply`,
`eval "$(terraform output -raw env_setup)"`, then `pytest -m gcp`.

See docs/memory-limits.md §5 for the testing policy this
implements.
"""

from __future__ import annotations

import os

import pytest


def pytest_collection_modifyitems(config, items):
    """Auto-skip @pytest.mark.gcp items when no cluster is reachable."""
    if os.environ.get("ARLEE_APISERVER") and os.environ.get("ARLEE_TOKEN"):
        return
    skip_no_cluster = pytest.mark.skip(
        reason="ARLEE_APISERVER + ARLEE_TOKEN not set; "
        "skipping live-cluster tests (set via "
        '`eval "$(cd deploy/terraform/gcp && terraform output -raw env_setup)"`)'
    )
    for item in items:
        if "gcp" in item.keywords:
            item.add_marker(skip_no_cluster)
