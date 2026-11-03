// Antigravity 모델 중 추론 강도를 바꿀 수 없는 모델 판정. Node의 antigravity-model-selection.mjs와 같다.

pub fn is_antigravity_fixed_thinking_model(model: &str) -> bool {
    matches!(model, "claude-sonnet-4-6" | "claude-opus-4-6-thinking")
}

/// antigravityModelArguments. 고정 추론 모델은 --model만, gemini는 노력 수준을 모델 ID 끝에 붙이고,
/// 이미 -low/-medium/-high로 끝나거나 노력이 default면 --model만, 그 밖은 --effort를 함께 넘긴다.
pub fn antigravity_model_arguments(model: &str, effort: &str) -> Vec<String> {
    if is_antigravity_fixed_thinking_model(model) {
        return vec!["--model".into(), model.into()];
    }
    // /^(.*)-(low|medium|high)$/는 마지막 '-' 뒤가 세 낱말 중 하나일 때만 맞고, `.`은 줄 끝 문자를 넘지 못한다.
    let variant = model
        .rsplit_once('-')
        .filter(|(base, suffix)| {
            matches!(*suffix, "low" | "medium" | "high") && !base.contains(['\n', '\r', '\u{2028}', '\u{2029}'])
        })
        .map(|(base, _)| base);
    if model.starts_with("gemini-") && matches!(effort, "low" | "medium" | "high") {
        return vec!["--model".into(), format!("{}-{effort}", variant.unwrap_or(model))];
    }
    if variant.is_some() || effort == "default" {
        return vec!["--model".into(), model.into()];
    }
    vec!["--model".into(), model.into(), "--effort".into(), effort.into()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 모델_인자는_node_규칙을_따른다() {
        let call = |model, effort| antigravity_model_arguments(model, effort).join(" ");
        assert_eq!(call("claude-opus-4-6-thinking", "high"), "--model claude-opus-4-6-thinking");
        assert_eq!(call("gemini-3-pro-high", "low"), "--model gemini-3-pro-low");
        assert_eq!(call("gemini-3-flash", "medium"), "--model gemini-3-flash-medium");
        assert_eq!(call("gemini-3-flash", "default"), "--model gemini-3-flash");
        assert_eq!(call("custom-high", "xhigh"), "--model custom-high");
        assert_eq!(call("custom", "xhigh"), "--model custom --effort xhigh");
        // 줄바꿈이 있으면 `.`가 맞지 않아 변형으로 보지 않는다.
        assert_eq!(call("a\nb-high", "xhigh"), "--model a\nb-high --effort xhigh");
    }

    #[test]
    fn 고정_추론_모델만_참이다() {
        assert!(is_antigravity_fixed_thinking_model("claude-opus-4-6-thinking"));
        assert!(is_antigravity_fixed_thinking_model("claude-sonnet-4-6"));
        assert!(!is_antigravity_fixed_thinking_model("gemini-3.8-flash"));
    }
}
