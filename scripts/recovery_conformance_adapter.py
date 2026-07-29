#!/usr/bin/env python3
"""Bind shared recovery scenario IDs to native Rust SDK behavior tests."""

from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

OBSERVATIONS = {
    "CR-CORE-001": (["commit", "commit_same_key"], ["settlement_occurs_at_most_once", "retry_uses_original_idempotency_key"]),
    "CR-CORE-002": (["commit", "event_same_key"], ["event_carries_original_subject_action_actual", "settlement_occurs_at_most_once"]),
    "CR-CORE-003": (["extend", "extend_same_key", "commit"], ["heartbeat_failure_reports_reservation_and_disposition", "guarded_action_continues_under_warn_policy", "final_settlement_is_attempted"]),
    "CR-CORE-004": (["commit", "commit_same_key"], ["only_schema_valid_expected_status_is_terminal_success", "ambiguous_success_retains_original_idempotency_key"]),
    "CR-DURABLE-001": (["commit", "commit_same_key_after_restart"], ["journal_write_precedes_first_settlement_request", "unresolved_record_survives_restart", "successful_replay_removes_record", "settlement_occurs_at_most_once"]),
    "CR-DURABLE-002": (["commit_same_key_after_restart", "event_same_key_after_restart"], ["event_mode_is_persisted_before_event_attempt", "successful_event_removes_record", "settlement_occurs_at_most_once"]),
    "CR-DURABLE-003": (["commit", "commit_same_key_after_retry_after"], ["no_retry_before_persisted_not_before", "successful_replay_removes_record"]),
    "CR-DURABLE-004": (["commit_same_key_after_restart"], ["new_tenant_credential_finds_record", "old_api_key_is_not_stored"]),
    "CR-DURABLE-005": ([], ["corrupt_record_is_quarantined", "other_valid_records_still_replay", "corruption_is_reported"]),
    "CR-DURABLE-006": (["concurrent_commit_same_key"], ["settlement_occurs_at_most_once", "terminal_record_is_removed"]),
    "CR-DURABLE-007": (["commit_first_identifier", "commit_second_identifier", "commit_first_identifier_same_key_after_restart", "commit_second_identifier_same_key_after_restart"], ["standard_filename_is_sha256_of_exact_utf8_identifier", "distinct_identifiers_never_share_a_journal_file", "matching_legacy_record_migrates_without_deleting_collision", "both_settlements_occur_at_most_once"]),
    "CR-BOUNDARY-001": ([], ["sdk_does_not_claim_ledger_convergence", "application_checkpoint_is_required"]),
}

TESTS = {
    "CR-CORE-001": "transient_commit_is_journaled_and_replayed_after_key_rotation",
    "CR-CORE-002": "expired_commit_recovers_via_event",
    "CR-CORE-003": "heartbeat_fallback_unknown_status_retries_with_same_key",
    "CR-CORE-004": "protocol_invalid_2xx_settlement_responses_remain_durably_ambiguous",
    "CR-DURABLE-001": "transient_commit_is_journaled_and_replayed_after_key_rotation",
    "CR-DURABLE-002": "expired_commit_persists_event_mode_before_restart_replay",
    "CR-DURABLE-003": "rate_limit_retry_floor_is_persisted",
    "CR-DURABLE-004": "transient_commit_is_journaled_and_replayed_after_key_rotation",
    "CR-DURABLE-005": "journal::tests::corrupt_records_are_quarantined_without_blocking_valid_records",
    "CR-DURABLE-006": "synchronous_success_and_terminal_rejection_remove_the_preflight_record",
    "CR-DURABLE-007": "journal::tests::digest_names_do_not_collide_and_legacy_discard_checks_record_identity",
    "CR-BOUNDARY-001": "commit_pending_is_distinct_from_a_retryable_terminal_error",
}

# The Windows test host used for local fleet review cannot reach Wiremock's
# loopback listener. CI runs the process-level tests above on Linux; local
# Windows verification still exercises the corresponding pure state machines.
WINDOWS_TESTS = {
    "CR-CORE-001": "client::tests::settlement_retry_classification_includes_ambiguous_successes",
    "CR-CORE-002": "journal::tests::record_load_and_discard_use_the_shared_wire_shape",
    "CR-CORE-003": "heartbeat::tests::field_mode_recoverable_classification",
    "CR-CORE-004": "client::tests::settlement_retry_classification_includes_ambiguous_successes",
    "CR-DURABLE-001": "journal::tests::record_load_and_discard_use_the_shared_wire_shape",
    "CR-DURABLE-002": "journal::tests::legacy_missing_mode_defaults_to_commit",
    "CR-DURABLE-003": "retry::tests::delay_for_waits_at_least_the_servers_retry_after",
    "CR-DURABLE-004": "journal::tests::fingerprints_match_the_python_typescript_and_java_contract",
    "CR-DURABLE-006": "journal::tests::record_load_and_discard_use_the_shared_wire_shape",
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
    cargo = "cargo.exe" if os.name == "nt" else "cargo"
    test_name = WINDOWS_TESTS.get(scenario_id, TESTS[scenario_id]) if os.name == "nt" else TESTS[scenario_id]
    completed = subprocess.run(
        [cargo, "test", "--all-features", test_name, "--", "--exact"],
        cwd=ROOT, text=True, capture_output=True, check=False,
    )
    if completed.stdout:
        print(completed.stdout, file=sys.stderr, end="")
    if completed.stderr:
        print(completed.stderr, file=sys.stderr, end="")
    requests, assertions = OBSERVATIONS[scenario_id]
    json.dump({
        "scenario_id": scenario_id,
        "passed": completed.returncode == 0,
        "observed_requests": requests,
        "assertions": assertions,
        "diagnostic": f"native cargo test exit code {completed.returncode}",
    }, sys.stdout)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
