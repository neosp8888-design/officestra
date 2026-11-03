// 수동 시제품과 앱 버튼이 같은 응답을 동시에 읽지 않도록 프로세스 간 소유권을 잡는다. Node voice/follow-lock.mjs와 같은 잠금 폴더 형식이라 서로 막는다.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;

use super::fs_message;
use crate::js::{join_path, now_milliseconds, number_to_string, string_to_number};
use crate::local_artifacts::modified_milliseconds;

/// 잡은 잠금. 놓을 때 pid 파일이 아직 내 번호일 때만 폴더를 지운다.
#[derive(Debug)]
pub struct FollowLock {
    directory: String,
    owner_file: String,
    pid: u32,
    released: bool,
}

impl FollowLock {
    pub fn release(&mut self) {
        if std::mem::replace(&mut self.released, true) {
            return;
        }
        // 이미 정리됐거나 소유권이 바뀌었으면 그대로 둔다.
        if read_owner(&self.owner_file) == Some(f64::from(self.pid)) {
            let _ = remove_tree(&self.directory);
        }
    }
}

impl Drop for FollowLock {
    fn drop(&mut self) {
        self.release();
    }
}

/// Number(readFileSync(file, 'utf8')). 읽지 못하면 None(Node의 null)이다.
fn read_owner(path: &str) -> Option<f64> {
    fs::read(path).ok().map(|bytes| string_to_number(&String::from_utf8_lossy(&bytes)))
}

/// rmSync(path, { recursive: true, force: true }). 없으면 조용히 넘어간다.
fn remove_tree(path: &str) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
        Ok(metadata) if metadata.is_dir() => match fs::remove_dir_all(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        },
        Ok(_) => fs::remove_file(path),
    }
}

/// process.kill(owner, 0)이 던지는 오류 코드와 문구. 살아 있으면 None이다.
fn probe(owner: f64) -> Option<(&'static str, String)> {
    // Node는 32비트 정수가 아닌 pid를 형식 오류로 던진다.
    let Ok(pid) = i32::try_from(owner as i64) else {
        let shown = number_to_string(owner);
        return Some(("ERR_INVALID_ARG_TYPE", format!("The \"pid\" argument must be of type number. Received type number ({shown})")));
    };
    // SAFETY: 신호 0은 보내지 않고 존재와 권한만 확인한다.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return None;
    }
    let error = io::Error::last_os_error();
    Some(match error.raw_os_error() {
        Some(libc::ESRCH) => ("ESRCH", "kill ESRCH".into()),
        Some(libc::EPERM) => ("EPERM", "kill EPERM".into()),
        _ => ("UNKNOWN", "kill UNKNOWN".into()),
    })
}

/// acquireFollowLock(home). 실패 문구는 Node와 같다.
pub fn acquire_follow_lock(home: &str) -> Result<FollowLock, String> {
    fs::create_dir_all(home).map_err(|error| fs_message(&error, "mkdir", home))?;
    let directory = join_path(&[home, "active-follow.lock"]);
    let owner_file = join_path(&[&directory, "pid"]);
    let pid = std::process::id();
    for _ in 0..2 {
        match fs::create_dir(&directory) {
            Ok(()) => {
                let written = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o666)
                    .open(&owner_file)
                    .and_then(|mut file| file.write_all(format!("{pid}\n").as_bytes()));
                match written {
                    Ok(()) => return Ok(FollowLock { directory, owner_file, pid, released: false }),
                    Err(error) if error.raw_os_error() == Some(libc::EEXIST) => {}
                    Err(error) => return Err(fs_message(&error, "open", &owner_file)),
                }
            }
            Err(error) if error.raw_os_error() == Some(libc::EEXIST) => {}
            Err(error) => return Err(fs_message(&error, "mkdir", &directory)),
        }
        let owner = read_owner(&owner_file);
        match owner {
            Some(owner) if owner > 0.0 && owner.fract() == 0.0 && owner <= 9_007_199_254_740_991.0 => match probe(owner) {
                None => return Err(format!("음성 따라 읽기가 이미 실행 중입니다 (PID {}).", number_to_string(owner))),
                Some(("ESRCH", _)) => {}
                Some((_, message)) => return Err(message),
            },
            // 방금 잠금을 만든 프로세스가 아직 PID를 기록하는 동안은 지우지 않는다.
            _ => match fs::metadata(&directory) {
                Ok(metadata) if now_milliseconds() - modified_milliseconds(&metadata) < 10_000.0 => {
                    return Err("음성 따라 읽기가 이미 시작 중입니다.".into());
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(fs_message(&error, "stat", &directory)),
            },
        }
        remove_tree(&directory).map_err(|error| fs_message(&error, "rm", &directory))?;
    }
    Err("음성 따라 읽기 잠금을 얻지 못했습니다.".into())
}
