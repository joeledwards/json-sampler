#!/usr/bin/env python3
"""Local process checks. Build the CLI first; pass its executable as argument."""
import json
import os
import pathlib
import select
import signal
import subprocess
import sys
import tempfile
import time
import unittest

BINARY = str(pathlib.Path(sys.argv.pop(1)).resolve())
EXAMPLE = pathlib.Path(__file__).resolve().parents[1] / "example.ndjson"


def run(*args, data=b"{}[", env=None):
    return subprocess.run([BINARY, *args], input=data, capture_output=True,
                          timeout=10, env=env)


def stop(process):
    if process.poll() is None:
        process.kill()
    process.wait(timeout=5)
    for stream in (process.stdin, process.stdout, process.stderr):
        if stream and not stream.closed:
            stream.close()


class ReleaseChecks(unittest.TestCase):
    def test_example_and_version(self):
        result = run("--final-only", "--format", "expanded", data=EXAMPLE.read_bytes())
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        self.assertTrue(report["finished"])
        self.assertEqual(report["stream"]["candidates_confirmed"], 7)
        self.assertEqual(report["stream"]["candidates_rejected"], 3)
        self.assertEqual(report["stream"]["max_depth_encountered"], 49)
        self.assertIn(b"1.0.0", run("--version", data=b"").stdout)

    def test_summary_file_always_expanded_json(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "report.txt"
            path.write_text("stale content")
            result = run("--summary-file", str(path), "--final-only", "--format", "flat",
                         "--color", "always", data=b'{"x":[1,2]}')
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn(b"Stream metrics", result.stdout)
            self.assertNotIn(b"Path counts", result.stdout)
            report = json.loads(path.read_text())
            self.assertIsInstance(report["metrics"], list)
            self.assertTrue(report["finished"])
            self.assertNotIn(b"\x1b", path.read_bytes())

    @unittest.skipUnless(os.name == "posix", "Unix SIGINT contract")
    def test_idle_sigint(self):
        process = subprocess.Popen([BINARY, "--format", "expanded", "--interval", "0.02"],
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            self.assertTrue(select.select([process.stdout], [], [], 5)[0], "no idle report")
            first = json.loads(process.stdout.readline())
            self.assertFalse(first["finished"])
            process.send_signal(signal.SIGINT)
            process.wait(timeout=5)
            reports = [json.loads(line) for line in process.stdout.read().splitlines()]
            self.assertEqual(process.returncode, 130)
            self.assertEqual(reports[-1]["stop_reason"], "sigint")
            self.assertFalse(reports[-1]["finished"])
        finally:
            stop(process)

    @unittest.skipUnless(os.name == "posix", "Unix SIGINT contract")
    def test_final_only_sigint_preserves_pending_and_writes_file(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "report.json"
            process = subprocess.Popen([BINARY, "--summary-file", str(path), "--final-only"],
                                       stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            try:
                deadline = time.monotonic() + 5
                while not path.exists() and time.monotonic() < deadline:
                    self.assertIsNone(process.poll())
                    time.sleep(0.01)
                self.assertTrue(path.exists(), "file not opened after installing handler")
                process.stdin.write(b"{}[")
                process.stdin.flush()
                # Give the reader an opportunity to consume input and assert
                # final-only remains silent before interruption.
                self.assertFalse(select.select([process.stdout], [], [], 0.2)[0])
                self.assertEqual(path.read_bytes(), b"")
                process.send_signal(signal.SIGINT)
                process.wait(timeout=5)
                self.assertEqual(process.returncode, 130, process.stderr.read())
                report = json.loads(path.read_text())
                self.assertEqual(report["stop_reason"], "sigint")
                self.assertEqual(report["stream"]["pending_bytes"], 1)
                self.assertEqual(report["stream"]["candidates_confirmed"], 1)
                self.assertEqual(report["stream"]["candidates_incomplete_at_eof"], 0)
                self.assertIn(b"interrupted", process.stdout.read())
            finally:
                stop(process)

    def test_broken_stdout_without_summary(self):
        process = subprocess.Popen([BINARY, "--interval", "0.02"], stdin=subprocess.PIPE,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            process.stdout.close()
            process.stdin.write(b"{}")
            process.stdin.flush()
            process.wait(timeout=5)
            self.assertEqual(process.returncode, 0, process.stderr.read())
        finally:
            stop(process)

    def test_broken_stdout_preserves_summary_to_eof(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / "report.json"
            process = subprocess.Popen([BINARY, "--summary-file", str(path), "--interval", "0.02"],
                                       stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            try:
                process.stdout.close()
                process.stdin.write(b'{"x":1}')
                process.stdin.close()
                process.wait(timeout=5)
                self.assertEqual(process.returncode, 0, process.stderr.read())
                reports = [json.loads(line) for line in path.read_text().splitlines()]
                self.assertTrue(reports[-1]["finished"])
                self.assertEqual(reports[-1]["stream"]["candidates_confirmed"], 1)
            finally:
                stop(process)

    def test_file_open_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            result = run("--summary-file", str(pathlib.Path(directory) / "missing" / "report.json"))
            self.assertNotEqual(result.returncode, 0)
            self.assertIn(b"summary file", result.stderr)

    @unittest.skipUnless(pathlib.Path("/dev/full").exists(), "requires /dev/full")
    def test_file_write_failure(self):
        result = run("--summary-file", "/dev/full", "--final-only")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b"summary file", result.stderr)

    def test_redirected_color_and_json(self):
        for flag, colored in [("always", True), ("never", False), ("auto", False)]:
            result = run("--final-only", "--color", flag, data=b'{"x":1}')
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(b"\x1b[" in result.stdout, colored)
        result = run("--final-only", "--format", "expanded", "--color", "always")
        self.assertNotIn(b"\x1b", result.stdout)
        json.loads(result.stdout)

    @unittest.skipUnless(os.name == "posix", "requires Unix PTY")
    def test_terminal_auto_color(self):
        import pty
        master, slave = pty.openpty()
        try:
            environment = dict(os.environ)
            environment.pop("NO_COLOR", None)
            process = subprocess.Popen([BINARY, "--final-only"], stdin=subprocess.PIPE,
                                       stdout=slave, stderr=subprocess.PIPE, env=environment)
            output, errors = process.communicate(b'{"x":1}', timeout=5)
            self.assertEqual(process.returncode, 0, errors)
            self.assertTrue(select.select([master], [], [], 5)[0])
            self.assertIn(b"\x1b[", os.read(master, 65536))
        finally:
            os.close(master)
            os.close(slave)


if __name__ == "__main__":
    unittest.main()
