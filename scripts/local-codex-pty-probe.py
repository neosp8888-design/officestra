"""Bounded real PTY smoke test. Only the child CLI is signalled; no UI apps."""
import os, pty, sys, select, time, signal, struct, fcntl, termios, re

marker = sys.argv[1]
pid, fd = pty.fork()
if pid == 0:
    os.environ['TERM'] = 'xterm-256color'
    os.execvp(sys.argv[2], sys.argv[2:])
def stop(_signal, _frame):
    raise SystemExit(1)
signal.signal(signal.SIGTERM, stop)
signal.signal(signal.SIGINT, stop)
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 150, 0, 0))
deadline = time.monotonic() + 110
tail = b''
accepted = False
trust_at = None
try:
    while time.monotonic() < deadline:
        ready, _, _ = select.select([fd], [], [], 0.2)
        if ready:
            try: data = os.read(fd, 65536)
            except OSError: break
            if not data: break
            sys.stdout.buffer.write(data); sys.stdout.buffer.flush()
            tail = (tail + data)[-1024*1024:]
            if b'\x1b[6n' in tail:
                os.write(fd, b'\x1b[1;1R'); tail = tail.replace(b'\x1b[6n', b'')
            if b'\x1b]11;?' in tail:
                os.write(fd, b'\x1b]11;rgb:0000/0000/0000\x1b\\'); tail = tail.replace(b'\x1b]11;?', b'')
            if b'\x1b]10;?' in tail:
                os.write(fd, b'\x1b]10;rgb:ffff/ffff/ffff\x1b\\'); tail = tail.replace(b'\x1b]10;?', b'')
            plain = re.sub(rb'\x1b\[[0-?]*[ -/]*[@-~]', b'', tail)
            if not accepted and (b'Yes, continue' in plain or b'Yes,continue' in plain):
                if trust_at is None: trust_at = time.monotonic() + 3
        if not accepted and trust_at is not None and time.monotonic() >= trust_at:
            os.write(fd, b'1\r'); accepted = True
        if os.path.exists(marker):
            # Completion was delivered by Codex's native notify, not guessed
            # from terminal paint output. Close only this isolated CLI.
            # A completed CLI can stop draining PTY input. Do not block on a
            # control-character write; the owned-child cleanup below is bounded.
            break
finally:
    try: os.kill(pid, signal.SIGTERM)
    except ProcessLookupError: pass
    end = time.monotonic() + 5
    while time.monotonic() < end:
        done, _ = os.waitpid(pid, os.WNOHANG)
        if done: break
        time.sleep(0.1)
    else:
        os.kill(pid, signal.SIGKILL); os.waitpid(pid, 0)
    os.close(fd)
sys.exit(0 if os.path.exists(marker) else 1)
