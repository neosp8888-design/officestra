#!/usr/bin/env python3
"""office-test.py의 줄 필터·종료 코드·원문 로그·취소 전달·원문 복귀를 검증한다."""

from __future__ import annotations

import importlib.util
import io
import os
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest
from pathlib import Path


# 공유 작업 폴더에 scripts/__pycache__가 남지 않게 한다.
sys.dont_write_bytecode = True

SCRIPT_PATH = Path(__file__).with_name("office-test.py")
SPEC = importlib.util.spec_from_file_location("office_test", SCRIPT_PATH)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)

NODE = shutil.which("node")


def run_wrapper(*arguments: str, env: dict | None = None, cwd: str | None = None) -> subprocess.CompletedProcess:
    return subprocess.run(
        [sys.executable, str(SCRIPT_PATH), *arguments],
        capture_output=True,
        env=env,
        cwd=cwd,
        timeout=60,
    )


def log_path_from(output: str) -> Path:
    line = next(line for line in output.splitlines() if line.startswith("[office-test] 원문 로그 "))
    return Path(line.split(" ")[3])


class LineFilterTests(unittest.TestCase):
    def test_node_drops_only_plain_passing_lines(self) -> None:
        keep = MODULE.make_line_filter("node")
        dropped = [
            "✔ 통과 시험 1 (0.081334ms)",
            "  ✔ 중첩 통과 3 (0.014834ms)",
            "✔ 중첩 묶음 (1.190333ms)",
            "✔ 오래 걸린 시험 (1.5s)",
        ]
        kept = [
            "✔ 나중에 붙일 시험 (0.016292ms) # 원격 관제 재기동 확인",
            "﹣ PostgreSQL 연동 건너뜀 (0.014875ms) # OFFICESTRA_TEST_DATABASE_URL 없음",
            "✖ 요금제 표시 실패 (0.028417ms)",
            "▶ 중첩 묶음",
            "ℹ skipped 1",
            "      at Test.run (node:internal/test_runner/test:1118:25)",
            "(node:23146) ExperimentalWarning: SQLite is an experimental feature and might change at any time",
            "✔ 괄호 없는 통과 표시",
            "",
        ]
        for line in dropped:
            self.assertIsNone(keep(line), line)
        for line in kept:
            self.assertEqual(keep(line), line)

    def test_swift_drops_boilerplate_and_keeps_diagnostics(self) -> None:
        keep = MODULE.make_line_filter("swift")
        dropped = [
            "[18 / 26]",
            "[23 / 31] Broken",
            "[Planning deferred tasks]",
            "[Computing dependencies]",
            "[Pre-planning 1 / 361]",
            "Test Case '-[MiniTests.MiniTests testPass01]' started.",
            "Test Case '-[MiniTests.MiniTests testPass01]' passed (0.000 seconds).",
            "Test Suite 'MiniTests' started at 2026-09-23 22:17:42.838.",
            "Test Suite 'Selected tests' passed at 2026-09-23 21:37:20.142.",
        ]
        kept = [
            "Building for debugging...",
            "Build complete! (9.12초)",
            "Test Case '-[MiniTests.MiniTests testPlanNameFailure]' failed (0.036 seconds).",
            "Test Case '-[MiniTests.MiniTests testSkippedDiagnostic]' skipped (0.001 seconds).",
            "/tmp/Mini/Tests/MiniTests.swift:20: -[MiniTests.MiniTests testSkippedDiagnostic] : Test skipped - 사유",
            "Test Suite 'MiniTests' failed at 2026-09-23 22:17:42.877.",
            "\t Executed 10 tests, with 1 test skipped and 1 failure (0 unexpected) in 0.038 (0.038) seconds",
            "Failed frontend command:",
            "/Applications/Xcode.app/Contents/Developer/usr/bin/swift-frontend -frontend -c -primary-file Broken.swift",
            "[12 / 30] Compiling Mini Mini.swift",
            "warning: missing creator for mutated node: ('/tmp/x')",
            "◇ Test run started.",
        ]
        for line in dropped:
            self.assertIsNone(keep(line), line)
        for line in kept:
            self.assertEqual(keep(line), line)

    def test_color_and_link_codes_are_removed_from_kept_lines(self) -> None:
        keep = MODULE.make_line_filter("swift")
        colored = (
            "/tmp/Broken.swift:7:23: \x1b[1;31merror: \x1b[1;39mcannot convert return expression\x1b[0;0m "
            "[#\x1b]8;;https://docs.swift.org/x\x1b\\NoUsage\x1b]8;;\x1b\\]"
        )
        self.assertEqual(keep(colored), "/tmp/Broken.swift:7:23: error: cannot convert return expression [#NoUsage]")
        self.assertIsNone(keep("\x1b[0;36m[18 / 26]\x1b[0;0m"))


class OutputPumpTests(unittest.TestCase):
    def test_raw_log_keeps_every_byte_while_screen_hides_matches(self) -> None:
        raw = "✔ 통과 (0.1ms)\n✖ 실패 (0.2ms)\n\x1b[31m빨강\x1b[0m\n끝 줄 개행 없음".encode("utf-8")
        log, out = io.BytesIO(), io.BytesIO()
        pump = MODULE.OutputPump(log, out, MODULE.make_line_filter("node"))
        pump.pump(io.BytesIO(raw))
        self.assertEqual(log.getvalue(), raw)
        self.assertEqual(out.getvalue().decode("utf-8"), "✖ 실패 (0.2ms)\n빨강\n끝 줄 개행 없음")
        self.assertEqual((pump.total_lines, pump.hidden_lines), (4, 1))

    def test_filter_failure_falls_back_to_raw_for_the_rest(self) -> None:
        def broken(line: str) -> str | None:
            if line == "둘":
                raise ValueError("필터 결함")
            return None

        log, out = io.BytesIO(), io.BytesIO()
        pump = MODULE.OutputPump(log, out, broken)
        pump.pump(io.BytesIO("하나\n둘\n셋\n".encode("utf-8")))
        shown = out.getvalue().decode("utf-8")
        self.assertIn("[office-test] 출력 필터 오류로 이후는 원문을 그대로 출력합니다. ValueError: 필터 결함", shown)
        self.assertTrue(shown.endswith("둘\n셋\n"))
        self.assertNotIn("하나", shown)
        self.assertEqual(log.getvalue(), "하나\n둘\n셋\n".encode("utf-8"))

    def test_closed_screen_does_not_stop_raw_logging(self) -> None:
        class ClosedPipe(io.BytesIO):
            def write(self, data: bytes) -> int:
                raise BrokenPipeError()

        log = io.BytesIO()
        pump = MODULE.OutputPump(log, ClosedPipe(), MODULE.make_line_filter("node"))
        pump.pump(io.BytesIO(b"a\nb\n"))
        self.assertEqual(log.getvalue(), b"a\nb\n")


class ProcessTests(unittest.TestCase):
    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.env = {**os.environ, "TMPDIR": str(self.root)}

    def python_child(self, code: str) -> list[str]:
        return [sys.executable, "-c", textwrap.dedent(code)]

    def test_exit_codes_pass_through_unchanged(self) -> None:
        for code in (0, 1, 2):
            result = run_wrapper("--kind", "node", "--", *self.python_child(f"import sys; print('x'); sys.exit({code})"), env=self.env)
            self.assertEqual(result.returncode, code)
            self.assertIn(f"종료 코드 {code}", result.stdout.decode("utf-8"))

    def test_arguments_reach_the_command_without_a_shell(self) -> None:
        marker = self.root / "shell-ran"
        argument = f"a b; touch {marker} $HOME *"
        result = run_wrapper("--kind", "node", "--", *self.python_child("import sys; print(repr(sys.argv[1:]))"), argument, env=self.env)
        self.assertEqual(result.returncode, 0)
        self.assertIn(repr([argument]), result.stdout.decode("utf-8"))
        self.assertFalse(marker.exists())

    def test_cwd_option_and_raw_log_permissions(self) -> None:
        work = self.root / "work"
        work.mkdir()
        result = run_wrapper("--cwd", str(work), "--kind", "swift", "--",
                             *self.python_child("import os; print(os.getcwd()); print('[1 / 2]')"), env=self.env)
        output = result.stdout.decode("utf-8")
        self.assertIn(str(work.resolve()), output)
        self.assertNotIn("[1 / 2]", output)
        log = log_path_from(output)
        self.assertEqual(stat.S_IMODE(log.stat().st_mode), 0o600)
        self.assertEqual(log.read_text("utf-8"), f"{work.resolve()}\n[1 / 2]\n")
        self.assertIn("전체 2줄 중 1줄 생략", output)

    def test_stderr_is_kept_in_order_with_stdout(self) -> None:
        code = "import sys; print('out1', flush=True); print('err1', file=sys.stderr, flush=True); print('out2', flush=True)"
        result = run_wrapper("--kind", "node", "--", *self.python_child(code), env=self.env)
        self.assertEqual(result.stdout.decode("utf-8").splitlines()[:3], ["out1", "err1", "out2"])

    def test_missing_command_fails_exactly_without_log(self) -> None:
        result = run_wrapper("--kind", "node", "--", str(self.root / "없는-명령"), env=self.env)
        self.assertEqual(result.returncode, MODULE.EXIT_NOT_FOUND)
        self.assertIn("명령을 실행하지 못했습니다", result.stderr.decode("utf-8"))
        self.assertNotIn("Traceback", result.stderr.decode("utf-8"))
        self.assertEqual(list(self.root.glob("office-test-*.log")), [])

    def test_usage_errors_use_their_own_exit_code(self) -> None:
        for arguments in (["--kind", "node", "node"], ["--kind", "ruby", "--", "true"], ["--cwd", str(self.root / "x"), "--kind", "node", "--", "true"]):
            result = run_wrapper(*arguments, env=self.env)
            self.assertEqual(result.returncode, MODULE.EXIT_USAGE, arguments)

    def test_unavailable_raw_log_runs_once_and_prints_raw(self) -> None:
        # tempfile은 TMPDIR이 잘못돼도 /tmp로 대신하므로 로그 생성 실패는 함수를 바꿔 끼워 만든다.
        counter = self.root / "count"
        code = f"open({str(counter)!r}, 'a').write('x'); print('✔ 통과 (0.1ms)'); raise SystemExit(1)"
        original = MODULE.open_raw_log
        MODULE.open_raw_log = lambda: (None, None, "디스크 공간 부족")
        self.addCleanup(setattr, MODULE, "open_raw_log", original)
        out, err = io.BytesIO(), io.BytesIO()
        returncode = MODULE.run(["--kind", "node", "--", *self.python_child(code)], out, err)
        output = out.getvalue().decode("utf-8")
        self.assertEqual(returncode, 1)
        self.assertEqual(counter.read_text(), "x")
        self.assertIn("원문 로그를 만들지 못해 필터 없이 원문을 그대로 출력합니다. 디스크 공간 부족", output)
        self.assertIn("✔ 통과 (0.1ms)", output)
        self.assertNotIn("원문 로그 /", output)

    def start_with_grandchild(self, child_code: str) -> tuple[subprocess.Popen, int]:
        pid_file = self.root / "grandchild.pid"
        code = f"""
            import subprocess, time
            grandchild = subprocess.Popen(["sleep", "60"])
            open({str(pid_file)!r}, "w").write(str(grandchild.pid))
            print("ready", flush=True)
            {child_code}
            time.sleep(60)
        """
        wrapper = subprocess.Popen(
            [sys.executable, str(SCRIPT_PATH), "--kind", "node", "--", *self.python_child(code)],
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, env=self.env,
        )
        deadline = time.monotonic() + 10
        while not pid_file.exists() or not pid_file.read_text():
            self.assertLess(time.monotonic(), deadline, "하위 프로세스가 뜨지 않았습니다")
            time.sleep(0.05)
        return wrapper, int(pid_file.read_text())

    def assert_gone(self, pid: int) -> None:
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                return
            time.sleep(0.05)
        os.kill(pid, signal.SIGKILL)
        self.fail(f"하위 프로세스 {pid}가 남았습니다")

    def test_cancel_signal_reaches_grandchildren(self) -> None:
        wrapper, grandchild = self.start_with_grandchild("")
        wrapper.send_signal(signal.SIGTERM)
        output, _ = wrapper.communicate(timeout=10)
        self.assertEqual(wrapper.returncode, 128 + signal.SIGTERM)
        self.assertIn("취소 신호 SIGTERM를 자식 프로세스 묶음에 전달했습니다", output.decode("utf-8"))
        self.assert_gone(grandchild)

    def test_second_signal_kills_a_child_that_ignores_the_first(self) -> None:
        ignore = 'import signal; signal.signal(signal.SIGTERM, signal.SIG_IGN); signal.signal(signal.SIGINT, signal.SIG_IGN)'
        wrapper, grandchild = self.start_with_grandchild(ignore)
        time.sleep(0.3)
        wrapper.send_signal(signal.SIGINT)
        time.sleep(0.3)
        self.assertIsNone(wrapper.poll())
        wrapper.send_signal(signal.SIGINT)
        wrapper.communicate(timeout=10)
        self.assertEqual(wrapper.returncode, 128 + signal.SIGKILL)
        self.assert_gone(grandchild)

    def test_cancel_cleans_grandchild_that_outlives_its_parent(self) -> None:
        # 직계 자식은 SIGTERM에 바로 끝나고 손자만 SIGTERM을 무시하는 경우.
        pid_file = self.root / "stubborn.pid"
        grandchild = (
            "import os, signal, time; signal.signal(signal.SIGTERM, signal.SIG_IGN); "
            f"open({str(pid_file)!r}, 'w').write(str(os.getpid())); time.sleep(60)"
        )
        parent = f"import subprocess, sys, time; subprocess.Popen([sys.executable, '-c', {grandchild!r}]); time.sleep(60)"
        wrapper = subprocess.Popen(
            [sys.executable, str(SCRIPT_PATH), "--kind", "node", "--", *self.python_child(parent)],
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, env=self.env,
        )
        deadline = time.monotonic() + 10
        while not pid_file.exists() or not pid_file.read_text():
            self.assertLess(time.monotonic(), deadline, "손자 프로세스가 뜨지 않았습니다")
            time.sleep(0.05)
        stubborn = int(pid_file.read_text())
        wrapper.send_signal(signal.SIGTERM)
        wrapper.communicate(timeout=MODULE.KILL_GRACE_SECONDS + 10)
        self.assertEqual(wrapper.returncode, 128 + signal.SIGTERM)
        self.assert_gone(stubborn)

    def test_normal_exit_leaves_background_process_and_stops_reading(self) -> None:
        pid_file = self.root / "background.pid"
        code = f"""
            import subprocess
            background = subprocess.Popen(["sleep", "30"])
            open({str(pid_file)!r}, "w").write(str(background.pid))
            print("끝", flush=True)
        """
        result = run_wrapper("--kind", "node", "--", *self.python_child(code), env=self.env)
        background = int(pid_file.read_text())
        self.addCleanup(lambda: subprocess.run(["kill", "-9", str(background)], capture_output=True))
        output = result.stdout.decode("utf-8")
        self.assertEqual(result.returncode, 0)
        self.assertIn("출력 파이프를 닫지 않아 이후 출력은 읽지 않았습니다", output)
        self.assertIn("종료 코드 0", output)
        os.kill(background, 0)

    def test_unanswered_signal_escalates_after_grace_period(self) -> None:
        ignore = "import signal; signal.signal(signal.SIGTERM, signal.SIG_IGN)"
        wrapper, grandchild = self.start_with_grandchild(ignore)
        time.sleep(0.3)
        started = time.monotonic()
        wrapper.send_signal(signal.SIGTERM)
        wrapper.communicate(timeout=MODULE.KILL_GRACE_SECONDS + 10)
        self.assertGreaterEqual(time.monotonic() - started, MODULE.KILL_GRACE_SECONDS - 0.5)
        self.assertEqual(wrapper.returncode, 128 + signal.SIGKILL)
        self.assert_gone(grandchild)


@unittest.skipIf(NODE is None, "node가 PATH에 없습니다")
class NodeFixtureTests(unittest.TestCase):
    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.env = {**os.environ, "TMPDIR": str(self.root)}

    def write_test(self, body: str) -> None:
        (self.root / "sample.test.mjs").write_text(textwrap.dedent(body), "utf-8")

    def test_failures_skip_and_todo_survive(self) -> None:
        self.write_test("""
            import assert from "node:assert/strict";
            import test, { describe } from "node:test";
            for (let index = 1; index <= 20; index += 1) test(`통과 ${index}`, () => {});
            describe("묶음", () => {
              test("묶음 통과", () => {});
              test("묶음 실패", () => assert.deepEqual([1, 2], [1, 3], "배열 불일치"));
            });
            test("두 번째 실패", () => { throw new Error("요금제 표시 오류"); });
            test("건너뜀", { skip: "DB 없음" }, () => {});
            test("할 일", { todo: "나중에 확인" }, () => {});
        """)
        result = run_wrapper("--cwd", str(self.root), "--kind", "node", "--", NODE, "--test", env=self.env)
        output = result.stdout.decode("utf-8")
        self.assertEqual(result.returncode, 1)
        for needed in ("묶음 실패", "배열 불일치", "두 번째 실패", "요금제 표시 오류", "sample.test.mjs:",
                       "건너뜀", "DB 없음", "할 일", "나중에 확인", "ℹ fail 2", "ℹ skipped 1", "ℹ todo 1"):
            self.assertIn(needed, output)
        self.assertNotIn("✔ 통과 1 ", output)
        self.assertNotIn("묶음 통과", output)
        self.assertIn("✔ 통과 1 ", log_path_from(output).read_text("utf-8"))

    def test_all_passing_run_keeps_summary_and_exit_zero(self) -> None:
        self.write_test("""
            import test from "node:test";
            for (let index = 1; index <= 5; index += 1) test(`통과 ${index}`, () => {});
        """)
        result = run_wrapper("--cwd", str(self.root), "--kind", "node", "--", NODE, "--test", env=self.env)
        output = result.stdout.decode("utf-8")
        self.assertEqual(result.returncode, 0)
        self.assertIn("ℹ pass 5", output)
        self.assertIn("전체", output)
        self.assertNotIn("✔ 통과", output)


if __name__ == "__main__":
    unittest.main()
