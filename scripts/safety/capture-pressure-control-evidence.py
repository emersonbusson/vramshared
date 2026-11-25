#!/usr/bin/env python3
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/safety/capture-pressure-control-evidence.py
"""Capture readonly, root-owned supervisor/admission evidence for a campaign."""
import argparse
import json
import os
import signal
import stat
import subprocess
import re
import time

stopping = False


def stop(_signum, _frame):
    global stopping
    stopping = True


def read_record(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC | os.O_NONBLOCK)
    try:
        metadata = os.fstat(fd)
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != 0 or metadata.st_nlink != 1:
            raise ValueError("unsafe_control_record")
        if metadata.st_mode & 0o022 or metadata.st_size > 65536:
            raise ValueError("unsafe_control_record")
        data = os.read(fd, 65537)
        if len(data) > 65536:
            raise ValueError("oversized_control_record")
        try:
            record = json.loads(data)
        except RecursionError as error:
            raise ValueError("control_record_exceeds_json_depth") from error
        if not isinstance(record, dict):
            raise ValueError("control_record_not_an_object")
        pending = [(record, 0)]
        visited = 0
        while pending:
            value, depth = pending.pop()
            visited += 1
            if depth > 32 or visited > 4096:
                raise ValueError("control_record_exceeds_json_structure_budget")
            if isinstance(value, dict):
                pending.extend((item, depth + 1) for item in value.values())
            elif isinstance(value, list):
                pending.extend((item, depth + 1) for item in value)
        return record
    finally:
        os.close(fd)


def capture():
    result = {"epoch_ms": time.time_ns() // 1000000, "errors": []}
    for key, path in (
        ("supervisor", "/run/ramshared/supervisor-state.json"),
        ("admission", "/run/ramshared/admission/admission-state.json"),
        ("reservations", "/run/ramshared/admission/reservations.json"),
    ):
        try:
            result[key] = read_record(path)
        except (OSError, ValueError):
            result[key] = None
            result["errors"].append(key + "_record_unavailable_or_invalid")
    try:
        try:
            frozen = read_record("/run/ramshared/frozen-scope.json")
        except FileNotFoundError:
            frozen = None
        result["frozen_scope"] = frozen
        result["frozen_scope_live"] = frozen_scope_live(frozen) if frozen is not None else None
    except (OSError, ValueError, subprocess.SubprocessError):
        result["frozen_scope"] = None
        result["frozen_scope_live"] = None
        result["errors"].append("frozen_scope_evidence_invalid_or_unavailable")
    return result


def frozen_scope_live(record):
    unit = record.get("unit")
    if not isinstance(unit, str) or not re.fullmatch(r"ramshared-(batch|build|browser-test)-[A-Za-z0-9_-]+\.scope", unit):
        raise ValueError("invalid_frozen_scope_unit")
    query = subprocess.run(
        ["systemctl", "show", "--property=Id,InvocationID,ControlGroup,FreezerState,ActiveState", unit],
        check=True, capture_output=True, text=True, timeout=1,
    )
    if len(query.stdout) > 8192:
        raise ValueError("oversized_frozen_scope_query")
    properties = {}
    for line in query.stdout.splitlines():
        key, value = line.split("=", 1)
        if key in properties:
            raise ValueError("duplicate_frozen_scope_property")
        properties[key] = value
    if set(properties) != {"Id", "InvocationID", "ControlGroup", "FreezerState", "ActiveState"}:
        raise ValueError("missing_frozen_scope_property")
    return properties


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    with open(args.output, "x", encoding="utf-8") as target:
        while not stopping:
            target.write(json.dumps(capture(), separators=(",", ":")) + "\n")
            target.flush()
            time.sleep(1)


if __name__ == "__main__":
    main()
