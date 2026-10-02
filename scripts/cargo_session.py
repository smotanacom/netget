#!/usr/bin/env python3
"""Supervise one Cargo process group; cancellation never signals discovered PIDs.

Registry entries identify the caller session and supervisor by PID/start identity.
The cancellation client only sends a nonce to a private Unix socket. The original
parent, which owns an unreaped child, signals its own process group. This avoids
both target-directory ambiguity and the check-then-kill PID-reuse race.
"""
import argparse
import ctypes
import json
import os
from pathlib import Path
import secrets
import select
import signal
import socket
import stat
import subprocess
import sys
import time


def process_identity(pid):
    if not isinstance(pid, int) or pid <= 0:
        return None
    if sys.platform == "darwin":
        # sys/proc_info.h: PROC_PIDTBSDINFO includes microsecond start time.
        # ps's second-resolution lstart is insufficient for rapid PID reuse.
        class BsdInfo(ctypes.Structure):
            _fields_ = [("identity", ctypes.c_uint32 * 12),
                        ("comm", ctypes.c_char * 16), ("name", ctypes.c_char * 32),
                        ("details", ctypes.c_uint32 * 6),
                        ("start_sec", ctypes.c_uint64), ("start_usec", ctypes.c_uint64)]
        try:
            libproc = ctypes.CDLL("/usr/lib/libproc.dylib")
            libproc.proc_pidinfo.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64,
                                           ctypes.c_void_p, ctypes.c_int]
            libproc.proc_pidinfo.restype = ctypes.c_int
            info = BsdInfo()
            size = libproc.proc_pidinfo(pid, 3, 0, ctypes.byref(info), ctypes.sizeof(info))
            if size == ctypes.sizeof(info) and info.identity[3] == pid:
                return f"darwin:{info.start_sec}:{info.start_usec}"
        except (OSError, AttributeError, OverflowError):
            pass
        return None
    # Linux includes boot identity and kernel start ticks.
    proc = Path(f"/proc/{pid}/stat")
    try:
        fields = proc.read_text().rsplit(")", 1)[1].split()
        return Path("/proc/sys/kernel/random/boot_id").read_text().strip() + ":" + fields[19]
    except (OSError, IndexError):
        pass
    try:
        result = subprocess.run(["/bin/ps", "-p", str(pid), "-o", "lstart="],
                                capture_output=True, text=True, timeout=2, check=False)
        return result.stdout.strip() if result.returncode == 0 else None
    except (OSError, subprocess.SubprocessError):
        return None


def private_directory(path):
    path.mkdir(mode=0o700, parents=True, exist_ok=True)
    info = path.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise RuntimeError(f"Refusing non-private build registry directory: {path}")
    return path


def registry(root):
    return private_directory(root / "tmp" / "cargo-sessions")


def run_build(args):
    session_start = process_identity(args.session_pid)
    if session_start is None:
        raise RuntimeError("CARGO_SESSION_PID must identify a live caller session")
    directory = registry(args.root)
    # Short path stays below Darwin's Unix-socket pathname limit, even in deep repos.
    sockets = private_directory(Path("/tmp") / f"netget-cargo-{os.getuid()}")
    token = secrets.token_hex(24)
    socket_path = sockets / f"{token[:24]}.sock"
    record_path = directory / f"{token}.json"
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    child = None
    stopped = []
    try:
        server.bind(str(socket_path))
        os.chmod(socket_path, 0o600)
        server.listen(4)
        command = args.command[1:] if args.command[:1] == ["--"] else args.command
        if not command:
            raise RuntimeError("missing Cargo command")
        for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
            signal.signal(sig, lambda number, _frame: stopped.append(number))
        child = subprocess.Popen(command, start_new_session=True)
        record = {"version": 1, "session_pid": args.session_pid,
                  "session_start": session_start, "supervisor_pid": os.getpid(),
                  "supervisor_start": process_identity(os.getpid()), "token": token,
                  "socket": str(socket_path), "command": command,
                  "target": os.environ.get("CARGO_TARGET_DIR", ""), "root": str(args.root)}
        temporary = record_path.with_suffix(".tmp")
        with temporary.open("x") as output:
            os.chmod(temporary, 0o600)
            json.dump(record, output)
        temporary.replace(record_path)
        print(f"Build session: {args.session_pid}; cancellation token: {token[:12]}", file=sys.stderr)
        while True:
            # Only this thread reaps the child. After a live poll, it cannot have
            # its PID reused until we next poll/wait, so killpg addresses our group.
            if stopped:
                os.killpg(child.pid, stopped[0])
                # Do not reap until escalation: an ignored TERM in a descendant
                # must not leak it or permit the group identifier to be recycled.
                time.sleep(0.5)
                try:
                    os.killpg(child.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                code = child.wait()
                return code if code >= 0 else 128 - code
            code = child.poll()
            if code is not None:
                return code if code >= 0 else 128 - code
            readable, _, _ = select.select([server], [], [], 0.1)
            if not readable:
                continue
            connection, _ = server.accept()
            with connection:
                try:
                    connection.settimeout(1)
                    supplied = connection.recv(128).decode("ascii", errors="replace").strip()
                    if secrets.compare_digest(supplied, token):
                        stopped.append(signal.SIGTERM)
                        connection.sendall(b"accepted\n")
                    else:
                        connection.sendall(b"rejected\n")
                except OSError:
                    # A stale/disconnected client must not terminate the build.
                    continue
    finally:
        server.close()
        # On setup errors, we still own this unreaped child. Never leave Cargo
        # detached merely because its registry could not be written.
        if child is not None and child.poll() is None:
            os.killpg(child.pid, signal.SIGKILL)
            child.wait()
        record_path.unlink(missing_ok=True)
        record_path.with_suffix(".tmp").unlink(missing_ok=True)
        socket_path.unlink(missing_ok=True)


def owned_records(args):
    directory = args.root / "tmp" / "cargo-sessions"
    if not directory.exists():
        return []
    registry(args.root)
    identity = process_identity(args.session_pid)
    records = []
    for path in directory.glob("*.json"):
        try:
            info = path.lstat()
            if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
                continue
            if info.st_size > 65536:
                continue
            record = json.loads(path.read_text())
            if (isinstance(record, dict) and record.get("version") == 1 and record.get("root") == str(args.root)
                    and record.get("session_pid") == args.session_pid
                    and identity is not None and record.get("session_start") == identity
                    and record.get("supervisor_start") is not None
                    and process_identity(record.get("supervisor_pid")) == record["supervisor_start"]):
                records.append(record)
        except (OSError, ValueError, TypeError):
            continue
    return records


def cancel_builds(args):
    records = owned_records(args)
    if not records:
        print("No registered builds owned by this live session.")
        return 0
    for record in records:
        print(f"Session {args.session_pid}: {record.get('command')!r} (target {record.get('target', '')})")
    if args.list_only:
        return 0
    if not args.yes and input("Cancel these session-owned builds? [y/N] ").strip().lower() != "y":
        return 0
    failed = False
    for record in records:
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
                client.settimeout(2)
                client.connect(record["socket"])
                client.sendall(record["token"].encode("ascii") + b"\n")
                if client.recv(128).strip() != b"accepted":
                    raise RuntimeError("supervisor rejected cancellation")
        except (OSError, KeyError, ValueError, TypeError, AttributeError, RuntimeError) as error:
            failed = True
            print(f"Build could not be cancelled through its owner: {error}", file=sys.stderr)
    return 1 if failed else 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("run", "cancel"))
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--session-pid", type=int, required=True)
    parser.add_argument("--yes", action="store_true")
    parser.add_argument("--list", dest="list_only", action="store_true")
    args, args.command = parser.parse_known_args()
    args.root = args.root.resolve()
    if os.name != "posix":
        parser.error("safe process-group supervision requires a POSIX host")
    try:
        return run_build(args) if args.mode == "run" else cancel_builds(args)
    except (OSError, RuntimeError, EOFError) as error:
        print(f"Cargo session error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
