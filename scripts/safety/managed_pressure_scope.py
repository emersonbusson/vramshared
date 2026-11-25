#!/usr/bin/env python3
# Author: Emerson Busson
# Email: emersonbusson@gmail.com
# GitHub: https://github.com/emersonbusson
# LinkedIn: https://www.linkedin.com/in/emersonbusson
# Source: https://github.com/emersonbusson/vramshared
# File: scripts/safety/managed_pressure_scope.py
"""Run one admitted pressure probe in a managed batch scope with external custody."""
import argparse
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import tempfile
import time


def scope_path(text):
    rows = text.splitlines()
    if len(rows) != 1 or not rows[0].startswith("0::"):
        raise ValueError("ambiguous_cgroup_membership")
    path = rows[0][3:]
    if not re.fullmatch(r"/ramshared\.slice/ramshared-workloads\.slice/ramshared-batch-[A-Za-z0-9_-]+\.scope", path):
        raise ValueError("pressure_requires_managed_batch_scope")
    return path


def process_identity(pid):
    text = Path(f"/proc/{pid}/stat").read_text()
    fields = text[text.rindex(")") + 2:].split()
    return int(fields[1]), int(fields[19])


def unit_properties(unit):
    result = subprocess.run(
        ["systemctl", "show", "--property=Id,InvocationID,ControlGroup,MemoryMax", unit],
        check=True, capture_output=True, text=True, timeout=5,
    )
    properties = {}
    for row in result.stdout.splitlines():
        key, value = row.split("=", 1)
        if key in properties:
            raise ValueError("duplicate_scope_property")
        properties[key] = value
    if set(properties) != {"Id", "InvocationID", "ControlGroup", "MemoryMax"}:
        raise ValueError("missing_scope_identity")
    return properties


def validate_receipt(receipt, properties, membership, start_time):
    path = scope_path(membership)
    if (receipt["cgroup"] != path or receipt["unit"] != path.rsplit("/", 1)[1]
            or receipt["start_time"] != start_time or properties["Id"] != receipt["unit"]
            or properties["ControlGroup"] != path
            or not re.fullmatch(r"[0-9a-f]{32}", receipt["invocation_id"])
            or properties["InvocationID"] != receipt["invocation_id"]
            or not 0 < int(properties["MemoryMax"]) <= receipt["memory_max_bytes"]):
        raise ValueError("managed_pressure_identity_changed")
    return "/sys/fs/cgroup" + path


def write_receipt(path, pid, memory_max_bytes):
    membership = Path(f"/proc/{pid}/cgroup").read_text()
    cgroup = scope_path(membership)
    properties = unit_properties(cgroup.rsplit("/", 1)[1])
    _, start_time = process_identity(pid)
    if int(properties["MemoryMax"]) != memory_max_bytes:
        raise ValueError("initial_managed_memory_ceiling_mismatch")
    receipt = {"schema_version": 1, "pid": pid, "start_time": start_time,
               "unit": properties["Id"], "invocation_id": properties["InvocationID"],
               "cgroup": cgroup, "memory_max_bytes": memory_max_bytes}
    absolute = validate_receipt(receipt, properties, membership, start_time)
    with open(path, "x", encoding="utf-8") as target:
        json.dump(receipt, target)
    print(absolute)


def owned_receipt(path, runner_pid):
    receipt = json.loads(Path(path).read_text())
    pid = receipt["pid"]
    _, start_time = process_identity(pid)
    membership = Path(f"/proc/{pid}/cgroup").read_text()
    validate_receipt(receipt, unit_properties(receipt["unit"]), membership, start_time)
    ancestor = pid
    for _ in range(16):
        if ancestor == runner_pid:
            return receipt
        ancestor, _ = process_identity(ancestor)
        if ancestor <= 1:
            break
    raise ValueError("pressure_scope_is_not_owned_by_this_launcher")


def bound_generation(receipt):
    path = scope_path("0::" + receipt["cgroup"])
    properties = unit_properties(receipt["unit"])
    if (properties["Id"] != receipt["unit"] or properties["ControlGroup"] != path
            or properties["InvocationID"] != receipt["invocation_id"]
            or not 0 < int(properties["MemoryMax"]) <= receipt["memory_max_bytes"]):
        raise ValueError("owned_pressure_scope_generation_changed")


def signal_probe(receipt):
    descriptor = os.pidfd_open(receipt["pid"])
    try:
        _, birth = process_identity(receipt["pid"])
        if birth != receipt["start_time"]:
            raise ValueError("pressure_probe_pid_reused")
        signal.pidfd_send_signal(descriptor, signal.SIGTERM)
    finally:
        os.close(descriptor)


def worker_birth(pid, parent_pid, cgroup):
    parent, birth = process_identity(pid)
    membership = scope_path(Path(f"/proc/{pid}/cgroup").read_text())
    if parent != parent_pid or "/sys/fs/cgroup" + membership != cgroup:
        raise ValueError("pressure_worker_is_not_owned_by_this_probe")
    return birth


def signal_worker(pid, birth, cgroup):
    try:
        descriptor = os.pidfd_open(pid)
    except ProcessLookupError:
        return
    try:
        _, current_birth = process_identity(pid)
        membership = scope_path(Path(f"/proc/{pid}/cgroup").read_text())
        if current_birth != birth or "/sys/fs/cgroup" + membership != cgroup:
            raise ValueError("pressure_worker_identity_changed")
        signal.pidfd_send_signal(descriptor, signal.SIGTERM)
    except (FileNotFoundError, ProcessLookupError):
        return
    finally:
        os.close(descriptor)


def contain_scope(receipt_path, runner, admitted=None):
    # Only this launcher's live, birth-bound descendant can authorize effects.
    receipt = owned_receipt(receipt_path, runner.pid) if admitted is None else admitted
    if admitted is not None:
        bound_generation(receipt)
    try:
        signal_probe(receipt)
    except (FileNotFoundError, ProcessLookupError):
        # A proved scope can outlive its probe (for example after OOM). The
        # cached pre-allocation generation owns the surviving scope members.
        if admitted is None:
            raise
        bound_generation(receipt)
        subprocess.run(["systemctl", "kill", "--kill-whom=all", "--signal=TERM", receipt["unit"]],
                       check=True, timeout=5)
    # A frozen probe cannot execute its cleanup trap. Thaw only our own scope;
    # supervisor state, unrelated workloads and active storage are untouched.
    subprocess.run(["systemctl", "thaw", receipt["unit"]], check=True, timeout=5)
    try:
        runner.wait(timeout=45)
        deadline = time.monotonic() + 5
        while not scope_terminal(receipt_path) and time.monotonic() < deadline:
            time.sleep(0.1)
        if not scope_terminal(receipt_path):
            raise subprocess.TimeoutExpired("owned pressure scope", 45)
    except subprocess.TimeoutExpired:
        if admitted is None:
            receipt = owned_receipt(receipt_path, runner.pid)
        else:
            bound_generation(receipt)
        subprocess.run(["systemctl", "kill", "--kill-whom=all", "--signal=KILL", receipt["unit"]],
                       check=True, timeout=5)
        runner.wait(timeout=10)
        raise RuntimeError("pressure_integrity_cleanup_timed_out")


def scope_terminal(receipt_path):
    if not Path(receipt_path).exists():
        return False
    receipt = json.loads(Path(receipt_path).read_text())
    path = scope_path("0::" + receipt["cgroup"])
    events = Path("/sys/fs/cgroup" + path) / "cgroup.events"
    try:
        return "populated 0" in events.read_text().splitlines()
    except FileNotFoundError:
        return True


def run(args):
    if os.geteuid() != 0 or os.environ.get("RAMSHARED_PRESSURE_PROBE_ADMITTED") != "1":
        raise ValueError("managed_pressure_requires_root_and_campaign_admission")
    if not 1 <= args.memory_max_mib <= 16384 or not 1 <= args.deadline_sec <= 1800:
        raise ValueError("managed_pressure_budget_out_of_range")
    if not hasattr(os, "pidfd_open") or not hasattr(signal, "pidfd_send_signal"):
        raise ValueError("managed_pressure_requires_pidfd_signal_custody")
    stopping = []
    for sig in (signal.SIGINT, signal.SIGTERM):
        signal.signal(sig, lambda number, _frame: stopping.append(number))
    temporary = tempfile.mkdtemp(prefix="ramshared-pressure-scope-")
    runner = None
    try:
        receipt_path = str(Path(temporary) / "scope.json")
        command = [args.binary, "run", "--class", "batch", "--memory-max", str(args.memory_max_mib),
                   "--", "bash", args.probe, "--managed-scope-receipt", receipt_path, *args.probe_args]
        runner = subprocess.Popen(command)
        admitted = None
        deadline = time.monotonic() + args.deadline_sec
        while runner.poll() is None and not stopping and time.monotonic() < deadline:
            if admitted is None and Path(receipt_path).exists():
                try:
                    admitted = owned_receipt(receipt_path, runner.pid)
                except (ValueError, OSError, KeyError):
                    time.sleep(0.1)
                    continue
                with open(receipt_path + ".admitted", "x") as acknowledgement:
                    acknowledgement.write("owned_scope_generation_verified\n")
            time.sleep(0.2)
        if runner.poll() is None:
            contain_scope(receipt_path, runner, admitted)
            raise RuntimeError("managed_pressure_deadline_or_cancellation")
        if not scope_terminal(receipt_path):
            if admitted is not None:
                contain_scope(receipt_path, runner, admitted)
            raise RuntimeError("managed_pressure_scope_terminal_state_unproven")
        return runner.returncode
    finally:
        if runner is not None and runner.poll() is not None and scope_terminal(receipt_path):
            Path(receipt_path).unlink(missing_ok=True)
            Path(receipt_path + ".admitted").unlink(missing_ok=True)
            Path(temporary).rmdir()
        else:
            print(f"managed pressure: custody evidence retained at {temporary}", file=sys.stderr)


def main():
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="action", required=True)
    receipt = sub.add_parser("receipt")
    receipt.add_argument("path")
    receipt.add_argument("pid", type=int)
    receipt.add_argument("memory_max_bytes", type=int)
    birth = sub.add_parser("worker-birth")
    birth.add_argument("pid", type=int)
    birth.add_argument("parent_pid", type=int)
    birth.add_argument("cgroup")
    terminate = sub.add_parser("signal-worker")
    terminate.add_argument("pid", type=int)
    terminate.add_argument("birth", type=int)
    terminate.add_argument("cgroup")
    launch = sub.add_parser("run")
    launch.add_argument("--binary", required=True)
    launch.add_argument("--probe", required=True)
    launch.add_argument("--memory-max-mib", type=int, required=True)
    launch.add_argument("--deadline-sec", type=int, required=True)
    launch.add_argument("probe_args", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if args.action == "receipt":
        write_receipt(args.path, args.pid, args.memory_max_bytes)
        return 0
    if args.action == "worker-birth":
        print(worker_birth(args.pid, args.parent_pid, args.cgroup))
        return 0
    if args.action == "signal-worker":
        signal_worker(args.pid, args.birth, args.cgroup)
        return 0
    if args.probe_args[:1] == ["--"]:
        args.probe_args.pop(0)
    return run(args)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, KeyError, subprocess.SubprocessError, RuntimeError) as error:
        print(f"managed pressure: {error}", file=sys.stderr)
        sys.exit(1)
