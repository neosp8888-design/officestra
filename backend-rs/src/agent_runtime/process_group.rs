// 직원 CLI를 새 세션으로 띄우고 프로세스 그룹째 끝내는 도우미. Node spawn(detached)·terminateProcessGroup·terminateChild와 같다.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::launch::Environment;

/// spawn(executable, args, { cwd, env, shell: false, detached: true }). Node처럼 환경을 통째로 바꾸고 setsid로 새 세션을 연다.
pub fn detached_command(executable: &str, arguments: &[String], cwd: Option<&str>, env: &Environment) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(executable);
    command.args(arguments).env_clear().envs(env);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    // SAFETY: fork 뒤 exec 전에 비동기 신호 안전 함수인 setsid만 부른다.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
}

/// 띄운 자식의 번호와 종료 상태. Node `child.exitCode !== null`처럼 신호로 끝난 경우는 종료 코드가 없는 것으로 본다.
#[derive(Clone)]
pub struct ProcessGroup {
    pid: Option<i32>,
    exit_code_known: Arc<AtomicBool>,
    reaped: Arc<AtomicBool>,
}

impl ProcessGroup {
    pub fn new(pid: Option<u32>) -> Self {
        Self {
            pid: pid.and_then(|pid| i32::try_from(pid).ok()),
            exit_code_known: Arc::default(),
            reaped: Arc::default(),
        }
    }

    pub fn pid(&self) -> Option<i32> {
        self.pid
    }

    /// 자식을 거둔 뒤 부른다. 이후로는 같은 번호를 다른 프로세스가 받을 수 있으므로 자식 하나에는 신호를 보내지 않는다.
    pub fn record_exit(&self, status: &std::process::ExitStatus) {
        self.reaped.store(true, Ordering::SeqCst);
        if status.code().is_some() {
            self.exit_code_known.store(true, Ordering::SeqCst);
        }
    }

    pub fn has_exit_code(&self) -> bool {
        self.exit_code_known.load(Ordering::SeqCst)
    }

    /// 그룹에 SIGTERM을 보내고 `force_after` 뒤에도 종료 코드가 없으면 SIGKILL을 보낸다.
    /// `fallback_to_process`는 Claude 지속 세션의 terminateChild처럼 그룹 신호가 실패하면 자식 하나에 다시 보내는 방식이다.
    /// 단일 실행의 terminateProcessGroup은 그룹 신호가 실패하면 그대로 멈춘다.
    pub fn terminate(&self, force_after: Duration, fallback_to_process: bool) {
        let Some(pid) = self.pid.filter(|_| !self.has_exit_code()) else {
            return;
        };
        if !signal_group(pid, libc::SIGTERM) {
            if !fallback_to_process {
                return;
            }
            self.signal_child(pid, libc::SIGTERM);
        }
        let group = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(force_after).await;
            if group.has_exit_code() {
                return;
            }
            if !signal_group(pid, libc::SIGKILL) && fallback_to_process {
                group.signal_child(pid, libc::SIGKILL);
            }
        });
    }

    fn signal_child(&self, pid: i32, signal: i32) {
        if !self.reaped.load(Ordering::SeqCst) {
            // SAFETY: 아직 거두지 않은 자식 번호에 신호만 보낸다.
            unsafe { libc::kill(pid, signal) };
        }
    }
}

fn signal_group(pid: i32, signal: i32) -> bool {
    // SAFETY: setsid로 만든 세션의 그룹 번호(자식 번호와 같다)에 신호만 보낸다.
    unsafe { libc::kill(-pid, signal) == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn 새_세션으로_띄워_그룹째_끝낸다() {
        let environment = Environment::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]);
        let mut child = detached_command("/bin/sh", &["-c".into(), "sleep 30 & wait".into()], Some("/"), &environment)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let group = ProcessGroup::new(child.id());
        let pid = group.pid().unwrap();
        // SAFETY: 방금 띄운 자식의 세션 번호만 읽는다.
        assert_eq!(unsafe { libc::getsid(pid) }, pid, "자식이 새 세션의 대표다");
        group.terminate(Duration::from_millis(200), false);
        let status = tokio::time::timeout(Duration::from_secs(5), child.wait()).await.unwrap().unwrap();
        group.record_exit(&status);
        assert_eq!(std::os::unix::process::ExitStatusExt::signal(&status), Some(libc::SIGTERM));
        assert!(!group.has_exit_code(), "신호로 끝나면 종료 코드가 없다");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!signal_group(pid, 0), "손자 sleep까지 그룹이 모두 끝났다");
    }
}
