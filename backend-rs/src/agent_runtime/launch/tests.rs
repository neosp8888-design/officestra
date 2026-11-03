// 실행 인자·환경 시험. Node agent-runtime.test.mjs의 인자·서명·환경 시험을 옮겼다.

use serde_json::json;

use super::*;

fn character(backend: &str) -> RuntimeCharacter {
    RuntimeCharacter {
        id: None,
        name: String::new(),
        seat: String::new(),
        backend: backend.into(),
        model: None,
        effort: "high".into(),
        fast_mode: false,
        auto_compact_percent: None,
        permission: String::new(),
        identity_prompt: String::new(),
        config: json!({}),
        local_profile: None,
    }
}

fn codex_character() -> RuntimeCharacter {
    RuntimeCharacter {
        model: Some("gpt-5.6-sol".into()),
        fast_mode: true,
        permission: "workspace-write".into(),
        name: "코과장".into(),
        seat: "우측 아래".into(),
        identity_prompt: "업무를 정확히 처리한다.".into(),
        ..character("codex")
    }
}

fn claude_character(permission: &str, identity: &str) -> RuntimeCharacter {
    RuntimeCharacter {
        model: Some("claude-opus-5".into()),
        fast_mode: true,
        permission: permission.into(),
        name: "클대리".into(),
        seat: "좌측 아래".into(),
        identity_prompt: identity.into(),
        ..character("claude")
    }
}

fn after<'a>(arguments: &'a [String], flag: &str) -> &'a str {
    let index = arguments.iter().position(|value| value == flag).unwrap_or_else(|| panic!("{flag} 없음"));
    &arguments[index + 1]
}

fn has(arguments: &[String], value: &str) -> bool {
    arguments.iter().any(|item| item == value)
}

fn image(path: &str, is_codex_image: bool) -> StagedAttachment {
    StagedAttachment { name: String::new(), path: path.into(), directory: String::new(), is_codex_image }
}

#[test]
fn 자동_압축_기준은_20부터_95까지와_기본값_90을_쓴다() {
    assert_eq!(normalize_auto_compact_percent(None, 95.0), 90);
    assert_eq!(normalize_auto_compact_percent(Some(42.0), 95.0), 42);
    assert_eq!(normalize_auto_compact_percent(Some(12.0), 95.0), 20);
    assert_eq!(normalize_auto_compact_percent(Some(93.6), 95.0), 94);
    assert_eq!(normalize_auto_compact_percent(Some(100.0), 95.0), 95);
    assert_eq!(normalize_auto_compact_percent(Some(f64::NAN), 95.0), 90);
    assert_eq!(normalize_auto_compact_percent(Some(100.0), 100.0), 100);
}

#[test]
fn codex에는_png와_jpeg만_이미지_인수로_전달한다() {
    let arguments = build_arguments(
        &codex_character(),
        "파일을 확인해줘.",
        Some("session-1"),
        &[image("/tmp/photo.png", true), image("/tmp/document.pdf", false), image("/tmp/photo.jpeg", true)],
        None,
    )
    .unwrap();
    let images: Vec<&String> = arguments.iter().filter(|value| value.starts_with("/tmp/")).collect();
    assert_eq!(images, ["/tmp/photo.png", "/tmp/photo.jpeg"]);
    assert_eq!(arguments.last().unwrap(), "파일을 확인해줘.");
    assert!(has(&arguments, "show_raw_agent_reasoning=true"));
    assert!(has(&arguments, "model_reasoning_summary=\"detailed\""));
    assert!(has(&arguments, "features.fast_mode=true"));
    assert!(has(&arguments, "service_tier=\"fast\""));
}

#[test]
fn codex는_fast_끄기도_신규와_재개에_명시하고_컨텍스트_설정은_cli_기본값이다() {
    for previous in [None, Some("session-1")] {
        let slow = RuntimeCharacter { fast_mode: false, ..codex_character() };
        let arguments = build_arguments(&slow, "상태를 확인해줘.", previous, &[], None).unwrap();
        assert!(has(&arguments, "features.fast_mode=true"));
        assert!(has(&arguments, "service_tier=\"default\""));
        let arguments = build_arguments(&codex_character(), "긴 컨텍스트로 계속해줘.", previous, &[], None).unwrap();
        assert!(!arguments.iter().any(|value| value.starts_with("model_context_window=")));
        assert!(!arguments.iter().any(|value| value.starts_with("model_auto_compact_token_limit=")));
    }
}

#[test]
fn codex_재개는_바뀐_모델·추론·fast·권한을_같은_세션에_전달한다() {
    let changed = RuntimeCharacter {
        model: Some("gpt-5.6-terra".into()),
        effort: "xhigh".into(),
        fast_mode: false,
        permission: "danger-full-access".into(),
        ..codex_character()
    };
    let arguments = build_arguments(&changed, "같은 세션에서 계속해줘.", Some("session-1"), &[], None).unwrap();
    assert_eq!(arguments[..4], ["exec", "resume", "session-1", "--json"]);
    assert!(has(&arguments, "model=\"gpt-5.6-terra\""));
    assert!(has(&arguments, "model_reasoning_effort=\"xhigh\""));
    assert!(has(&arguments, "service_tier=\"default\""));
    assert!(has(&arguments, "sandbox_mode=\"danger-full-access\""));

    let fresh = build_arguments(&changed, "새로", None, &[], None).unwrap();
    assert_eq!(fresh[..3], ["exec", "--json", "--skip-git-repo-check"]);
    assert_eq!(after(&fresh, "-s"), "danger-full-access");
    let discovered = RuntimeCharacter { model: Some("gpt-future-mini".into()), effort: "medium".into(), ..codex_character() };
    let arguments = build_arguments(&discovered, "새 모델을 사용해줘.", Some("session-1"), &[], None).unwrap();
    assert!(has(&arguments, "model=\"gpt-future-mini\""));
    assert!(has(&arguments, "model_reasoning_effort=\"medium\""));
    let unset = RuntimeCharacter { model: Some(String::new()), ..codex_character() };
    let arguments = build_arguments(&unset, "모델 없음", None, &[], None).unwrap();
    assert!(!arguments.iter().any(|value| value.starts_with("model=")), "빈 모델은 넘기지 않는다");
}

#[test]
fn codex_신규와_재개는_db_업무_지침과_짧은_결과_통로_안내만_전달한다() {
    for previous in [None, Some("session-1")] {
        let updated = RuntimeCharacter { identity_prompt: "업데이트된 역할 지침을 따른다.".into(), ..codex_character() };
        let arguments = build_arguments(&updated, "계속해줘.", previous, &[], None).unwrap();
        let instructions = arguments.iter().find(|value| value.starts_with("developer_instructions=")).unwrap();
        let expected = identity_prompt_with_structured_result("업데이트된 역할 지침을 따른다.", None);
        assert_eq!(instructions, &format!("developer_instructions={}", serde_json::to_string(&expected).unwrap()));
        assert_eq!(arguments.last().unwrap(), "계속해줘.");
    }
}

#[test]
fn claude는_fast_설정과_db_업무_지침을_매_실행마다_전달한다() {
    for previous in [None, Some("session-1")] {
        let arguments =
            build_arguments(&claude_character("auto", "업무를 정확히 처리한다."), "상태를 확인해줘.", previous, &[], None).unwrap();
        assert_eq!(serde_json::from_str::<Value>(after(&arguments, "--settings")).unwrap(), json!({ "fastMode": true }));

        let arguments = build_arguments(
            &claude_character("bypassPermissions", "업데이트된 역할 지침을 따른다."),
            "계속해줘.",
            previous,
            &[],
            None,
        )
        .unwrap();
        assert_eq!(
            after(&arguments, "--append-system-prompt"),
            identity_prompt_with_structured_result("업데이트된 역할 지침을 따른다.", None)
        );
        assert_eq!(after(&arguments, "--permission-mode"), "bypassPermissions");
        assert_eq!(has(&arguments, "--resume"), previous.is_some());
        assert!(!has(&arguments, "--exclude-dynamic-system-prompt-sections"));
        assert_eq!(after(&arguments, "--prompt-suggestions"), "true");
        assert_eq!(arguments[..2], ["-p", "계속해줘."]);
    }
    let slow = RuntimeCharacter { fast_mode: false, ..claude_character("auto", "") };
    let arguments = build_arguments(&slow, "x", None, &[], None).unwrap();
    assert_eq!(after(&arguments, "--settings"), r#"{"fastMode":false}"#);
}

fn antigravity_character(model: &str, effort: &str, permission: &str, identity: &str) -> RuntimeCharacter {
    RuntimeCharacter {
        model: Some(model.into()),
        effort: effort.into(),
        permission: permission.into(),
        identity_prompt: identity.into(),
        ..character("antigravity")
    }
}

#[test]
fn antigravity_신규와_재개는_모델·추론·대화_id와_업무_폴더를_전달한다() {
    for previous in [None, Some("conversation-1")] {
        let agent = antigravity_character("gemini-3.7-flash", "high", "accept-edits", "업무 지시를 정확히 이해한다.");
        let arguments = build_arguments(&agent, "상태를 확인해줘.", previous, &[], Some("/repo/project")).unwrap();
        assert_eq!(arguments[0], "-p");
        assert!(arguments[1].contains("업무 지시를 정확히 이해한다"));
        assert!(arguments[1].contains("상태를 확인해줘"));
        assert!(arguments[1].contains("/repo/project"));
        assert_eq!(after(&arguments, "--add-dir"), "/repo/project");
        assert_eq!(after(&arguments, "--model"), "gemini-3.7-flash-high");
        assert!(!has(&arguments, "--effort"));
        assert_eq!(after(&arguments, "--print-timeout"), "24h");
        assert!(has(&arguments, "--sandbox"));
        assert!(has(&arguments, "--dangerously-skip-permissions"));
        assert_eq!(has(&arguments, "--conversation"), previous.is_some());
        assert!(!has(&arguments, "service_tier=\"fast\""));
        assert!(!arguments.iter().any(|value| value.starts_with("model_context_window=")));
    }
}

#[test]
fn antigravity_opus_thinking은_지원하지_않는_effort를_넘기지_않는다() {
    for effort in ["high", "default"] {
        for previous in [None, Some("conversation-opus")] {
            let agent = antigravity_character("claude-opus-4-6-thinking", effort, "plan", "확인");
            let arguments = build_arguments(&agent, "확인", previous, &[], Some("/repo")).unwrap();
            assert_eq!(after(&arguments, "--model"), "claude-opus-4-6-thinking");
            assert!(!has(&arguments, "--effort"));
            assert_eq!(has(&arguments, "--conversation"), previous.is_some());
        }
    }
}

#[test]
fn antigravity_공통_권한_3단계를_cli_안전_모드로_바꾼다() {
    let arguments_for = |permission: &str| {
        build_arguments(&antigravity_character("gemini-3.1-pro", "high", permission, "정확히 처리한다."), "확인해줘.", None, &[], Some("/repo"))
    };
    let read_only = arguments_for("plan").unwrap();
    let mode = read_only.iter().position(|value| value == "--mode").unwrap();
    assert_eq!(read_only[mode..], ["--mode", "plan"]);
    assert!(has(&arguments_for("accept-edits").unwrap(), "--sandbox"));
    let full = arguments_for("dangerously-skip-permissions").unwrap();
    assert!(!has(&full, "--sandbox"));
    assert!(has(&full, "--dangerously-skip-permissions"));
    assert_eq!(arguments_for("auto"), Err("지원하지 않는 Antigravity 권한입니다: auto".into()));
    assert_eq!(
        build_arguments(&character("gemini"), "x", None, &[], None),
        Err("지원하지 않는 직원 백엔드입니다: gemini".into())
    );
}

#[test]
fn antigravity_실행_프롬프트는_node_줄_구성과_같다() {
    let agent = antigravity_character("gemini-3.1-pro", "high", "plan", "역할");
    let identity = identity_prompt_with_structured_result("역할", None);
    assert_eq!(
        antigravity_execution_prompt(&agent, "요청", Some(" /repo ")),
        [
            "<officestra_identity>",
            &identity,
            "</officestra_identity>",
            "<officestra_execution>",
            "업무 폴더는 \"/repo\"입니다.",
            "명령을 실행할 때는 반드시 \"/repo\"로 이동한 뒤 실행합니다.",
            "사용자 요청 범위를 벗어난 파일이나 서비스를 변경하지 않습니다.",
            "</officestra_execution>",
            "<user_request>",
            "요청",
            "</user_request>",
        ]
        .join("\n")
    );
    assert!(antigravity_execution_prompt(&agent, "요청", None).contains("\n현재 업무 폴더 안에서만 파일 작업을 수행합니다.\n사용자"));
}

#[test]
fn claude_지속_세션은_prompt를_인수가_아닌_stream_json_stdin으로_받는다() {
    let agent = RuntimeCharacter {
        effort: "xhigh".into(),
        fast_mode: false,
        ..claude_character("bypassPermissions", "업데이트된 역할 지침을 따른다.")
    };
    for previous in [None, Some("session-1")] {
        let arguments = claude_persistent_arguments(&agent, previous);
        assert_eq!(arguments[0], "-p");
        assert_eq!(after(&arguments, "--input-format"), "stream-json");
        assert!(has(&arguments, "--include-partial-messages"));
        assert!(has(&arguments, "--prompt-suggestions"));
        assert_eq!(
            after(&arguments, "--append-system-prompt"),
            identity_prompt_with_structured_result("업데이트된 역할 지침을 따른다.", None)
        );
        assert_eq!(has(&arguments, "--resume"), previous.is_some());
        assert!(!has(&arguments, "--exclude-dynamic-system-prompt-sections"));
        assert!(!has(&arguments, "테스트 prompt"));
    }
}

#[test]
fn claude_지속_세션_서명은_캐시를_바꾸는_설정과_작업_공간을_구분한다() {
    let agent = RuntimeCharacter { fast_mode: false, ..claude_character("auto", "업무 지침") };
    let original = claude_persistent_worker_signature(&agent, "/bin/claude", "/repo/a");
    assert_eq!(original, claude_persistent_worker_signature(&agent.clone(), "/bin/claude", "/repo/a"));
    let changed = RuntimeCharacter { identity_prompt: "바뀐 지침".into(), ..agent.clone() };
    assert_ne!(original, claude_persistent_worker_signature(&changed, "/bin/claude", "/repo/a"));
    assert_ne!(original, claude_persistent_worker_signature(&agent, "/bin/claude", "/repo/b"));
    // Node JSON.stringify와 같은 키 순서다.
    assert!(original.starts_with(r#"{"executable":"/bin/claude","workdir":"/repo/a","model":"claude-opus-5","effort":"high","fastMode":false,"permission":"auto","autoCompactPercent":90,"identityPrompt":"#));
}

fn paths(driver: &str) -> LaunchPaths {
    LaunchPaths {
        result_tool_directory: "/office/backend/src".into(),
        node_executable: Some("/node/bin/node".into()),
        playwright_driver_directory: driver.into(),
        runtime_id: "4242".into(),
    }
}

#[test]
fn claude_실행은_직원별_자동_압축_기준과_명시적_업데이트를_쓴다() {
    let base = Environment::from([
        ("PATH".into(), "/tmp/bin".into()),
        ("CLAUDE_CODE_AUTO_COMPACT_WINDOW".into(), "250000".into()),
        ("CLAUDE_AUTOCOMPACT_PCT_OVERRIDE".into(), "65".into()),
    ]);
    let claude = RuntimeCharacter { auto_compact_percent: Some(93.0), ..character("claude") };
    let environment = execution_environment(&claude, &base, None, &paths("/none"));
    assert_eq!(environment["PATH"], "/tmp/bin");
    assert!(!environment.contains_key("CLAUDE_CODE_AUTO_COMPACT_WINDOW"));
    assert_eq!(environment["CLAUDE_AUTOCOMPACT_PCT_OVERRIDE"], "93");
    assert_eq!(environment["DISABLE_AUTOUPDATER"], "1");

    let codex = Environment::from([("PATH".into(), "/tmp/bin".into())]);
    assert_eq!(execution_environment(&character("codex"), &codex, None, &paths("/none")), codex);

    let root = format!("{}/launch-playwright-{}", crate::js::node_tmpdir(), std::process::id());
    let driver = format!("{root}/playwright-driver-1.57.0");
    std::fs::create_dir_all(format!("{driver}/package")).unwrap();
    std::fs::write(format!("{driver}/package/cli.js"), "").unwrap();
    let antigravity = execution_environment(&character("antigravity"), &codex, None, &paths(&driver));
    assert!(antigravity["PLAYWRIGHT_DRIVER_PATH"].ends_with("playwright-driver-1.57.0"));
    assert_eq!(antigravity["PLAYWRIGHT_NODEJS_PATH"], "/node/bin/node");
    std::fs::remove_dir_all(&root).unwrap();

    for backend in ["codex", "claude", "antigravity"] {
        let worker = RuntimeCharacter { id: Some(format!("{backend}-worker")), ..character(backend) };
        let environment = execution_environment(&worker, &codex, Some("/repo/project"), &paths("/none"));
        assert_eq!(
            environment[STRUCTURED_RESULT_ENV],
            structured_turn_result_path("/repo/project", worker.id.as_deref(), "4242")
        );
        assert_eq!(environment["PATH"], "/office/backend/src:/node/bin:/tmp/bin");
        assert_eq!(environment["OFFICESTRA_TOOL_MODE"], "gui");
        assert_eq!(environment["OFFICESTRA_TOOL_CHARACTER_ID"], format!("{backend}-worker"));
        assert!(environment["OFFICESTRA_TOOL_EVENTS_URL"].ends_with(":4317/api/developer-tool-events"));
    }
}

#[test]
fn path_앞자리는_중복을_빼고_빈_조각을_버린다() {
    assert_eq!(prepend_path_entries("/a::/b:/a", &[Some("/b"), None, Some("")]), "/b:/a");
    assert_eq!(prepend_path_entries("", &[Some("/tool")]), "/tool");
}
