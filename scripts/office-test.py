#!/usr/bin/env python3
"""테스트 명령을 셸 없이 그대로 실행하고, 반복되는 성공·시작 줄과 색상·진행률만 걸러 보여 준다.

사용법:
  python3 scripts/office-test.py [--cwd DIR] --kind node|swift -- COMMAND [ARG...]

- 자식의 종료 코드를 그대로 돌려준다. 신호로 끝나면 128+신호 번호다.
- stdout·stderr를 합친 원문 전체를 0600 임시 로그에 남기고 마지막 줄에 경로를 적는다.
- 걸러 내는 줄은 아래 규칙과 정확히 맞는 줄뿐이다. 해석하지 못한 줄은 모두 남긴다.
- 취소 신호(SIGINT·SIGTERM·SIGHUP)는 자식 프로세스 묶음 전체에 전달한다. 직계 자식이 먼저
  끝나도 묶음이 5초 안에 비지 않거나 신호가 한 번 더 오면 남은 프로세스를 SIGKILL로 정리한다.
- 취소하지 않고 정상 종료했으면 남은 백그라운드 프로세스는 건드리지 않는다. 그 프로세스가
  출력 파이프를 잡고 있으면 5초 뒤 읽기만 멈춘다.
- 원문 로그를 만들 수 없거나 필터가 실패하면 명령을 다시 실행하지 않고 원문을 그대로 출력한다.
- 이 스크립트 자신의 사용법 오류는 125, 실행 권한 없음은 126, 명령 없음은 127로 끝난다.
"""

from __future__ import annotations

import os
import re
import select
import signal
import subprocess
import sys
import tempfile
import threading
import time
from typing import BinaryIO, Callable, Optional

USAGE = "사용법: python3 scripts/office-test.py [--cwd DIR] --kind node|swift -- COMMAND [ARG...]"
EXIT_USAGE = 125
EXIT_NOT_EXECUTABLE = 126
EXIT_NOT_FOUND = 127
KILL_GRACE_SECONDS = 5.0
READER_DRAIN_SECONDS = 5.0
GROUP_EXIT_SECONDS = 2.0
WAIT_POLL_SECONDS = 0.1
PIPE_POLL_SECONDS = 0.1
CHUNK_BYTES = 65536
FORWARDED_SIGNALS = (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)

# 색상 코드와 터미널 링크 코드. swift 컴파일러는 파이프로 받아도 붙인다.
ANSI_PATTERN = re.compile(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b\[[0-9;?]*[A-Za-z]")

# 통과한 시험·묶음 줄만 지운다. 할 일·건너뜀은 끝에 "# 사유"가 붙어 이 규칙에 맞지 않는다.
NODE_DROP_PATTERNS = (
    re.compile(r"^\s*✔ .* \(\d+(?:\.\d+)?m?s\)$"),
)

SWIFT_DROP_PATTERNS = (
    # 빌드 진행률. "/" 양옆이 유니코드 공백일 때도 있고 뒤에 대상 이름 하나가 붙기도 한다.
    re.compile(r"^\[\d+\s*/\s*\d+\](?: \S+)?$"),
    re.compile(r"^\[(?:Planning deferred tasks|Computing dependencies)\]$"),
    re.compile(r"^\[Pre-planning \d+\s*/\s*\d+\]$"),
    re.compile(r"^Test Case '-\[[^\]]+\]' started\.$"),
    re.compile(r"^Test Case '-\[[^\]]+\]' passed \(\d+(?:\.\d+)? seconds\)\.$"),
    re.compile(r"^Test Suite '[^']+' (?:started|passed) at \d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}\.\d{3}\.$"),
)

DROP_PATTERNS = {"node": NODE_DROP_PATTERNS, "swift": SWIFT_DROP_PATTERNS}


class UsageError(Exception):
    pass


def parse_arguments(argv: list[str]) -> tuple[Optional[str], str, list[str]]:
    if "--" not in argv:
        raise UsageError("명령 앞에 -- 가 필요합니다.")
    split = argv.index("--")
    options, command = argv[:split], argv[split + 1:]
    if not command:
        raise UsageError("실행할 명령이 없습니다.")
    cwd: Optional[str] = None
    kind: Optional[str] = None
    index = 0
    while index < len(options):
        name = options[index]
        if name in ("--cwd", "--kind") and index + 1 < len(options):
            value = options[index + 1]
            if name == "--cwd":
                cwd = value
            else:
                kind = value
            index += 2
            continue
        raise UsageError(f"알 수 없는 옵션입니다: {name}")
    if kind not in DROP_PATTERNS:
        raise UsageError("--kind 는 node 또는 swift 여야 합니다.")
    if cwd is not None and not os.path.isdir(cwd):
        raise UsageError(f"--cwd 폴더가 없습니다: {cwd}")
    return cwd, kind, command


def make_line_filter(kind: str) -> Callable[[str], Optional[str]]:
    """줄을 받아 보여 줄 문자열을 돌려주고, 지울 줄이면 None을 돌려준다."""
    patterns = DROP_PATTERNS[kind]

    def line_filter(line: str) -> Optional[str]:
        cleaned = ANSI_PATTERN.sub("", line)
        if any(pattern.match(cleaned) for pattern in patterns):
            return None
        return cleaned

    return line_filter


class OutputPump:
    """자식 출력을 한 줄씩 원문 로그에 쓰고, 걸러서 화면에 쓴다."""

    def __init__(self, raw_log: Optional[BinaryIO], out: BinaryIO, line_filter: Optional[Callable[[str], Optional[str]]]):
        self.raw_log = raw_log
        self.out = out
        self.line_filter = line_filter
        self.total_lines = 0
        self.hidden_lines = 0
        self.filter_error: Optional[str] = None
        self.log_error: Optional[str] = None
        self.out_closed = False
        self.ends_with_newline = True
        self.pending = b""

    def write_out(self, data: bytes) -> None:
        if self.out_closed or not data:
            return
        try:
            self.out.write(data)
            self.out.flush()
            self.ends_with_newline = data.endswith(b"\n")
        except (BrokenPipeError, OSError):
            # 읽는 쪽이 사라져도 자식이 막히지 않도록 원문 로그 기록은 계속한다.
            self.out_closed = True

    def feed(self, raw: bytes) -> None:
        self.total_lines += 1
        if self.raw_log is not None and self.log_error is None:
            try:
                self.raw_log.write(raw)
            except OSError as error:
                self.log_error = str(error)
                self.fall_back(f"원문 로그를 더 쓰지 못해 이후는 원문을 그대로 출력합니다. {error}")
        if self.line_filter is None:
            self.write_out(raw)
            return
        try:
            newline = b"\r\n" if raw.endswith(b"\r\n") else b"\n" if raw.endswith(b"\n") else b""
            text = raw[: len(raw) - len(newline)].decode("utf-8", "replace")
            shown = self.line_filter(text)
        except Exception as error:  # 필터 결함이 출력을 삼키지 않게 원문으로 돌아간다.
            self.filter_error = f"{type(error).__name__}: {error}"
            self.fall_back(f"출력 필터 오류로 이후는 원문을 그대로 출력합니다. {self.filter_error}")
            self.write_out(raw)
            return
        if shown is None:
            self.hidden_lines += 1
        elif shown == text:
            self.write_out(raw)
        else:
            self.write_out(shown.encode("utf-8") + newline)

    def fall_back(self, message: str) -> None:
        self.line_filter = None
        self.notice(message)

    def notice(self, message: str) -> None:
        prefix = b"" if self.ends_with_newline else b"\n"
        self.write_out(prefix + f"[office-test] {message}\n".encode("utf-8"))

    def feed_chunk(self, data: bytes) -> None:
        self.pending += data
        while True:
            end = self.pending.find(b"\n")
            if end < 0:
                return
            line, self.pending = self.pending[: end + 1], self.pending[end + 1:]
            self.feed(line)

    def finish(self) -> None:
        if self.pending:
            line, self.pending = self.pending, b""
            self.feed(line)

    def pump(self, stream: BinaryIO) -> None:
        for data in iter(lambda: stream.read(CHUNK_BYTES), b""):
            self.feed_chunk(data)
        self.finish()

    def pump_pipe(self, descriptor: int, stop: threading.Event) -> None:
        """파이프를 EOF까지 읽는다. stop이 켜지면 곧바로 멈춰 호출한 쪽이 파이프를 닫을 수 있게 한다."""
        while not stop.is_set():
            readable, _, _ = select.select([descriptor], [], [], PIPE_POLL_SECONDS)
            if not readable:
                continue
            data = os.read(descriptor, CHUNK_BYTES)
            if not data:
                break
            self.feed_chunk(data)
        self.finish()


def open_raw_log() -> tuple[Optional[BinaryIO], Optional[str], Optional[str]]:
    try:
        # mkstemp는 파일을 0600으로 만든다.
        descriptor, path = tempfile.mkstemp(prefix="office-test-", suffix=".log")
        return os.fdopen(descriptor, "wb"), path, None
    except OSError as error:
        return None, None, str(error)


def signal_name(number: int) -> str:
    try:
        return signal.Signals(number).name
    except ValueError:
        return f"signal {number}"


def run(argv: list[str], out: BinaryIO, err: BinaryIO) -> int:
    try:
        cwd, kind, command = parse_arguments(argv)
    except UsageError as error:
        err.write(f"[office-test] {error}\n{USAGE}\n".encode("utf-8"))
        err.flush()
        return EXIT_USAGE

    raw_log, log_path, log_open_error = open_raw_log()
    line_filter = make_line_filter(kind) if raw_log is not None else None
    pump = OutputPump(raw_log, out, line_filter)
    if raw_log is None:
        pump.notice(f"원문 로그를 만들지 못해 필터 없이 원문을 그대로 출력합니다. {log_open_error}")

    received: list[int] = []
    cancelled_at: list[float] = []
    # 자식을 새 세션으로 띄우므로 묶음 번호는 자식 pid와 같다. 직계 자식이 먼저 끝나도
    # 묶음에 남은 하위 프로세스에는 이 번호로 신호가 간다.
    group: list[int] = []

    def kill_group(number: int) -> None:
        if not group:
            return
        try:
            os.killpg(group[0], number)
        except (ProcessLookupError, PermissionError):
            pass

    def group_alive() -> bool:
        try:
            os.killpg(group[0], 0)
            return True
        except ProcessLookupError:
            return False
        except PermissionError:
            return True

    def grace_expired() -> bool:
        return bool(cancelled_at) and time.monotonic() >= cancelled_at[0] + KILL_GRACE_SECONDS

    def forward(number: int, _frame: object) -> None:
        received.append(number)
        if len(received) == 1:
            cancelled_at.append(time.monotonic())
            kill_group(number)
        else:
            kill_group(signal.SIGKILL)

    # 자식을 띄우기 전에 등록해야 그 사이에 온 취소 신호도 놓치지 않는다.
    previous_handlers = {number: signal.signal(number, forward) for number in FORWARDED_SIGNALS}
    try:
        try:
            child = subprocess.Popen(
                command,
                cwd=cwd,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
        except OSError as error:
            if raw_log is not None:
                raw_log.close()
                os.unlink(log_path)
            code = EXIT_NOT_FOUND if isinstance(error, FileNotFoundError) else EXIT_NOT_EXECUTABLE
            err.write(f"[office-test] 명령을 실행하지 못했습니다: {command[0]} ({error.strerror or error})\n".encode("utf-8"))
            err.flush()
            return code
        group.append(child.pid)
        if received:
            kill_group(signal.SIGKILL if len(received) > 1 else received[0])
        stop_reading = threading.Event()
        reader = threading.Thread(target=pump.pump_pipe, args=(child.stdout.fileno(), stop_reading), daemon=True)
        reader.start()
        while True:
            try:
                returncode = child.wait(WAIT_POLL_SECONDS)
                break
            except subprocess.TimeoutExpired:
                if grace_expired():
                    kill_group(signal.SIGKILL)
        if received:
            # 취소했다면 직계 자식이 끝나도 묶음 전체가 끝날 때까지 정리한다. 취소하지 않은
            # 정상 종료에서는 남은 백그라운드 프로세스를 건드리지 않는다.
            while group_alive() and not grace_expired():
                time.sleep(WAIT_POLL_SECONDS)
            if group_alive():
                kill_group(signal.SIGKILL)
                deadline = time.monotonic() + GROUP_EXIT_SECONDS
                while group_alive() and time.monotonic() < deadline:
                    time.sleep(WAIT_POLL_SECONDS)
    finally:
        for number, handler in previous_handlers.items():
            signal.signal(number, handler)

    # 남은 출력을 마저 읽는다. 하위 프로세스가 파이프를 계속 잡고 있으면 읽기만 멈춘다.
    reader.join(READER_DRAIN_SECONDS)
    pipe_held = reader.is_alive()
    if pipe_held:
        stop_reading.set()
        reader.join()
    child.stdout.close()
    if raw_log is not None:
        try:
            raw_log.close()
        except OSError:
            pass
    if pipe_held:
        pump.notice("하위 프로세스가 출력 파이프를 닫지 않아 이후 출력은 읽지 않았습니다. 그 프로세스는 종료하지 않았습니다.")

    exit_code = returncode if returncode >= 0 else 128 - returncode
    if received:
        pump.notice(f"취소 신호 {signal_name(received[0])}를 자식 프로세스 묶음에 전달했습니다.")
    if log_path is not None:
        pump.notice(
            f"원문 로그 {log_path} · 전체 {pump.total_lines}줄 중 {pump.hidden_lines}줄 생략 · 종료 코드 {exit_code}"
        )
    return exit_code


def main() -> int:
    return run(sys.argv[1:], sys.stdout.buffer, sys.stderr.buffer)


if __name__ == "__main__":
    sys.exit(main())
