#!/usr/bin/env python3
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/safety/test_managed_pressure_scope.py
"""Counterexamples for managed pressure identity and external containment."""
import copy
import signal
import subprocess
import unittest
from unittest.mock import Mock, patch

import managed_pressure_scope as scope


class ManagedScopeTests(unittest.TestCase):
    def setUp(self):
        self.path = "/ramshared.slice/ramshared-workloads.slice/ramshared-batch-fixture.scope"
        self.receipt = {"pid": 123, "start_time": 44, "cgroup": self.path,
                        "unit": "ramshared-batch-fixture.scope", "invocation_id": "a" * 32,
                        "memory_max_bytes": 1200 * 1024 * 1024}
        self.properties = {"Id": self.receipt["unit"], "InvocationID": "a" * 32,
                           "ControlGroup": self.path, "MemoryMax": str(self.receipt["memory_max_bytes"])}

    def test_exact_scope_and_reduced_ceiling_are_accepted(self):
        self.assertEqual(scope.validate_receipt(self.receipt, self.properties, "0::" + self.path, 44),
                         "/sys/fs/cgroup" + self.path)
        self.properties["MemoryMax"] = "1000"
        scope.validate_receipt(self.receipt, self.properties, "0::" + self.path, 44)

    def test_unmanaged_nested_forged_and_ambiguous_membership_refuse(self):
        for path in ("/", "/ramshared-probe-123", self.path + "/child", self.path + "/../x",
                     self.path.replace("batch", "interactive"), self.path + "\n0::/"):
            with self.subTest(path=path), self.assertRaises(ValueError):
                scope.scope_path("0::" + path)

    def test_generation_birth_path_and_budget_mismatch_refuse(self):
        for key, value in (("Id", "foreign.scope"), ("InvocationID", "b" * 32),
                           ("ControlGroup", "/"), ("MemoryMax", "0"),
                           ("MemoryMax", str(self.receipt["memory_max_bytes"] + 1))):
            properties = copy.deepcopy(self.properties)
            properties[key] = value
            with self.subTest(key=key), self.assertRaises(ValueError):
                scope.validate_receipt(self.receipt, properties, "0::" + self.path, 44)
        with self.assertRaises(ValueError):
            scope.validate_receipt(self.receipt, self.properties, "0::" + self.path, 45)

    def test_unknown_ownership_performs_no_signal_or_thaw(self):
        with patch.object(scope, "owned_receipt", side_effect=ValueError("foreign")), \
                patch.object(scope, "signal_probe") as kill, patch.object(scope.subprocess, "run") as run:
            with self.assertRaises(ValueError):
                scope.contain_scope("fixture", Mock(pid=1))
            kill.assert_not_called()
            run.assert_not_called()

    def test_owned_frozen_cleanup_terms_thaws_and_reaps(self):
        runner = Mock(pid=1)
        with patch.object(scope, "owned_receipt", return_value=self.receipt), \
                patch.object(scope, "signal_probe") as kill, patch.object(scope.subprocess, "run") as run, \
                patch.object(scope, "process_identity", return_value=(1, 44)), \
                patch.object(scope, "scope_terminal", return_value=True):
            scope.contain_scope("fixture", runner)
            kill.assert_called_once_with(self.receipt)
            self.assertEqual(run.call_args.args[0], ["systemctl", "thaw", self.receipt["unit"]])
            runner.wait.assert_called_once_with(timeout=45)

    def test_forced_cleanup_rechecks_generation_and_reports_failed_integrity(self):
        runner = Mock(pid=1)
        runner.wait.side_effect = [subprocess.TimeoutExpired("fixture", 45), 0]
        with patch.object(scope, "owned_receipt", side_effect=[self.receipt, ValueError("reused")]), \
                patch.object(scope, "signal_probe"), patch.object(scope.subprocess, "run") as run, \
                patch.object(scope, "process_identity", return_value=(1, 44)):
            with self.assertRaises(ValueError):
                scope.contain_scope("fixture", runner)
            self.assertEqual(run.call_count, 1)
        with patch.object(scope, "owned_receipt", return_value=self.receipt), \
                patch.object(scope, "signal_probe"), patch.object(scope.subprocess, "run") as run, \
                patch.object(scope, "process_identity", return_value=(1, 44)):
            runner.wait.side_effect = [subprocess.TimeoutExpired("fixture", 45), 0]
            with self.assertRaisesRegex(RuntimeError, "integrity_cleanup_timed_out"):
                scope.contain_scope("fixture", runner)
            self.assertIn("--signal=KILL", run.call_args.args[0])

    def test_cached_scope_custody_survives_probe_death_without_foreign_pid_signal(self):
        with patch.object(scope, "bound_generation") as bound, \
                patch.object(scope, "signal_probe", side_effect=FileNotFoundError) as kill, patch.object(scope.subprocess, "run") as run, \
                patch.object(scope, "scope_terminal", return_value=True):
            scope.contain_scope("fixture", Mock(pid=1), self.receipt)
            kill.assert_called_once_with(self.receipt)
            self.assertEqual(bound.call_count, 2)
            self.assertIn("--signal=TERM", run.call_args_list[0].args[0])

    def test_reused_probe_pid_and_scope_generation_refuse_all_effects(self):
        for drift in ("pid", "scope"):
            with patch.object(scope, "bound_generation", side_effect=ValueError("reused") if drift == "scope" else None), \
                    patch.object(scope, "signal_probe", side_effect=ValueError("pid reused")) as kill, patch.object(scope.subprocess, "run") as run:
                with self.assertRaises(ValueError):
                    scope.contain_scope("fixture", Mock(pid=1), self.receipt)
                if drift == "scope":
                    kill.assert_not_called()
                run.assert_not_called()

    def test_pidfd_is_pinned_before_birth_check_and_never_signals_reused_pid(self):
        with patch.object(scope.os, "pidfd_open", return_value=7), \
                patch.object(scope, "process_identity", return_value=(1, 45)), \
                patch.object(scope.signal, "pidfd_send_signal") as send, \
                patch.object(scope.os, "close") as close:
            with self.assertRaises(ValueError):
                scope.signal_probe(self.receipt)
            send.assert_not_called()
            close.assert_called_once_with(7)
        with patch.object(scope.os, "pidfd_open", return_value=7), \
                patch.object(scope, "process_identity", return_value=(1, 44)), \
                patch.object(scope.signal, "pidfd_send_signal") as send, \
                patch.object(scope.os, "close"):
            scope.signal_probe(self.receipt)
            send.assert_called_once_with(7, signal.SIGTERM)

    def test_worker_cleanup_requires_matching_birth_and_managed_cgroup(self):
        for birth, cgroup in ((45, self.path), (44, "/ramshared.slice/ramshared-workloads.slice/ramshared-batch-foreign.scope")):
            with patch.object(scope.os, "pidfd_open", return_value=7), \
                    patch.object(scope, "process_identity", return_value=(1, birth)), \
                    patch.object(scope.Path, "read_text", return_value="0::" + cgroup), \
                    patch.object(scope.signal, "pidfd_send_signal") as send, patch.object(scope.os, "close"):
                with self.assertRaises(ValueError):
                    scope.signal_worker(123, 44, "/sys/fs/cgroup" + self.path)
                send.assert_not_called()
        with patch.object(scope.os, "pidfd_open", return_value=7), \
                patch.object(scope, "process_identity", return_value=(1, 44)), \
                patch.object(scope.Path, "read_text", return_value="0::" + self.path), \
                patch.object(scope.signal, "pidfd_send_signal") as send, patch.object(scope.os, "close"):
            scope.signal_worker(123, 44, "/sys/fs/cgroup" + self.path)
            send.assert_called_once_with(7, signal.SIGTERM)

    def test_worker_birth_refuses_a_foreign_parent_before_start_gate_release(self):
        with patch.object(scope, "process_identity", return_value=(999, 44)), \
                patch.object(scope.Path, "read_text", return_value="0::" + self.path):
            with self.assertRaises(ValueError):
                scope.worker_birth(123, 1, "/sys/fs/cgroup" + self.path)


if __name__ == "__main__":
    unittest.main()
