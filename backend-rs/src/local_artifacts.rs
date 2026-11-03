// CLI가 만든 로컬 이미지를 찾아 Markdown 응답에 미리보기로 붙인다. Node의 local-artifacts.mjs를 옮겼다.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::sync::{LazyLock, Mutex};

use sha2::{Digest, Sha256};

use crate::js::{decode_uri_component, extname, join_path, js_compare, js_trim, normalize_path, now_milliseconds};

const IMAGE_EXTENSIONS: [&str; 9] = [".bmp", ".gif", ".heic", ".jpeg", ".jpg", ".png", ".tif", ".tiff", ".webp"];
const IMAGE_IDENTITY_CACHE_LIMIT: usize = 256;

#[derive(Default)]
struct IdentityCache {
    values: HashMap<String, String>,
    order: VecDeque<String>,
}

static IMAGE_IDENTITY_CACHE: LazyLock<Mutex<IdentityCache>> = LazyLock::new(Mutex::default);

/// 미리보기는 선택적인 보강 정보다. 외장 디스크 권한·분리·파일 이동으로 읽을 수 없어도
/// DB 대화와 턴 기록은 그대로 돌려준다. Node의 EPERM·ENOENT·EIO·ENXIO·EACCES·ENOTDIR·ESTALE.
fn is_unavailable(error: &io::Error) -> bool {
    const ESTALE: i32 = if cfg!(target_os = "macos") { 70 } else { 116 };
    error.raw_os_error().is_some_and(|code| [1, 2, 5, 6, 13, 20, ESTALE].contains(&code))
}

pub fn home_directory() -> String {
    std::env::home_dir().map(|path| path.to_string_lossy().into_owned()).unwrap_or_default()
}

/// fs.Stats의 mtimeMs(초×1000 + 나노초/1e6).
pub fn modified_milliseconds(metadata: &fs::Metadata) -> f64 {
    metadata.mtime() as f64 * 1_000.0 + metadata.mtime_nsec() as f64 / 1_000_000.0
}

pub fn list_generated_images(session_id: &str, generated_root: &str) -> io::Result<Vec<String>> {
    if session_id.is_empty() {
        return Ok(Vec::new());
    }
    let directory = join_path(&[generated_root, session_id]);
    if fs::metadata(&directory).is_err() {
        return Ok(Vec::new());
    }
    let entries = match fs::read_dir(&directory).and_then(|entries| entries.collect::<io::Result<Vec<_>>>()) {
        Ok(entries) => entries,
        Err(error) if is_unavailable(&error) => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut images: Vec<String> = entries
        .iter()
        // Dirent.isFile처럼 링크를 따라가지 않는다.
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .map(|entry| join_path(&[&directory, &entry.file_name().to_string_lossy()]))
        .filter(|path| is_local_image(path))
        .collect();
    images.sort_by(|left, right| js_compare(left, right));
    Ok(images)
}

pub fn generated_image_root(backend: Option<&str>) -> String {
    let home = home_directory();
    if backend == Some("antigravity") {
        join_path(&[&home, ".gemini", "antigravity-cli", "brain"])
    } else {
        join_path(&[&home, ".codex", "generated_images"])
    }
}

/// 턴 시작 2초 전부터 끝 2초 뒤까지 고친 생성 이미지. 끝 시각이 없으면 지금까지다.
pub fn generated_images_for_turn(
    session_id: Option<&str>,
    started_at: f64,
    ended_at: Option<f64>,
    generated_root: &str,
) -> io::Result<Vec<String>> {
    let end = ended_at.unwrap_or_else(now_milliseconds);
    if !started_at.is_finite() || !end.is_finite() {
        return Ok(Vec::new());
    }
    let mut images = Vec::new();
    for path in list_generated_images(session_id.unwrap_or(""), generated_root)? {
        match fs::metadata(&path) {
            Ok(metadata) => {
                let modified = modified_milliseconds(&metadata);
                if modified >= started_at - 2_000.0 && modified <= end + 2_000.0 {
                    images.push(path);
                }
            }
            Err(error) if is_unavailable(&error) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(images)
}

pub fn append_local_image_previews(markdown: &str, candidate_paths: &[String]) -> String {
    let source = js_trim(markdown);
    let destinations = markdown_destinations(source);
    let previewed_paths: Vec<String> = destinations
        .iter()
        .filter(|destination| destination.is_image)
        .filter_map(|destination| local_image_path(&destination.value))
        .collect();
    let mut previewed: HashSet<String> = previewed_paths.iter().map(|path| image_content_identity(path)).collect();
    let mut included: HashSet<String> = previewed_paths.into_iter().collect();
    let linked: Vec<String> = destinations
        .iter()
        .filter(|destination| !destination.is_image)
        .filter_map(|destination| local_image_path(&destination.value))
        .collect();

    let mut paths = Vec::new();
    // 응답에서 명시한 프로젝트 파일을 생성 임시 원본보다 우선한다.
    // 경로가 달라도 내용이 같은 이미지는 미리보기를 한 번만 붙인다.
    for value in linked.iter().chain(candidate_paths) {
        let Some(path) = local_image_path(value) else {
            continue;
        };
        if included.contains(&path) {
            continue;
        }
        let identity = image_content_identity(&path);
        if previewed.contains(&identity) {
            continue;
        }
        included.insert(path.clone());
        previewed.insert(identity);
        paths.push(path);
        if paths.len() == 8 {
            break;
        }
    }
    if paths.is_empty() {
        return source.to_string();
    }

    let previews: Vec<String> = paths
        .iter()
        .enumerate()
        .map(|(index, path)| {
            let url = path_to_file_url(path);
            format!("[![생성 이미지 {}](<{url}>)](<{url}>)", index + 1)
        })
        .collect();
    format!("{source}\n\n{}", previews.join("\n\n"))
}

fn image_content_identity(path: &str) -> String {
    let Ok(metadata) = fs::metadata(path) else {
        return format!("path:{path}");
    };
    let key = format!("{path}\u{1f}{}\u{1f}{}", metadata.size(), modified_milliseconds(&metadata));
    if let Some(cached) = IMAGE_IDENTITY_CACHE.lock().unwrap_or_else(|poison| poison.into_inner()).values.get(&key) {
        return cached.clone();
    }
    let Ok(bytes) = fs::read(path) else {
        return format!("path:{path}");
    };
    let digest: String = Sha256::digest(&bytes).iter().map(|byte| format!("{byte:02x}")).collect();
    let identity = format!("{}:{digest}", metadata.size());
    let mut cache = IMAGE_IDENTITY_CACHE.lock().unwrap_or_else(|poison| poison.into_inner());
    if cache.values.insert(key.clone(), identity.clone()).is_none() {
        cache.order.push_back(key);
    }
    if cache.values.len() > IMAGE_IDENTITY_CACHE_LIMIT
        && let Some(oldest) = cache.order.pop_front()
    {
        cache.values.remove(&oldest);
    }
    identity
}

struct Destination {
    is_image: bool,
    value: String,
}

/// `/(!?)\[(?!\!)[^\]\n]*\]\((?:<([^>\n]+)>|([^\s)\n]+))\)/g`의 matchAll. 대괄호와 괄호 안은
/// 되돌아가도 다른 답이 없으므로 앞에서부터 한 번씩 맞춰 본다.
fn markdown_destinations(markdown: &str) -> Vec<Destination> {
    let bytes = markdown.as_bytes();
    let mut destinations = Vec::new();
    let mut position = 0;
    while position < bytes.len() {
        let (is_image, bracket) = if bytes[position] == b'!' && bytes.get(position + 1) == Some(&b'[') {
            (true, position + 1)
        } else if bytes[position] == b'[' {
            (false, position)
        } else {
            position += 1;
            continue;
        };
        match link_destination(markdown, bracket) {
            Some((value, end)) => {
                destinations.push(Destination { is_image, value: value.to_string() });
                position = end;
            }
            None => position += 1,
        }
    }
    destinations
}

fn link_destination(markdown: &str, bracket: usize) -> Option<(&str, usize)> {
    let bytes = markdown.as_bytes();
    let mut index = bracket + 1;
    if bytes.get(index) == Some(&b'!') {
        return None;
    }
    while index < bytes.len() && bytes[index] != b']' && bytes[index] != b'\n' {
        index += 1;
    }
    if bytes.get(index) != Some(&b']') || bytes.get(index + 1) != Some(&b'(') {
        return None;
    }
    index += 2;
    if bytes.get(index) == Some(&b'<') {
        let start = index + 1;
        let mut end = start;
        while end < bytes.len() && bytes[end] != b'>' && bytes[end] != b'\n' {
            end += 1;
        }
        if end > start && bytes.get(end) == Some(&b'>') && bytes.get(end + 1) == Some(&b')') {
            return Some((&markdown[start..end], end + 2));
        }
    }
    let start = index;
    let end = markdown[start..]
        .char_indices()
        .find(|(_, c)| crate::js::is_js_whitespace(*c) || *c == ')')
        .map_or(bytes.len(), |(offset, _)| start + offset);
    if end > start && bytes.get(end) == Some(&b')') {
        return Some((&markdown[start..end], end + 1));
    }
    None
}

fn local_image_path(value: &str) -> Option<String> {
    let path = if value.starts_with("file:") {
        file_url_to_path(value)?
    } else if value.starts_with('/') {
        decode_uri_component(value)?
    } else {
        return None;
    };
    let normalized = normalize_path(&path);
    is_local_image(&normalized).then_some(normalized)
}

fn is_local_image(path: &str) -> bool {
    IMAGE_EXTENSIONS.contains(&extname(path).to_lowercase().as_str())
        && fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
}

/// Node v24 url.fileURLToPath(POSIX). 호스트가 있거나 경로에 %2F가 있으면 Node는 예외를 낸다.
pub fn file_url_to_path(value: &str) -> Option<String> {
    let url = url::Url::parse(value).ok()?;
    if url.scheme() != "file" || url.host_str().is_some_and(|host| !host.is_empty()) {
        return None;
    }
    let pathname = url.path();
    if pathname.to_ascii_lowercase().contains("%2f") {
        return None;
    }
    decode_uri_component(pathname)
}

/// Node v24 url.pathToFileURL(POSIX)의 href. 이 바이트들은 퍼센트 인코딩하고 나머지는 그대로 둔다.
pub fn path_to_file_url(path: &str) -> String {
    let resolved = normalize_path(path);
    let resolved = if resolved.len() > 1 { resolved.trim_end_matches('/') } else { resolved.as_str() };
    let mut url = String::from("file://");
    for byte in resolved.bytes() {
        if byte < 0x21 || byte >= 0x7f || b"\"#%<>?[\\]^`{|}~".contains(&byte) {
            url.push_str(&format!("%{byte:02X}"));
        } else {
            url.push(byte as char);
        }
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    struct TempDirectory(String);

    impl TempDirectory {
        fn new(prefix: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("{prefix}{}-{unique}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Self(path.to_string_lossy().into_owned())
        }

        fn join(&self, name: &str) -> String {
            join_path(&[&self.0, name])
        }
    }

    impl Drop for TempDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn set_modified(path: &str, iso: &str) {
        let milliseconds = crate::js::date_parse(iso).unwrap();
        let time = SystemTime::UNIX_EPOCH + Duration::from_millis(milliseconds as u64);
        fs::File::options().write(true).open(path).unwrap().set_modified(time).unwrap();
    }

    fn preview_count(rendered: &str) -> usize {
        rendered.matches("[![생성 이미지").count()
    }

    #[test]
    fn 생성_이미지_폴더를_읽을_수_없어도_대화_조회를_막지_않는다() {
        let root = TempDirectory::new("officellm-unavailable-images-");
        // 폴더 자리에 파일이 있으면 readdir가 ENOTDIR을 낸다.
        fs::write(root.join("session-1"), "not a directory").unwrap();
        assert!(list_generated_images("session-1", &root.0).unwrap().is_empty());
        let started = crate::js::date_parse("2026-09-05T00:00:00Z").unwrap();
        let ended = crate::js::date_parse("2026-09-05T01:00:00Z").unwrap();
        let images = generated_images_for_turn(Some("session-1"), started, Some(ended), &root.0).unwrap();
        assert_eq!(append_local_image_previews("대화 본문은 유지", &images), "대화 본문은 유지");
    }

    #[test]
    fn 읽기_권한이_없는_생성_이미지_폴더도_빈_목록이다() {
        use std::os::unix::fs::PermissionsExt;
        let root = TempDirectory::new("officellm-unreadable-images-");
        let directory = root.join("session-1");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o000)).unwrap();
        let result = list_generated_images("session-1", &root.0);
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn antigravity_생성_이미지는_전용_brain_디렉터리에서_찾는다() {
        assert!(generated_image_root(Some("antigravity")).ends_with("/.gemini/antigravity-cli/brain"));
        assert!(generated_image_root(Some("codex")).ends_with("/.codex/generated_images"));
    }

    #[test]
    fn 절대경로_이미지_링크에_미리보기를_추가한다() {
        let directory = TempDirectory::new("officellm-artifact-");
        let image = directory.join("sample image.png");
        fs::write(&image, "png").unwrap();
        let rendered = append_local_image_previews(&format!("[이미지 열기](<{image}>)"), &[]);
        let url = path_to_file_url(&image);
        assert!(url.starts_with("file:///") && url.ends_with("sample%20image.png"));
        assert!(rendered.ends_with(&format!("\n\n[![생성 이미지 1](<{url}>)](<{url}>)")));
        assert_eq!(append_local_image_previews(&rendered, &[]), rendered);
    }

    #[test]
    fn 이미_표시된_이미지는_중복_첨부하지_않는다() {
        let directory = TempDirectory::new("officellm-artifact-");
        let image = directory.join("sample.png");
        fs::write(&image, "png").unwrap();
        let markdown = format!("![결과](<{image}>)");
        assert_eq!(append_local_image_previews(&markdown, std::slice::from_ref(&image)), markdown);
    }

    #[test]
    fn 프로젝트_복사본과_생성_원본이_같으면_미리보기_하나만_붙인다() {
        let directory = TempDirectory::new("officellm-artifact-");
        let generated = directory.join("generated.png");
        let project_copy = directory.join("episode-03.png");
        fs::write(&generated, "same-image-bytes").unwrap();
        fs::write(&project_copy, "same-image-bytes").unwrap();
        let rendered = append_local_image_previews(&format!("[결과 파일](<{project_copy}>)"), &[generated.clone()]);
        assert_eq!(preview_count(&rendered), 1);
        assert!(rendered.contains(&path_to_file_url(&project_copy)));
        assert!(!rendered.contains(&path_to_file_url(&generated)));
    }

    #[test]
    fn 크기가_같아도_내용이_다른_이미지는_각각_미리보기를_붙인다() {
        let directory = TempDirectory::new("officellm-artifact-");
        let first = directory.join("first.png");
        let second = directory.join("second.png");
        fs::write(&first, "image-a").unwrap();
        fs::write(&second, "image-b").unwrap();
        assert_eq!(preview_count(&append_local_image_previews("완료", &[first, second])), 2);
    }

    #[test]
    fn 업무_시간에_생성된_이미지만_찾는다() {
        let root = TempDirectory::new("officellm-generated-");
        let session = root.join("session-1");
        fs::create_dir(&session).unwrap();
        let image = join_path(&[&session, "generated.png"]);
        fs::write(&image, "png").unwrap();
        set_modified(&image, "2026-07-27T12:51:32Z");
        let started = crate::js::date_parse("2026-07-27T12:49:55Z").unwrap();
        let ended = crate::js::date_parse("2026-07-27T12:51:36Z").unwrap();
        assert_eq!(generated_images_for_turn(Some("session-1"), started, Some(ended), &root.0).unwrap(), [image.clone()]);
        let late = crate::js::date_parse("2026-07-27T12:51:29Z").unwrap();
        assert!(generated_images_for_turn(Some("session-1"), started, Some(late), &root.0).unwrap().is_empty());
    }

    #[test]
    fn 파일_url_변환은_node_v24와_같다() {
        let mut path = String::from("/t/");
        for byte in 1u8..128 {
            if byte != b'/' {
                path.push(byte as char);
            }
        }
        path.push_str("é한😀.png");
        // Node v24.22.1 pathToFileURL(path).href
        assert_eq!(
            path_to_file_url(&path),
            "file:///t/%01%02%03%04%05%06%07%08%09%0A%0B%0C%0D%0E%0F%10%11%12%13%14%15%16%17%18%19%1A%1B%1C%1D%1E%1F%20!%22%23$%25&'()*+,-.0123456789:;%3C=%3E%3F@ABCDEFGHIJKLMNOPQRSTUVWXYZ%5B%5C%5D%5E_%60abcdefghijklmnopqrstuvwxyz%7B%7C%7D%7E%7F%C3%A9%ED%95%9C%F0%9F%98%80.png"
        );
        // Node v24.22.1 fileURLToPath(value). 예외는 None이다.
        let cases = [
            ("file:///a/b%20c.png", Some("/a/b c.png")),
            ("file://localhost/a/x.png", Some("/a/x.png")),
            ("file://host/a.png", None),
            ("file:///a/%2Fb.png", None),
            ("file:///a/%2fb.png", None),
            ("file:///a/../b/./c.png", Some("/b/c.png")),
            ("file:/a/b.png", Some("/a/b.png")),
            ("file:a/b.png", Some("/a/b.png")),
            ("file:///a/b.png?x=1#y", Some("/a/b.png")),
            ("file:///a/%E0%A4.png", None),
            ("file:///a/b\\c.png", Some("/a/b/c.png")),
            ("file:///a/한.png", Some("/a/한.png")),
        ];
        for (value, expected) in cases {
            assert_eq!(file_url_to_path(value).as_deref(), expected, "{value}");
        }
    }

    #[test]
    fn 마크다운_링크는_node_정규식과_같은_곳을_고른다() {
        let found = |markdown: &str| {
            markdown_destinations(markdown)
                .into_iter()
                .map(|destination| (destination.is_image, destination.value))
                .collect::<Vec<_>>()
        };
        assert_eq!(found("![a](<x y>) [b](c) [!c](d)"), [(true, "x y".into()), (false, "c".into())]);
        // Node: 꺾쇠 갈래가 실패하면 두 번째 갈래가 꺾쇠까지 값으로 잡는다.
        assert_eq!(found("[a](<b>c) [d](e f)"), [(false, "<b>c".into())]);
        assert_eq!(found("[a](<b>c)"), [(false, "<b>c".into())]);
        assert_eq!(found("[a]( b) [c](d\u{3000}) [e](f)"), [(false, "f".into())]);
        assert_eq!(found("![x] [y](z)"), [(false, "z".into())]);
        assert_eq!(found("[a\nb](c) [](d)"), [(false, "d".into())]);
    }
}
