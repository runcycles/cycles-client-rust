#!/usr/bin/env python3
"""Bind shared recovery scenario IDs to native Rust SDK behavior tests."""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

TESTS = {
    "CR-CORE-001": ["transient_commit_is_journaled_and_replayed_after_key_rotation"],
    "CR-CORE-002": ["expired_commit_recovers_via_event"],
    "CR-CORE-003": [
        "heartbeat_transport_timeout_is_observable_nonfatal_and_same_key",
        "heartbeat::tests::fallback_retry_warning_includes_reservation_and_disposition",
    ],
    "CR-CORE-004": ["protocol_invalid_2xx_settlement_responses_remain_durably_ambiguous"],
    "CR-DURABLE-001": ["transient_commit_is_journaled_and_replayed_after_key_rotation"],
    "CR-DURABLE-002": ["expired_commit_persists_event_mode_before_restart_replay"],
    "CR-DURABLE-003": ["rate_limit_retry_floor_is_persisted"],
    "CR-DURABLE-004": ["transient_commit_is_journaled_and_replayed_after_key_rotation"],
    "CR-DURABLE-005": [
        "journal::tests::corrupt_and_unsupported_records_are_quarantined_without_blocking_valid_records"
    ],
    "CR-DURABLE-006": ["concurrent_replay_workers_reuse_one_key_and_remove_record"],
    "CR-DURABLE-007": [
        "journal::tests::digest_names_do_not_collide_and_legacy_discard_checks_record_identity"
    ],
    "CR-BOUNDARY-001": ["with_cycles_error_releases_automatically"],
}

def main() -> int:
    if len(sys.argv) != 2:
        print("expected one scenario ID", file=sys.stderr)
        return 2
    scenario = json.load(sys.stdin)
    scenario_id = sys.argv[1]
    if scenario.get("id") != scenario_id or scenario_id not in TESTS:
        print("unknown or mismatched scenario ID", file=sys.stderr)
        return 2
    if "expected_requests" in scenario or "assertions" in scenario:
        print("runner disclosed conformance oracle", file=sys.stderr)
        return 2
    cargo = "cargo"
    executed = []
    passed = True
    last_code = 0
    for test_name in TESTS[scenario_id]:
        completed = subprocess.run(
            [cargo, "test", "--all-features", test_name, "--", "--exact"],
            cwd=ROOT, text=True, capture_output=True, check=False,
        )
        executed.append(test_name)
        last_code = completed.returncode
        if completed.stdout:
            print(completed.stdout, file=sys.stderr, end="")
        if completed.stderr:
            print(completed.stderr, file=sys.stderr, end="")
        if completed.returncode != 0:
            passed = False
            break
    json.dump({
        "scenario_id": scenario_id,
        "passed": passed,
        "native_tests": executed,
        "diagnostic": f"native cargo test exit code {last_code}",
    }, sys.stdout)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
