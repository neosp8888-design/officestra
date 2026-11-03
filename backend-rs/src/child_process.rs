// Node child_process.execFile을 흉내 낸 자식 프로세스 실행기. 모델 목록 수집과 CLI 갱신이 같이 쓴다.

use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStderr, ChildStdout};

use crate::js::uv_error;
use crate::local_artifacts::home_directory;

/// Node execFile의 기본 maxBuffer(1MiB).
pub const DEFAULT_MAX_BUFFER: usize = 1_024 * 1_024;

/// execFile이 성공했을 때의 { stdout, stderr }. 둘 다 UTF-8로 풀고 잘못된 바이트는 U+FFFD로 바꾼다.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExecFileOutput {
    pub stdout: String,
    pub stderr: String,
}

/// promisify(execFile). 작업 폴더는 홈, 환경은 그대로 물려준다. 입력이 있으면 쓰고 닫고,
/// 없으면 Node처럼 stdin 파이프를 열어 둔다. 시간 초과·출력 한도를 넘으면 SIGTERM을 보내고 끝나기를 기다린다.
/// 종료 코드가 0이면 시간 초과로 끊었어도 Node처럼 성공이다. 실패 문구는 Node 오류의 message와 같다.
pub async fn exec_file(
    executable: &str,
    arguments: &[&str],
    input: Option<&str>,
    timeout: Duration,
    max_bytes: usize,
) -> Result<ExecFileOutput, String> {
    let mut child = tokio::process::Command::new(executable)
        .args(arguments)
        .current_dir(home_directory())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| spawn_error_message(executable, &error))?;
    let pid = child.id();
    let mut stdin = child.stdin.take();
    if let Some(input) = input
        && let Some(mut pipe) = stdin.take()
    {
        // Node도 stdin 쓰기 오류(EPIPE)를 무시한다. 다 쓰면 닫는다(stdin.end).
        let _ = pipe.write_all(input.as_bytes()).await;
    }
    let (mut stdout, mut stderr) = (child.stdout.take(), child.stderr.take());
    // Node execFile의 kill()은 stdout·stderr를 destroy한 뒤 SIGTERM을 보낸다. 읽기 끝이 닫히므로
    // 신호를 받은 뒤 더 쓰는 자식은 SIGPIPE로 끝나 실패가 된다. 여기서도 두 파이프를 버려 닫는다.
    let terminate = |stdout: &mut Option<ChildStdout>, stderr: &mut Option<ChildStderr>| {
        (*stdout, *stderr) = (None, None);
        if let Some(pid) = pid.and_then(|pid| i32::try_from(pid).ok()) {
            // SAFETY: 아직 거두지 않은 자식 프로세스 번호에 신호만 보낸다.
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
    };
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    let (mut output, mut errors) = (Vec::new(), Vec::new());
    let (mut output_buffer, mut error_buffer) = ([0_u8; 8_192], [0_u8; 8_192]);
    let mut killed = false;
    let mut limit_error: Option<&str> = None;
    let status = loop {
        tokio::select! {
            read = read_open_pipe(&mut stdout, &mut output_buffer), if stdout.is_some() => match read {
                Ok(0) | Err(_) => stdout = None,
                Ok(count) => {
                    output.extend_from_slice(&output_buffer[..count]);
                    if output.len() > max_bytes {
                        output.truncate(max_bytes);
                        limit_error = Some("stdout maxBuffer length exceeded");
                        killed = true;
                        terminate(&mut stdout, &mut stderr);
                    }
                }
            },
            read = read_open_pipe(&mut stderr, &mut error_buffer), if stderr.is_some() => match read {
                Ok(0) | Err(_) => stderr = None,
                Ok(count) => {
                    errors.extend_from_slice(&error_buffer[..count]);
                    if errors.len() > max_bytes {
                        errors.truncate(max_bytes);
                        limit_error = Some("stderr maxBuffer length exceeded");
                        killed = true;
                        terminate(&mut stdout, &mut stderr);
                    }
                }
            },
            status = child.wait(), if stdout.is_none() && stderr.is_none() => break status,
            () = &mut deadline, if !killed => {
                killed = true;
                terminate(&mut stdout, &mut stderr);
            }
        }
    };
    drop(stdin);
    if let Some(limit_error) = limit_error {
        return Err(limit_error.into());
    }
    let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
    match status {
        Ok(status) if status.code() == Some(0) => Ok(ExecFileOutput { stdout: text(&output), stderr: text(&errors) }),
        Ok(_) => {
            let command = std::iter::once(executable).chain(arguments.iter().copied()).collect::<Vec<_>>().join(" ");
            Err(format!("Command failed: {command}\n{}", text(&errors)))
        }
        Err(error) => Err(error.to_string()),
    }
}

/// Node ChildProcess.spawn은 아래 다섯 오류만 'error' 이벤트로 넘겨 실행 파일 경로를 붙이고,
/// 나머지는 바로 던져 "spawn 코드"만 남긴다(Node 24 실측: ENOTDIR·ENAMETOOLONG).
pub(crate) fn spawn_error_message(executable: &str, error: &std::io::Error) -> String {
    let code = uv_error(error).0;
    match code {
        "EACCES" | "EAGAIN" | "EMFILE" | "ENFILE" | "ENOENT" => format!("spawn {executable} {code}"),
        _ => format!("spawn {code}"),
    }
}

/// 열려 있는 파이프에서 한 번 읽는다. select!는 막힌 분기의 식도 만들므로 닫힌 파이프는 영영 기다리게 둔다.
pub(crate) async fn read_open_pipe(pipe: &mut Option<impl AsyncRead + Unpin>, buffer: &mut [u8]) -> std::io::Result<usize> {
    match pipe {
        Some(pipe) => pipe.read(buffer).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn success_keeps_stderr_and_runs_in_home() {
        let second = Duration::from_secs(5);
        assert_eq!(
            exec_file("/bin/sh", &["-c", "echo out; echo warn >&2"], None, second, DEFAULT_MAX_BUFFER).await,
            Ok(ExecFileOutput { stdout: "out\n".into(), stderr: "warn\n".into() })
        );
        assert_eq!(
            exec_file("/bin/pwd", &[], None, second, DEFAULT_MAX_BUFFER).await.map(|output| output.stdout),
            Ok(format!("{}\n", home_directory()))
        );
    }
}
