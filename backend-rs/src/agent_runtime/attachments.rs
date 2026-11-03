// 업무 첨부 파일을 작업 폴더에 복사하고 프롬프트에 안내를 붙인다. Node agent-runtime.mjs의 stageAttachments 묶음과 같다.

use std::collections::HashSet;
use std::path::Path;

use serde_json::Value;

use crate::js::{extname, join_path, js_string_or_empty, js_trim, node_fs_message, random_uuid, realpath_sync};

pub const MAX_ATTACHMENTS: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedAttachment {
    pub name: String,
    pub path: String,
    pub directory: String,
    /// Codex `-i`로 넘길 그림(png·jpg·jpeg).
    pub is_codex_image: bool,
}

/// promptWithAttachments. 첨부가 없으면 프롬프트를 그대로 돌려준다.
pub fn prompt_with_attachments(prompt: &str, attachments: &[StagedAttachment]) -> String {
    if attachments.is_empty() {
        return prompt.to_string();
    }
    let quote = |text: &str| serde_json::to_string(text).unwrap_or_default();
    let mut lines = vec![prompt.to_string(), String::new(), "첨부 파일".into(), "다음 로컬 파일을 업무 자료로 사용하세요.".into()];
    lines.extend(attachments.iter().map(|attachment| format!("- {}: {}", quote(&attachment.name), quote(&attachment.path))));
    lines.join("\n")
}

/// stageAttachments. 경로는 `String(path ?? "").trim()` 뒤 중복·빈 값을 빼고, 20개를 넘으면 거부한다.
/// `<workdir>/.office-attachments/<uuid>/NN-이름`으로 복사하고, 하나라도 실패하면 폴더를 지운다.
pub fn stage_attachments(attachment_paths: &Value, workdir: &str) -> Result<Vec<StagedAttachment>, String> {
    let Value::Array(items) = attachment_paths else {
        return Err("첨부 파일 목록이 올바르지 않습니다.".into());
    };
    let mut seen = HashSet::new();
    let paths: Vec<String> = items
        .iter()
        .map(|item| js_trim(&js_string_or_empty(Some(item))).to_string())
        .filter(|path| seen.insert(path.clone()))
        .filter(|path| !path.is_empty())
        .collect();
    if paths.len() > MAX_ATTACHMENTS {
        return Err("첨부 파일은 한 번에 20개까지 선택할 수 있습니다.".into());
    }
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let directory = join_path(&[workdir, ".office-attachments", &random_uuid()]);
    std::fs::create_dir_all(&directory).map_err(|error| node_fs_message(&error, "mkdir", &directory))?;
    let staged = paths
        .iter()
        .enumerate()
        .map(|(index, path)| stage_one(path, index, &directory))
        .collect::<Result<Vec<_>, _>>();
    if staged.is_err() {
        let _ = std::fs::remove_dir_all(&directory);
    }
    staged
}

fn stage_one(path: &str, index: usize, directory: &str) -> Result<StagedAttachment, String> {
    let source = realpath_sync(path)?;
    let metadata = std::fs::metadata(&source).map_err(|error| node_fs_message(&error, "stat", &source))?;
    if !metadata.is_file() {
        return Err(format!("파일만 첨부할 수 있습니다. {path}"));
    }
    let name = Path::new(&source).file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let staged = join_path(&[directory, &format!("{:02}-{name}", index + 1)]);
    std::fs::copy(&source, &staged).map_err(|error| {
        let (code, description) = crate::js::uv_error(&error);
        format!("{code}: {description}, copyfile '{source}' -> '{staged}'")
    })?;
    let is_codex_image = matches!(extname(&name).to_lowercase().as_str(), ".png" | ".jpg" | ".jpeg");
    Ok(StagedAttachment { name, path: staged, directory: directory.to_string(), is_codex_image })
}

/// removeStagedAttachments. 첫 첨부의 폴더를 통째로 지운다.
pub fn remove_staged_attachments(attachments: &[StagedAttachment]) {
    if let Some(first) = attachments.first()
        && !first.directory.is_empty()
    {
        let _ = std::fs::remove_dir_all(&first.directory);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn temporary(prefix: &str) -> String {
        let path = format!("{}/{prefix}{}", crate::js::node_tmpdir(), random_uuid());
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn 업무_프롬프트에_보관된_첨부_경로를_기록한다() {
        let attachment = StagedAttachment {
            name: "report.pdf".into(),
            path: "/workspace/.office-attachments/01-report.pdf".into(),
            directory: "/workspace/.office-attachments".into(),
            is_codex_image: false,
        };
        assert_eq!(
            prompt_with_attachments("분석해줘.", &[attachment]),
            "분석해줘.\n\n첨부 파일\n다음 로컬 파일을 업무 자료로 사용하세요.\n- \"report.pdf\": \"/workspace/.office-attachments/01-report.pdf\""
        );
        assert_eq!(prompt_with_attachments("그대로", &[]), "그대로");
    }

    #[test]
    fn 첨부_원본을_작업_폴더에_보관한다() {
        let workdir = temporary("office-attachment-test-");
        let source = format!("{workdir}/source.txt");
        std::fs::write(&source, "attachment body").unwrap();
        let image = format!("{workdir}/screen.PNG");
        std::fs::write(&image, "png").unwrap();

        let staged = stage_attachments(&json!([source, format!(" {source} "), "", null, image]), &workdir).unwrap();
        assert_eq!(staged.len(), 2, "다듬은 뒤 같은 경로와 빈 값은 뺀다");
        assert_eq!(std::fs::read_to_string(&staged[0].path).unwrap(), "attachment body");
        assert!(staged[0].path.contains("/.office-attachments/"));
        assert!(staged[0].path.ends_with("/01-source.txt"));
        assert!(!staged[0].is_codex_image);
        assert!(staged[1].path.ends_with("/02-screen.PNG"));
        assert!(staged[1].is_codex_image);
        remove_staged_attachments(&staged);
        assert!(!Path::new(&staged[0].directory).exists());
        std::fs::remove_dir_all(&workdir).unwrap();
    }

    #[test]
    fn 잘못된_목록·폴더·없는_파일은_거부하고_폴더를_남기지_않는다() {
        let workdir = temporary("office-attachment-error-");
        assert_eq!(stage_attachments(&json!(null), &workdir), Err("첨부 파일 목록이 올바르지 않습니다.".into()));
        assert_eq!(stage_attachments(&json!("a"), &workdir), Err("첨부 파일 목록이 올바르지 않습니다.".into()));
        let many: Vec<String> = (0..21).map(|index| format!("/tmp/{index}")).collect();
        assert_eq!(stage_attachments(&json!(many), &workdir), Err("첨부 파일은 한 번에 20개까지 선택할 수 있습니다.".into()));
        assert_eq!(stage_attachments(&json!([" ", null]), &workdir), Ok(Vec::new()));
        assert!(!Path::new(&format!("{workdir}/.office-attachments")).exists(), "첨부가 없으면 폴더를 만들지 않는다");

        assert_eq!(stage_attachments(&json!([workdir.clone()]), &workdir), Err(format!("파일만 첨부할 수 있습니다. {workdir}")));
        let missing = stage_attachments(&json!([format!("{workdir}/none.txt")]), &workdir).unwrap_err();
        assert!(missing.starts_with("ENOENT: no such file or directory, lstat '"), "{missing}");
        let left: Vec<_> = std::fs::read_dir(format!("{workdir}/.office-attachments")).unwrap().collect();
        assert!(left.is_empty(), "실패한 준비 폴더는 지운다");
        std::fs::remove_dir_all(&workdir).unwrap();
    }
}
