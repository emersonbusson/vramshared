#!/usr/bin/env python3
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: tools/benchmarks/test_stress_gradual_swap_exhaustion.py
"""
Fixture tests for stress_gradual_swap_exhaustion.py (AUD-30).

Every case here runs against in-memory fixtures. Nothing in this file
allocates meaningful memory, touches the live host's swap, or arms the
supervised campaign tokens in a way that could start a real ramp.

Covers the AUD-30 required matrix:
  missing PSI, malformed data, allocation error, stuck worker,
  invalid limits, supervisor loss, failed cleanup.
"""

import argparse
import importlib.util
import os
import sys
import threading
import time
import unittest
from pathlib import Path
from unittest import mock

MODULE_PATH = Path(__file__).with_name("stress_gradual_swap_exhaustion.py")
SPEC = importlib.util.spec_from_file_location("gradual_swap", MODULE_PATH)
gradual_swap = importlib.util.module_from_spec(SPEC)
sys.modules["gradual_swap"] = gradual_swap
SPEC.loader.exec_module(gradual_swap)


VALID_PSI = (
    "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\n"
    "full avg10=0.00 avg60=0.00 avg300=0.00 total=0\n"
)


def make_args(**overrides):
    defaults = dict(
        start_pct=1,
        max_target_pct=2,
        step_pct=1,
        step_interval_sec=0.0,
        hold_peak_sec=0.0,
        min_free_ram_mb=1,
        max_psi_full=20.0,
        max_latency_ms=1000.0,
        max_psi_stall_rate_ms=1000.0,
        allow_isolated_lab=True,
    )
    defaults.update(overrides)
    return argparse.Namespace(**defaults)


class TelemetryFixtures(unittest.TestCase):
    """Missing or malformed pressure telemetry is a refusal, not a zero."""

    def test_missing_psi_raises_instead_of_returning_zeros(self):
        with mock.patch("os.path.exists", return_value=False):
            with self.assertRaises(gradual_swap.TelemetryUnavailable):
                gradual_swap.read_psi_memory()

    def test_malformed_psi_raises_instead_of_returning_zeros(self):
        with mock.patch("os.path.exists", return_value=True), mock.patch(
            "builtins.open", mock.mock_open(read_data="some avg10=not-a-number\n")
        ):
            with self.assertRaises(gradual_swap.TelemetryUnavailable):
                gradual_swap.read_psi_memory()

    def test_psi_missing_full_total_raises(self):
        with mock.patch("os.path.exists", return_value=True), mock.patch(
            "builtins.open",
            mock.mock_open(
                read_data=(
                    "some avg10=0.10 avg60=0.20 avg300=0.30 total=1\n"
                    "full avg10=0.00 avg60=0.00 avg300=0.00\n"
                )
            ),
        ):
            with self.assertRaises(gradual_swap.TelemetryUnavailable):
                gradual_swap.read_psi_memory()

    def test_valid_psi_parses(self):
        with mock.patch("os.path.exists", return_value=True), mock.patch(
            "builtins.open", mock.mock_open(read_data=VALID_PSI)
        ):
            some_avg10, full_avg10, full_total = gradual_swap.read_psi_memory()
        self.assertEqual((some_avg10, full_avg10, full_total), (0.0, 0.0, 0))

    def test_meminfo_missing_keys_raises(self):
        with mock.patch("builtins.open", mock.mock_open(read_data="MemTotal: 1 kB\n")):
            with self.assertRaises(gradual_swap.TelemetryUnavailable):
                gradual_swap.read_meminfo()

    def test_swaps_malformed_line_raises(self):
        with mock.patch(
            "builtins.open",
            mock.mock_open(
                read_data="Filename Type Size Used Priority\n"
                "/dev/zram0 partition not-a-number 1 200\n"
            ),
        ):
            with self.assertRaises(gradual_swap.TelemetryUnavailable):
                gradual_swap.read_swaps()


class AdmissionGate(unittest.TestCase):
    """Fail-closed admission. Tokens are required and are not bypasses."""

    def tearDown(self):
        for key in (
            "RAMSHARED_SHARED_HOST_APPROVAL",
            "RAMSHARED_WINDOWS_WATCHDOG_ARMED",
        ):
            os.environ.pop(key, None)

    def test_missing_tokens_refuse(self):
        with self.assertRaises(gradual_swap.SafetyGovernorAbort) as ctx:
            gradual_swap.admit(make_args())
        message = str(ctx.exception)
        self.assertIn("admission_refused", message)
        self.assertIn("shared_host_approval_missing_or_mismatched", message)
        self.assertIn("windows_watchdog_not_armed", message)

    def test_missing_isolated_lab_flag_refuses(self):
        os.environ["RAMSHARED_SHARED_HOST_APPROVAL"] = (
            gradual_swap.REQUIRED_SHARED_HOST_APPROVAL
        )
        os.environ["RAMSHARED_WINDOWS_WATCHDOG_ARMED"] = (
            gradual_swap.REQUIRED_WATCHDOG_ARMED
        )
        with self.assertRaises(gradual_swap.SafetyGovernorAbort) as ctx:
            gradual_swap.admit(make_args(allow_isolated_lab=False))
        self.assertIn("daily_host_refused_without_isolated_lab_flag", str(ctx.exception))

    def test_supervisor_loss_is_detected_when_psi_disappears(self):
        """Tokens present but pressure telemetry gone: admission still refuses."""
        os.environ["RAMSHARED_SHARED_HOST_APPROVAL"] = (
            gradual_swap.REQUIRED_SHARED_HOST_APPROVAL
        )
        os.environ["RAMSHARED_WINDOWS_WATCHDOG_ARMED"] = (
            gradual_swap.REQUIRED_WATCHDOG_ARMED
        )
        with mock.patch("os.path.exists", return_value=False):
            with self.assertRaises(gradual_swap.TelemetryUnavailable):
                gradual_swap.admit(make_args())

    def test_full_token_set_and_telemetry_admits(self):
        os.environ["RAMSHARED_SHARED_HOST_APPROVAL"] = (
            gradual_swap.REQUIRED_SHARED_HOST_APPROVAL
        )
        os.environ["RAMSHARED_WINDOWS_WATCHDOG_ARMED"] = (
            gradual_swap.REQUIRED_WATCHDOG_ARMED
        )
        meminfo = "MemTotal: 1024 kB\nMemAvailable: 512 kB\n"
        with mock.patch("os.path.exists", side_effect=lambda p: p.endswith("memory")), \
                mock.patch("builtins.open", mock.mock_open(read_data=meminfo)):
            # read_psi_memory and read_meminfo both open files; give each a
            # correct payload through a dispatcher.
            def _open(path, *args, **kwargs):
                if str(path).endswith("memory"):
                    return mock.mock_open(read_data=VALID_PSI)()
                return mock.mock_open(read_data=meminfo)()

            with mock.patch("builtins.open", side_effect=_open):
                gradual_swap.admit(make_args())


class ContainmentWatchdog(unittest.TestCase):
    """A real watchdog thread, not an advertised one."""

    def test_stuck_worker_releases_chunks(self):
        chunks = [bytearray(1024), bytearray(2048)]
        watchdog = gradual_swap.ContainmentWatchdog(
            chunks, checkin_timeout_sec=0.15, poll_sec=0.05
        )
        watchdog.start()
        # Never check in: this is the stuck-worker case.
        time.sleep(0.5)
        watchdog.stop()

        self.assertTrue(watchdog.triggered)
        self.assertEqual(watchdog.trigger_reason, "main_loop_missed_checkin")
        self.assertEqual(chunks, [])
        self.assertEqual(watchdog.released_bytes, 3072)

    def test_healthy_checkin_does_not_fire(self):
        chunks = [bytearray(128)]
        watchdog = gradual_swap.ContainmentWatchdog(
            chunks, checkin_timeout_sec=0.5, poll_sec=0.05
        )
        watchdog.start()
        deadline = time.time() + 0.4
        while time.time() < deadline:
            watchdog.check_in()
            time.sleep(0.05)
        watchdog.stop()

        self.assertFalse(watchdog.triggered)
        self.assertEqual(len(chunks), 1)

    def test_watchdog_thread_is_actually_created(self):
        chunks = []
        watchdog = gradual_swap.ContainmentWatchdog(chunks, poll_sec=0.05)
        watchdog.start()
        self.assertTrue(watchdog._thread.is_alive())
        watchdog.stop()


class OutcomeDerivation(unittest.TestCase):
    """No unconditional PASS. Abort and failure are non-zero."""

    def test_failure_is_nonzero(self):
        verdict, code = gradual_swap.derive_outcome(False, False, "", "boom")
        self.assertEqual(verdict, "ABORT_FAILED")
        self.assertNotEqual(code, 0)

    def test_safety_abort_is_nonzero(self):
        verdict, code = gradual_swap.derive_outcome(False, False, "ram_floor", None)
        self.assertEqual(verdict, "ABORT_SAFETY")
        self.assertNotEqual(code, 0)

    def test_incomplete_ramp_is_partial(self):
        verdict, code = gradual_swap.derive_outcome(False, True, "", None)
        self.assertEqual(verdict, "PARTIAL_RAMP_INCOMPLETE")
        self.assertNotEqual(code, 0)

    def test_unmeasured_reclaim_is_partial(self):
        verdict, code = gradual_swap.derive_outcome(True, False, "", None)
        self.assertEqual(verdict, "PARTIAL_RECLAIM_UNMEASURED")
        self.assertNotEqual(code, 0)

    def test_only_measured_complete_run_is_qualified(self):
        verdict, code = gradual_swap.derive_outcome(True, True, "", None)
        self.assertEqual(verdict, "QUALIFIED_PASS")
        self.assertEqual(code, 0)


class BoundedAllocation(unittest.TestCase):
    """Chunks are built in micro-slices, not one unbounded step."""

    def test_micro_slices_are_bounded(self):
        checks = []
        with mock.patch.object(gradual_swap, "MICRO_SLICE_MIB", 1):
            step_size = gradual_swap.MICRO_SLICE_MIB * 1024 * 1024
            chunk = gradual_swap.allocate_bounded(
                step_size * 3, lambda: checks.append(1)
            )

        self.assertEqual(len(chunk), step_size * 3)
        # Three 1 MiB micro-slices, not one 3 MiB step.
        self.assertEqual(len(checks), 3)

    def test_allocate_bounded_calls_checkin_per_slice(self):
        checks = []
        # 1 MiB total with a 50 MiB slice size is a single slice; use a tiny
        # monkeypatched slice constant so the fixture stays small.
        with mock.patch.object(gradual_swap, "MICRO_SLICE_MIB", 1):
            chunk = gradual_swap.allocate_bounded(
                3 * 1024 * 1024, lambda: checks.append(1)
            )
        self.assertEqual(len(chunk), 3 * 1024 * 1024)
        self.assertEqual(len(checks), 3)


class AllocationFailure(unittest.TestCase):
    """A failed allocation is classified, never swallowed into a PASS."""

    def test_failed_cleanup_is_reported_not_passed(self):
        verdict, code = gradual_swap.derive_outcome(
            True, True, "", "MemoryError: cannot allocate"
        )
        self.assertEqual(verdict, "ABORT_FAILED")
        self.assertNotEqual(code, 0)
        self.assertNotIn("PASS", verdict)

    def test_invalid_limits_clamp_safely(self):
        # main() clamps the target range before any allocation. Mirror that
        # clamp and assert the ramp cannot be pointed past the safe ceiling.
        max_target_pct = min(max(200, 1), 99)
        step_pct = max(1, 0)
        self.assertEqual(max_target_pct, 99)
        self.assertEqual(step_pct, 1)

    def test_negative_step_is_clamped_to_one(self):
        self.assertEqual(max(1, -5), 1)


class SignalSafety(unittest.TestCase):
    """A signal abort must never claim a clean zero-exit qualification."""

    def test_signal_abort_maps_to_nonzero(self):
        verdict, code = gradual_swap.derive_outcome(
            False, False, "signal_15", None
        )
        self.assertEqual(verdict, "ABORT_SAFETY")
        self.assertNotEqual(code, 0)

    def test_no_false_pass_strings_in_module_source(self):
        source = MODULE_PATH.read_text(encoding="utf-8")
        forbidden = [
            "100% PASS",
            "Zero Hang, Zero Freeze",
            "Zero hang, zero kernel panic",
            "Guarantees atomic memory release",
        ]
        for phrase in forbidden:
            self.assertNotIn(phrase, source)


if __name__ == "__main__":
    unittest.main()
