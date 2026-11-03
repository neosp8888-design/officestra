// 로컬 모델의 능력은 검증된 산출물·런타임에 고정하고 이름으로 추정하지 않는다. Node의 local-model-capabilities.mjs와 같다.

pub const MEROMERO_MODEL_ID: &str = "officestra-meromero-31b-uncensored-heretic-q4km";
pub const MEROMERO_HOST_KEY: &str = "g4-meromero-31b-uncensored-heretic";
pub const MEROMERO_26B_MODEL_ID: &str = "officestra-meromero-26b-a4b-uncensored-heretic-q4km";
pub const MEROMERO_26B_HOST_KEY: &str = "g4-meromero-26b-a4b-it-uncensored-heretic";
pub const DIRECT_RUNTIME: &str = "llama-cpp-b10982";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Artifact {
    pub root: &'static str,
    pub file: &'static str,
    pub size: u64,
    pub sha256: &'static str,
}

pub const MEROMERO_ARTIFACT: Artifact = Artifact {
    root: r"G:\llm\llmfan46\G4-MeroMero-31B-uncensored-heretic-GGUF",
    file: "G4-MeroMero-31B-uncensored-heretic-Q4_K_M.gguf",
    size: 18_687_063_776,
    sha256: "A33DAFFCE3B76ADA423616D7CCBE7A70724DA854F8175605967B290CE3D9EF0C",
};
pub const MEROMERO_VISION_ARTIFACT: Artifact = Artifact {
    root: MEROMERO_ARTIFACT.root,
    file: "G4-MeroMero-31B-uncensored-heretic-mmproj-BF16.gguf",
    size: 1_200_727_200,
    sha256: "7555F7CB379C138A01C7A70298747AD4451C9641FBD42E09EE7C86D61150EA49",
};
pub const MEROMERO_26B_ARTIFACT: Artifact = Artifact {
    root: r"G:\llm\llmfan46\G4-MeroMero-26B-A4B-it-uncensored-heretic-GGUF",
    file: "G4-MeroMero-26B-A4B-it-uncensored-heretic-Q4_K_M.gguf",
    size: 17_331_776_896,
    sha256: "9101D86B7F2AECD90FF0FB5F43AE73B2CBE580D335011FD60399AA864A3FE7E1",
};
pub const MEROMERO_26B_VISION_ARTIFACT: Artifact = Artifact {
    root: MEROMERO_26B_ARTIFACT.root,
    file: "G4-MeroMero-26B-A4B-it-uncensored-heretic-mmproj-BF16.gguf",
    size: 1_194_827_840,
    sha256: "A4DD63C9C0B67098CE25CF85359B6E4B325BB719F65590AD8E8ACD5ED77731F7",
};

/// 능력 판정에 필요한 프로필 조각. 전체 프로필 구조는 local_agent_profile에 있다.
pub trait LocalProfileLike {
    fn model(&self) -> &str;
    fn runtime(&self) -> Option<&str>;
    fn reasoning(&self) -> Option<&str>;
}

pub fn is_mero_mero_model(model: &str) -> bool {
    model == MEROMERO_MODEL_ID || model == MEROMERO_26B_MODEL_ID
}

pub fn is_mero_mero(profile: &impl LocalProfileLike) -> bool {
    profile.runtime() == Some(DIRECT_RUNTIME) && is_mero_mero_model(profile.model())
}

pub fn local_model_supports_vision(profile: &impl LocalProfileLike) -> bool {
    profile.model() != MEROMERO_MODEL_ID
}

pub struct RuntimeArtifacts {
    pub model: Artifact,
    pub vision: Option<Artifact>,
    pub host_key: &'static str,
}

pub fn mero_runtime_artifacts(profile: &impl LocalProfileLike) -> Result<RuntimeArtifacts, String> {
    if !is_mero_mero(profile) {
        return Err("Unsupported MeroMero profile".to_string());
    }
    Ok(if profile.model() == MEROMERO_26B_MODEL_ID {
        RuntimeArtifacts {
            model: MEROMERO_26B_ARTIFACT,
            vision: Some(MEROMERO_26B_VISION_ARTIFACT),
            host_key: MEROMERO_26B_HOST_KEY,
        }
    } else {
        RuntimeArtifacts { model: MEROMERO_ARTIFACT, vision: None, host_key: MEROMERO_HOST_KEY }
    })
}

pub fn direct_reasoning_options(profile: &impl LocalProfileLike) -> &'static [&'static str] {
    if is_mero_mero(profile) { &["default", "off", "on"] } else { &["default", "low", "medium", "xhigh"] }
}

pub fn direct_default_reasoning(profile: &impl LocalProfileLike) -> &'static str {
    if is_mero_mero(profile) { "off" } else { "xhigh" }
}

pub fn direct_codex_reasoning(profile: &impl LocalProfileLike) -> String {
    let selected = match profile.reasoning() {
        Some(value) if value != "default" => value.to_string(),
        _ => direct_default_reasoning(profile).to_string(),
    };
    if is_mero_mero(profile) {
        if selected == "on" { "medium".to_string() } else { "none".to_string() }
    } else {
        selected
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct P(&'static str, Option<&'static str>, Option<&'static str>);
    impl LocalProfileLike for P {
        fn model(&self) -> &str {
            self.0
        }
        fn runtime(&self) -> Option<&str> {
            self.1
        }
        fn reasoning(&self) -> Option<&str> {
            self.2
        }
    }

    #[test]
    fn 메로메로는_직접_런타임일_때만_인정한다() {
        assert!(is_mero_mero(&P(MEROMERO_26B_MODEL_ID, Some(DIRECT_RUNTIME), None)));
        assert!(!is_mero_mero(&P(MEROMERO_26B_MODEL_ID, None, None)));
        assert!(!local_model_supports_vision(&P(MEROMERO_MODEL_ID, Some(DIRECT_RUNTIME), None)));
        assert!(local_model_supports_vision(&P(MEROMERO_26B_MODEL_ID, Some(DIRECT_RUNTIME), None)));
        let artifacts = mero_runtime_artifacts(&P(MEROMERO_26B_MODEL_ID, Some(DIRECT_RUNTIME), None)).unwrap();
        assert_eq!(artifacts.host_key, MEROMERO_26B_HOST_KEY);
        assert!(artifacts.vision.is_some());
        assert!(mero_runtime_artifacts(&P("other", Some(DIRECT_RUNTIME), None)).is_err());
    }

    #[test]
    fn codex_추론_값_변환() {
        let mero = |r| P(MEROMERO_MODEL_ID, Some(DIRECT_RUNTIME), r);
        assert_eq!(direct_codex_reasoning(&mero(None)), "none");
        assert_eq!(direct_codex_reasoning(&mero(Some("on"))), "medium");
        assert_eq!(direct_codex_reasoning(&mero(Some("default"))), "none");
        let qwen = |r| P("officestra-qwen38-27b-uncensored-q4km", Some(DIRECT_RUNTIME), r);
        assert_eq!(direct_codex_reasoning(&qwen(None)), "xhigh");
        assert_eq!(direct_codex_reasoning(&qwen(Some("low"))), "low");
        assert_eq!(direct_reasoning_options(&qwen(None)), ["default", "low", "medium", "xhigh"]);
    }
}
