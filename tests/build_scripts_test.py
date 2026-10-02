"""CPU-only build-wrapper regressions; all commands/configs stay in temporary fixtures."""

import os
import importlib.util
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import time
import unittest


ROOT = Path(__file__).resolve().parent.parent


class BuildScriptsTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="netget-build-script-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        for name in ("cargo.sh", "cargo-isolated.sh", "cargo-isolated-kill.sh",
                     "scripts/cargo_session.py", "scripts/sccache/cargo-sccache.sh"):
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(ROOT / name, path)
        bins = self.root / "bin"
        bins.mkdir()
        for name, source in {
            "cargo": '#!/bin/bash\nprintf "%s\\n" "$@" > "$AUDIT_ARGS"\nprintf "%s" "${RUSTC_WRAPPER-unset}" > "$AUDIT_WRAPPER"\nif [[ "${AUDIT_HOLD:-0}" == 1 ]]; then while true; do sleep 1; done; fi\nexit "${AUDIT_EXIT:-0}"\n',
            "ps": "#!/bin/bash\nexit 1\n",
        }.items():
            path = bins / name
            path.write_text(source)
            path.chmod(0o755)
        self.args = self.root / "args"
        self.wrapper = self.root / "wrapper"
        self.env = dict(os.environ, PATH=f"{bins}:/usr/bin:/bin",
                        AUDIT_ARGS=str(self.args), AUDIT_WRAPPER=str(self.wrapper),
                        CARGO_TARGET_DIR=str(self.root / "target"),
                        CARGO_USE_ISOLATION="false", RUSTC_WRAPPER="",
                        CARGO_SESSION_PID=str(os.getpid()))

    def run_wrapper(self, name, *args):
        return subprocess.run(["/bin/bash", str(self.root / name), *args],
                              cwd=self.root, env=self.env, capture_output=True,
                              text=True, timeout=10)

    def test_cleanup_does_not_exit_before_cargo(self):
        stale = self.root / "target-claude/claude-99999999"
        stale.mkdir(parents=True)
        self.env.update(CARGO_USE_ISOLATION="true", CARGO_CLEANUP_OLD="true")
        result = self.run_wrapper("cargo.sh", "check")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(stale.exists())
        self.assertEqual(self.args.read_text(), "check\n")

    def test_explicit_empty_rustc_wrapper_disables_sccache(self):
        result = self.run_wrapper("cargo-isolated.sh", "check", "--offline")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.wrapper.read_text(), "")

    def test_sccache_fallback_finds_root_wrapper_and_preserves_arguments(self):
        result = self.run_wrapper("scripts/sccache/cargo-sccache.sh", "check", "a b")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.args.read_text().splitlines(), ["check", "a b"])
        self.assertEqual(self.wrapper.read_text(), "")

    def test_cargo_failure_is_not_hidden_by_logging(self):
        self.env["AUDIT_EXIT"] = "23"
        result = self.run_wrapper("cargo-isolated.sh", "check")
        self.assertEqual(result.returncode, 23, result.stderr)

    def wait_for_records(self, count):
        deadline = time.monotonic() + 5
        directory = self.root / "tmp/cargo-sessions"
        while time.monotonic() < deadline:
            records = list(directory.glob("*.json"))
            if len(records) == count:
                return records
            time.sleep(0.02)
        self.fail(f"expected {count} fixture build records")

    def cancel_fixture(self, session_pid):
        return subprocess.run(["/bin/bash", str(self.root / "cargo-isolated-kill.sh"), "--yes"],
                              cwd=self.root, env=dict(self.env, CARGO_SESSION_PID=str(session_pid)),
                              capture_output=True, text=True, timeout=5)

    def test_shared_target_cancellation_only_reaches_own_session(self):
        # Both commands are harmless fixture loops, never real Cargo builds.
        other_session = subprocess.Popen(["/bin/sleep", "30"])
        processes = []
        try:
            for session in [os.getpid(), other_session.pid]:
                processes.append(subprocess.Popen(["/bin/bash", str(self.root / "cargo-isolated.sh"), "check"],
                    cwd=self.root, env=dict(self.env, AUDIT_HOLD="1", CARGO_SESSION_PID=str(session)),
                    stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True))
            self.wait_for_records(2)
            result = self.cancel_fixture(os.getpid())
            self.assertEqual(result.returncode, 0, result.stderr)
            processes[0].communicate(timeout=5)
            self.assertNotEqual(processes[0].returncode, 0)
            self.assertIsNone(processes[1].poll(), "another session's fixture was cancelled")
            self.assertIsNone(other_session.poll(), "session owner itself was signalled")
            self.assertEqual(self.cancel_fixture(other_session.pid).returncode, 0)
            processes[1].communicate(timeout=5)
            self.wait_for_records(0)
        finally:
            self.cancel_fixture(os.getpid())
            self.cancel_fixture(other_session.pid)
            for process in processes:
                process.communicate(timeout=5)
            other_session.terminate()
            other_session.wait(timeout=5)

    def test_stale_or_forged_pid_record_never_signals_a_process(self):
        spec = importlib.util.spec_from_file_location("cargo_session_fixture", self.root / "scripts/cargo_session.py")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        directory = module.registry(self.root)
        unrelated = subprocess.Popen(["/bin/sleep", "30"])
        try:
            record = {"version": 1, "root": str(self.root), "session_pid": os.getpid(),
                      "session_start": "stale-reused-pid", "supervisor_pid": unrelated.pid,
                      "supervisor_start": module.process_identity(unrelated.pid),
                      "token": "forged", "socket": str(self.root / "missing.sock"),
                      "command": ["fixture"], "target": str(self.root / "target")}
            path = directory / "forged.json"
            path.write_text(json.dumps(record))
            path.chmod(0o600)
            self.assertEqual(self.cancel_fixture(os.getpid()).returncode, 0)
            self.assertIsNone(unrelated.poll())
            # Even a fabricated current identity can only attempt socket IPC;
            # no signal is ever sent to the PID stored in this JSON.
            record["session_start"] = module.process_identity(os.getpid())
            path.write_text(json.dumps(record))
            self.assertEqual(self.cancel_fixture(os.getpid()).returncode, 1)
            self.assertIsNone(unrelated.poll())
        finally:
            unrelated.terminate()
            unrelated.wait(timeout=5)

    def test_generated_credentials_are_literal_shell_data_and_private(self):
        marker = self.root / "must-not-execute"
        credential = f'quote" \\ $HOME $(touch {marker}) `touch {marker}`'
        for script, variable in [("setup-sccache-r2.sh", "AWS_SECRET_ACCESS_KEY"),
                                 ("setup-sccache-upstash.sh", "SCCACHE_REDIS_ENDPOINT")]:
            source = (ROOT / "scripts/sccache" / script).read_text()
            # Exercise only generation, never the interactive/network setup steps.
            block = source[source.index("(umask 077"):source.index('\nchmod 600')]
            config = self.root / script
            env = dict(self.env, CONFIG_FILE=str(config), BUCKET_NAME="bucket",
                       ENDPOINT="endpoint", ACCESS_KEY="key", SECRET_KEY=credential,
                       REDIS_URL=credential)
            subprocess.run(["/bin/bash", "-c", block], env=env, check=True, timeout=10)
            result = subprocess.run(["/bin/bash", "-c",
                                     'source "$CONFIG_FILE"; printf "%s" "$' + variable + '"'],
                                    env=env, check=True, capture_output=True, text=True, timeout=10)
            self.assertEqual(result.stdout, credential)
            self.assertFalse(marker.exists())
            self.assertEqual(config.stat().st_mode & 0o777, 0o600)


if __name__ == "__main__":
    unittest.main()
