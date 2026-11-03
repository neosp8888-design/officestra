// 음성 문장 나누기·따라 읽기 제어·잠금을 확인하는 단위 시험이다. Node voice-speakable·voice-follow-control 시험을 옮겼다.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use serde_json::json;

use super::follow_lock::acquire_follow_lock;
use super::speakable::{
    DIRECTION_PAUSE_MS, Directions, PARAGRAPH_BREAK, SentenceStream, parse_directions, speakable_text, split_sentences,
};
use super::{FollowStatus, FollowerEvent, FollowerEvents, FollowerHandle, SpawnFollower, VoiceFollowControl};
use crate::js::join_path;

fn directions(text: &str, style: Option<&'static str>, pause_ms: u64) -> Directions {
    Directions { text: text.into(), style, pause_ms }
}

fn texts(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_string()).collect()
}

#[test]
fn markdown_symbols_links_and_paths_become_speakable_text() {
    assert_eq!(speakable_text("## **결론** 먼저 말씀드릴게요", true), "결론 먼저 말씀드릴게요");
    assert_eq!(speakable_text("- [문서](https://a.b/c) 확인", true), "문서 확인");
    assert_eq!(
        speakable_text("`Sources/OfficeGame/StreamingPlainTextView.swift` 수정", true),
        "StreamingPlainTextView.swift 수정"
    );
    assert_eq!(speakable_text("참고 https://example.com/x?y=1 입니다", true), "참고 링크 입니다");
}

#[test]
fn short_fragments_join_the_next_sentence() {
    assert_eq!(split_sentences("네. 확인했습니다. 바로 진행할게요!"), texts(&["네. 확인했습니다.", "바로 진행할게요!"]));
}

#[test]
fn finished_sentences_are_emitted_before_the_line_ends() {
    let mut stream = SentenceStream::new(false);
    assert_eq!(stream.push("첫 문장입니다. 두 번"), texts(&["첫 문장입니다."]));
    assert_eq!(stream.push("첫 문장입니다. 두 번째 문장도 끝났어요. 세"), texts(&["두 번째 문장도 끝났어요."]));
    assert_eq!(stream.finish(), texts(&["세"]));
}

#[test]
fn code_blocks_and_tables_are_not_read() {
    let mut stream = SentenceStream::new(false);
    let text = "설명입니다.\n```js\nconst a = 1. b\n```\n| 항목 | 값 |\n| --- | --- |\n---\n끝입니다.\n";
    // JS text.slice(0, 20)은 UTF-16 20단위다.
    let head = String::from_utf16(&text.encode_utf16().take(20).collect::<Vec<_>>()).unwrap();
    let mut spoken = stream.push(&head);
    spoken.extend(stream.push(text));
    spoken.extend(stream.finish());
    assert_eq!(spoken, texts(&["설명입니다.", "끝입니다."]));
}

#[test]
fn character_by_character_input_emits_each_sentence_once() {
    let mut stream = SentenceStream::new(false);
    let text = "## 보고\n작업을 마쳤습니다. 테스트도 통과했어요!\n- 수정 파일은 `a/b/c.swift` 입니다.\n";
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut spoken = Vec::new();
    for end in 1..=units.len() {
        // 한 글자씩 자라는 JS 문자열처럼 UTF-16 앞부분을 넘긴다. 이 글은 모두 BMP 글자다.
        spoken.extend(stream.push(&String::from_utf16(&units[..end]).unwrap()));
    }
    spoken.extend(stream.finish());
    assert_eq!(spoken, texts(&["보고", "작업을 마쳤습니다.", "테스트도 통과했어요!", "수정 파일은 c.swift 입니다."]));
}

#[test]
fn bracket_directions_become_style_and_pause() {
    assert_eq!(
        parse_directions("(잠시 침묵하다가) 몸으로 나누는 대화라..."),
        directions("몸으로 나누는 대화라...", None, DIRECTION_PAUSE_MS)
    );
    assert_eq!(parse_directions("(귓가에 속삭이며) 이건 비밀이야."), directions("이건 비밀이야.", Some("whisper"), 0));
    assert_eq!(parse_directions("아, 정말요? (장난스럽게 넘기기)"), directions("아, 정말요?", Some("playful"), 0));
    assert_eq!(parse_directions("수정 파일（Swift）입니다."), directions("수정 파일 입니다.", None, 0));
    // 괄호 안에서 문장이 끊긴 경우
    assert_eq!(parse_directions("좋아요 (웃으며"), directions("좋아요", Some("playful"), 0));
    assert_eq!(parse_directions("한숨) 그래도 해 볼게요."), directions("그래도 해 볼게요.", None, DIRECTION_PAUSE_MS));
    assert_eq!(parse_directions("(웃음)"), directions("", Some("playful"), 0));
}

#[test]
fn paragraph_breaks_are_inserted_for_blank_lines_when_enabled() {
    let text = "(울먹이며) 힘든 하루였어요. 그래도 고마워요.\n둘째 줄이에요.\n\n새 문단이에요.\n```\n\n```\n끝.\n";
    let mut with_breaks = SentenceStream::new(true);
    let mut spoken = with_breaks.push(text);
    spoken.extend(with_breaks.finish());
    assert_eq!(
        spoken,
        texts(&["(울먹이며) 힘든 하루였어요.", "그래도 고마워요.", "둘째 줄이에요.", PARAGRAPH_BREAK, "새 문단이에요.", "끝."])
    );
    let mut plain = SentenceStream::new(false);
    let mut spoken = plain.push(text);
    spoken.extend(plain.finish());
    assert!(!spoken.iter().any(|sentence| sentence == PARAGRAPH_BREAK));
}

#[test]
fn emotion_words_in_directions_pick_the_reference_voice() {
    let style_of = |sentence: &str| parse_directions(sentence).style;
    assert_eq!(style_of("(울먹이며) 미안해요."), Some("sad"));
    assert_eq!(style_of("(배를 움켜쥐고 아파하며) 괜찮아요."), Some("pain"));
    assert_eq!(style_of("(화가 나서) 그만하세요."), Some("angry"));
    assert_eq!(style_of("(들뜬 목소리로) 정말요?"), Some("excited"));
    assert_eq!(style_of("(살짝 질투하며) 그 사람이 좋아요?"), Some("jealous"));
    // 감정과 속삭임이 섞이면 감정을 먼저 본다.
    assert_eq!(style_of("(울먹이며 작게 속삭이듯) 보고 싶었어."), Some("sad"));
    // 한숨은 쉼으로, 감정 낱말이 없으면 말투는 그대로 둔다.
    assert_eq!(parse_directions("(한숨을 쉬며) 알겠어요."), directions("알겠어요.", None, DIRECTION_PAUSE_MS));
}

/// 가짜 자식. 띄운 직원과 받은 종료 신호, 사건 보내기 함수를 남긴다.
#[derive(Clone)]
struct FakeChild {
    character_id: String,
    killed: Arc<Mutex<Option<&'static str>>>,
    events: FollowerEvents,
}

impl FakeChild {
    fn emit(&self, event: FollowerEvent) {
        (self.events)(event);
    }

    fn killed(&self) -> Option<&'static str> {
        *self.killed.lock().unwrap()
    }
}

struct FakeHandle(Arc<Mutex<Option<&'static str>>>);

impl FollowerHandle for FakeHandle {
    fn kill(&mut self) {
        *self.0.lock().unwrap() = Some("SIGTERM");
    }
}

type Children = Arc<Mutex<Vec<FakeChild>>>;

fn fake_spawner() -> (Children, SpawnFollower) {
    let children: Children = Arc::default();
    let recorded = Arc::clone(&children);
    let spawn: SpawnFollower = Arc::new(move |character_id, events| {
        let killed = Arc::new(Mutex::new(None));
        recorded.lock().unwrap().push(FakeChild { character_id: character_id.into(), killed: Arc::clone(&killed), events });
        Ok(Box::new(FakeHandle(killed)) as Box<dyn FollowerHandle>)
    });
    (children, spawn)
}

fn child(children: &Children, index: usize) -> FakeChild {
    children.lock().unwrap()[index].clone()
}

fn enable(control: &VoiceFollowControl, character_id: &str) -> Result<FollowStatus, String> {
    control.set(Some(&json!(true)), Some(&json!(character_id)))
}

fn status(character_id: Option<&str>, state: &'static str, error: Option<&str>) -> FollowStatus {
    FollowStatus { character_id: character_id.map(Into::into), state, error: error.map(Into::into) }
}

#[test]
fn enabling_is_starting_until_ready_then_running() {
    let (children, spawn) = fake_spawner();
    let control = VoiceFollowControl::new(spawn);
    assert_eq!(control.status(), status(None, "stopped", None));
    assert_eq!(serde_json::to_value(control.status()).unwrap(), json!({ "characterId": null, "state": "stopped", "error": null }));
    assert_eq!(enable(&control, "right-woman").unwrap().state, "starting");
    // 준비 신호가 아닌 메시지는 상태를 바꾸지 않는다.
    child(&children, 0).emit(FollowerEvent::Message(json!({ "ready": false })));
    assert_eq!(control.status().state, "starting");
    child(&children, 0).emit(FollowerEvent::Message(json!({ "ready": true })));
    assert_eq!(control.status(), status(Some("right-woman"), "running", None));
    // 같은 직원으로 다시 켜도 새로 띄우지 않는다.
    enable(&control, "right-woman").unwrap();
    assert_eq!(children.lock().unwrap().len(), 1);
    assert_eq!(child(&children, 0).character_id, "right-woman");
}

#[test]
fn disabling_kills_the_child_and_its_later_exit_is_not_a_failure() {
    let (children, spawn) = fake_spawner();
    let control = VoiceFollowControl::new(spawn);
    enable(&control, "right-woman").unwrap();
    assert_eq!(control.set(Some(&json!(false)), None).unwrap().state, "stopped");
    assert_eq!(child(&children, 0).killed(), Some("SIGTERM"));
    child(&children, 0).emit(FollowerEvent::Exit(None));
    assert_eq!(control.status(), status(None, "stopped", None));
    // 백엔드 종료 정리도 같은 멈춤이다.
    enable(&control, "right-woman").unwrap();
    control.stop();
    assert_eq!(child(&children, 1).killed(), Some("SIGTERM"));
    assert_eq!(control.status(), status(None, "stopped", None));
}

#[test]
fn switching_characters_stops_the_previous_follower() {
    let (children, spawn) = fake_spawner();
    let control = VoiceFollowControl::new(spawn);
    enable(&control, "right-woman").unwrap();
    enable(&control, "left-man").unwrap();
    assert_eq!(child(&children, 0).killed(), Some("SIGTERM"));
    assert_eq!(child(&children, 1).character_id, "left-man");
    child(&children, 0).emit(FollowerEvent::Exit(None));
    // 멈춘 자식의 늦은 준비 신호도 버린다.
    child(&children, 0).emit(FollowerEvent::Message(json!({ "ready": true })));
    assert_eq!(control.status(), status(Some("left-man"), "starting", None));
}

#[test]
fn unexpected_exits_and_spawn_errors_are_failed_with_a_reason() {
    let (children, spawn) = fake_spawner();
    let control = VoiceFollowControl::new(spawn);
    let home = super::voice_home_directory();
    let log = join_path(&[&home, "logs", "follow.log"]);
    let venv = join_path(&[&home, ".venv"]);
    enable(&control, "right-woman").unwrap();
    child(&children, 0).emit(FollowerEvent::Exit(Some(2)));
    assert_eq!(control.status().state, "failed");
    assert_eq!(control.status().error.unwrap(), format!("음성 파이썬 환경이 없습니다 ({venv})."));
    enable(&control, "right-woman").unwrap();
    child(&children, 1).emit(FollowerEvent::Exit(Some(4)));
    assert_eq!(control.status().error.unwrap(), format!("음성 모델 초기화 또는 예열에 실패했습니다. 기록은 {log}에 있습니다."));
    enable(&control, "right-woman").unwrap();
    child(&children, 2).emit(FollowerEvent::Error("spawn failed".into()));
    assert_eq!(control.status(), status(Some("right-woman"), "failed", Some("spawn failed")));
    enable(&control, "right-woman").unwrap();
    child(&children, 3).emit(FollowerEvent::Exit(Some(3)));
    assert_eq!(
        control.status().error.unwrap(),
        "음성 따라 읽기가 이미 다른 프로세스에서 실행 중입니다. 기존 실행을 종료한 뒤 다시 켜 주세요."
    );
    enable(&control, "right-woman").unwrap();
    child(&children, 4).emit(FollowerEvent::Exit(Some(7)));
    assert_eq!(control.status().error.unwrap(), format!("음성 따라 읽기가 종료되었습니다 (코드 7). 기록은 {log}에 있습니다."));
    // 신호로 끝나 종료 코드가 없으면 Node처럼 null로 적는다.
    enable(&control, "right-woman").unwrap();
    child(&children, 5).emit(FollowerEvent::Exit(None));
    assert_eq!(control.status().error.unwrap(), format!("음성 따라 읽기가 종료되었습니다 (코드 null). 기록은 {log}에 있습니다."));
    // 실패한 자식이 없으니 같은 직원으로 다시 켜면 새로 띄운다.
    assert_eq!(children.lock().unwrap().len(), 6);
}

#[test]
fn invalid_requests_are_rejected() {
    let (children, spawn) = fake_spawner();
    let control = VoiceFollowControl::new(spawn);
    assert_eq!(control.set(Some(&json!("yes")), None).unwrap_err(), "enabled must be a boolean");
    assert_eq!(control.set(None, Some(&json!("right-woman"))).unwrap_err(), "enabled must be a boolean");
    assert_eq!(enable(&control, "../x").unwrap_err(), "characterId is required");
    assert_eq!(control.set(Some(&json!(true)), Some(&json!(7))).unwrap_err(), "characterId is required");
    assert_eq!(control.set(Some(&json!(true)), None).unwrap_err(), "characterId is required");
    // 끌 때는 Node처럼 직원 ID를 보지 않는다.
    assert_eq!(control.set(Some(&json!(false)), Some(&json!("../x"))).unwrap(), status(None, "stopped", None));
    assert!(children.lock().unwrap().is_empty());
}

#[test]
fn a_synchronous_spawn_failure_leaves_the_control_stopped() {
    let (children, spawn) = fake_spawner();
    let failing = Arc::new(Mutex::new(false));
    let fail = Arc::clone(&failing);
    let control = VoiceFollowControl::new(Arc::new(move |character_id, events| {
        if *fail.lock().unwrap() {
            return Err("EPERM: operation not permitted, mkdir '/x/logs'".into());
        }
        spawn(character_id, events)
    }));
    enable(&control, "right-woman").unwrap();
    *failing.lock().unwrap() = true;
    assert_eq!(enable(&control, "left-man").unwrap_err(), "EPERM: operation not permitted, mkdir '/x/logs'");
    assert_eq!(child(&children, 0).killed(), Some("SIGTERM"));
    assert_eq!(control.status(), status(None, "stopped", None));
    // 앞 자식의 늦은 사건은 버린다.
    child(&children, 0).emit(FollowerEvent::Exit(Some(1)));
    assert_eq!(control.status(), status(None, "stopped", None));
}

/// 시험마다 따로 쓰는 음성 폴더. 끝나면 지운다.
struct TempHome(String);

impl TempHome {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "officestra-voice-lock-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path.to_string_lossy().into_owned())
    }

    fn lock_directory(&self) -> String {
        join_path(&[&self.0, "active-follow.lock"])
    }

    fn owner_file(&self) -> String {
        join_path(&[&self.lock_directory(), "pid"])
    }

    /// 다른 프로세스가 남긴 잠금 폴더. owner가 None이면 pid 파일을 만들지 않는다.
    fn leave_lock(&self, owner: Option<&str>) {
        std::fs::create_dir(self.lock_directory()).unwrap();
        if let Some(owner) = owner {
            std::fs::write(self.owner_file(), owner).unwrap();
        }
    }

    fn owner(&self) -> Option<String> {
        std::fs::read_to_string(self.owner_file()).ok()
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 끝나서 거둔 프로세스 번호. 곧바로 다른 프로세스에 다시 쓰일 가능성은 낮다.
fn dead_pid() -> u32 {
    let mut child = std::process::Command::new("/usr/bin/true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

#[test]
fn a_live_owner_blocks_and_release_removes_the_lock() {
    let home = TempHome::new("own");
    let pid = std::process::id();
    let lock = acquire_follow_lock(&home.0).unwrap();
    assert_eq!(home.owner().as_deref(), Some(format!("{pid}\n").as_str()));
    assert_eq!(acquire_follow_lock(&home.0).unwrap_err(), format!("음성 따라 읽기가 이미 실행 중입니다 (PID {pid})."));
    drop(lock);
    assert!(!std::path::Path::new(&home.lock_directory()).exists());
    let mut again = acquire_follow_lock(&home.0).unwrap();
    again.release();
    assert!(!std::path::Path::new(&home.lock_directory()).exists());
}

#[test]
fn owner_probe_errors_are_reported_like_node() {
    let home = TempHome::new("eperm");
    home.leave_lock(Some("1\n"));
    assert_eq!(acquire_follow_lock(&home.0).unwrap_err(), "kill EPERM");
    let home = TempHome::new("huge");
    home.leave_lock(Some("99999999999\n"));
    assert_eq!(
        acquire_follow_lock(&home.0).unwrap_err(),
        "The \"pid\" argument must be of type number. Received type number (99999999999)"
    );
}

#[test]
fn a_lock_without_a_recorded_owner_is_left_alone_while_fresh() {
    let home = TempHome::new("fresh");
    home.leave_lock(None);
    assert_eq!(acquire_follow_lock(&home.0).unwrap_err(), "음성 따라 읽기가 이미 시작 중입니다.");
    let home = TempHome::new("garbage");
    home.leave_lock(Some("abc"));
    assert_eq!(acquire_follow_lock(&home.0).unwrap_err(), "음성 따라 읽기가 이미 시작 중입니다.");
}

#[test]
fn stale_and_dead_locks_are_taken_over() {
    let pid = std::process::id();
    let home = TempHome::new("stale");
    home.leave_lock(None);
    let old = SystemTime::now() - Duration::from_secs(60);
    std::fs::File::open(home.lock_directory()).unwrap().set_modified(old).unwrap();
    let lock = acquire_follow_lock(&home.0).unwrap();
    assert_eq!(home.owner().as_deref(), Some(format!("{pid}\n").as_str()));
    drop(lock);
    assert!(!std::path::Path::new(&home.lock_directory()).exists());
    let home = TempHome::new("dead");
    home.leave_lock(Some(&format!("{}\n", dead_pid())));
    let lock = acquire_follow_lock(&home.0).unwrap();
    assert_eq!(home.owner().as_deref(), Some(format!("{pid}\n").as_str()));
    drop(lock);
}

#[test]
fn release_keeps_a_lock_that_changed_owner() {
    let home = TempHome::new("changed");
    let lock = acquire_follow_lock(&home.0).unwrap();
    std::fs::write(home.owner_file(), "1\n").unwrap();
    drop(lock);
    assert_eq!(home.owner().as_deref(), Some("1\n"));
}

#[test]
fn lock_errors_on_the_home_folder_use_node_fs_messages() {
    let home = TempHome::new("file");
    let blocked = join_path(&[&home.0, "not-a-folder"]);
    std::fs::write(&blocked, "").unwrap();
    assert_eq!(
        acquire_follow_lock(&blocked).unwrap_err(),
        format!("EEXIST: file already exists, mkdir '{blocked}'")
    );
}
