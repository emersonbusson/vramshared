#!/usr/bin/env python3
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/safety/test_pressure_control_evidence.py
"""Exercise actual readonly control capture against unsafe filesystem records."""
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import subprocess

spec = importlib.util.spec_from_file_location("control_capture", Path(__file__).with_name("capture-pressure-control-evidence.py"))
capture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(capture)


class ControlCaptureTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.path = Path(self.temporary.name) / "record.json"
        self.path.write_text(json.dumps({"schema_version": 3}))
        self.path.chmod(0o600)

    def tearDown(self):
        self.temporary.cleanup()

    @unittest.skipUnless(os.geteuid() == 0, "acceptance requires actual root ownership")
    def test_exact_root_owned_regular_record(self):
        self.assertEqual(capture.read_record(self.path), {"schema_version": 3})

    def test_symlink_hardlink_fifo_and_writable_records_refuse(self):
        link = self.path.with_name("symlink")
        link.symlink_to(self.path)
        with self.assertRaises(OSError):
            capture.read_record(link)
        hardlink = self.path.with_name("hardlink")
        os.link(self.path, hardlink)
        with self.assertRaises(ValueError):
            capture.read_record(self.path)
        hardlink.unlink()
        fifo = self.path.with_name("fifo")
        os.mkfifo(fifo, 0o600)
        with self.assertRaises(ValueError):
            capture.read_record(fifo)
        self.path.chmod(0o622)
        with self.assertRaises(ValueError):
            capture.read_record(self.path)

    def test_oversized_malformed_and_non_object_records_refuse(self):
        for data in ("x" * 65537, "{broken", "[]", "null", '{"nested":' * 1100 + '0' + '}' * 1100):
            self.path.write_text(data)
            with self.subTest(data=data[:20]), self.assertRaises(ValueError):
                capture.read_record(self.path)

    @unittest.skipUnless(os.geteuid() == 0, "refusal requires actual foreign ownership")
    def test_non_root_owner_refuses(self):
        os.chown(self.path, 65534, 65534)
        with self.assertRaises(ValueError):
            capture.read_record(self.path)

    def test_frozen_scope_query_is_readonly_bounded_and_rejects_foreign_units(self):
        with patch.object(capture.subprocess, 'run') as query:
            with self.assertRaises(ValueError):
                capture.frozen_scope_live({'unit': 'foreign.service'})
            query.assert_not_called()
            query.return_value = subprocess.CompletedProcess([], 0, 'Id=ramshared-batch-fixture.scope\nInvocationID=' + 'a' * 32 + '\nControlGroup=/ramshared.slice/ramshared-workloads.slice/ramshared-batch-fixture.scope\nFreezerState=frozen\nActiveState=active\n')
            result = capture.frozen_scope_live({'unit': 'ramshared-batch-fixture.scope'})
            self.assertEqual(result['FreezerState'], 'frozen')
            self.assertEqual(query.call_args.kwargs['timeout'], 1)
            self.assertEqual(query.call_args.args[0][1], 'show')
            query.return_value.stdout += 'Id=duplicate\n'
            with self.assertRaises(ValueError):
                capture.frozen_scope_live({'unit': 'ramshared-batch-fixture.scope'})

    def test_frozen_scope_query_timeout_and_incomplete_projection_refuse(self):
        with patch.object(capture.subprocess, 'run', side_effect=subprocess.TimeoutExpired('fixture', 1)):
            with self.assertRaises(subprocess.TimeoutExpired):
                capture.frozen_scope_live({'unit': 'ramshared-batch-fixture.scope'})
        with patch.object(capture.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0, 'Id=only\n')):
            with self.assertRaises(ValueError):
                capture.frozen_scope_live({'unit': 'ramshared-batch-fixture.scope'})


if __name__ == "__main__":
    unittest.main()
