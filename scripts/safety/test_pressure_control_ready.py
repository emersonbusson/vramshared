#!/usr/bin/env python3
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/safety/test_pressure_control_ready.py
"""Exercise the actual bounded readiness helper through readonly fixtures."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


class ReadinessTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)
        self.binary = self.root / "status"
        self.binary.write_text('#!/bin/sh\ncat "$RAMSHARED_READY_FIXTURE"\n')
        self.binary.chmod(0o700)
        self.health = dict(schema_version=4, ok=True, overall_state="HEALTHY", control_state="HEALTHY",
                           guardian_state="HEALTHY", origin_state="READY", topology_ok=True, ghost=False,
                           activation=dict(active=True), daemon=dict(alive=True), measurement_errors=[])

    def tearDown(self):
        self.directory.cleanup()

    def invoke(self, health, allow_origin=0):
        fixture = self.root / "fixture.json"
        fixture.write_text(json.dumps(health))
        environment = dict(os.environ, RAMSHARED_READY_FIXTURE=str(fixture))
        return subprocess.run(["bash", str(Path(__file__).with_name("wait-pressure-control-ready.sh")),
                               str(self.binary), str(self.root / "snapshot.json"), "1", str(allow_origin)],
                              env=environment, capture_output=True, timeout=10).returncode

    def test_real_healthy_and_authoritative_origin_recovery(self):
        self.assertEqual(self.invoke(self.health), 0)
        self.health.update(ok=False, overall_state="BLOCKED", cache_state="OFF")
        self.assertEqual(self.invoke(self.health, 1), 0)
        self.assertNotEqual(self.invoke(self.health), 0)

    def test_unknown_stale_missing_and_inactive_control_refuse(self):
        for field, value in (("schema_version", "4"), ("guardian_state", "STALE"),
                             ("control_state", "GUARDED"), ("origin_state", "FAILED"),
                             ("daemon", {"alive": False}), ("activation", {}),
                             ("measurement_errors", None)):
            health = dict(self.health)
            health[field] = value
            with self.subTest(field=field):
                self.assertNotEqual(self.invoke(health, 1), 0)

    def test_writeback_uses_measured_protection_without_an_nbd_swap_entry(self):
        self.health.update(schema_version=5, posture="writeback", protection_state="ACTIVE",
                           activation={"active": False}, tiers={"zram": {"present": True}},
                           tier_roles={"writeback_target_is_tier": True}, tier_state="bound",
                           capacity={"logical_capacity_kib": 4194304})
        self.assertEqual(self.invoke(self.health), 0)
        self.health["tier_state"] = "unbound"
        self.assertNotEqual(self.invoke(self.health, 1), 0)
        self.health["tier_state"] = "bound"
        self.health["protection_state"] = "BLOCKED"
        self.assertNotEqual(self.invoke(self.health, 1), 0)


if __name__ == "__main__":
    unittest.main()
