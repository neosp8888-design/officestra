#!/bin/sh
# Community Preview CI가 실제 외부 계정 없이 첫 업무 전체 흐름을 검증할 때 쓴다.
set -eu

if [ "${1:-}" = "debug" ] && [ "${2:-}" = "models" ]; then
    printf '%s\n' '{"models":[{"slug":"gpt-5.6-terra","display_name":"GPT-5.6-Terra","visibility":"list","default_reasoning_level":"high","supported_reasoning_levels":[{"effort":"high"}]}]}'
    exit 0
fi
if [ "${1:-}" = "--version" ]; then
    printf '%s\n' 'codex-cli 0.0.0-e2e'
    exit 0
fi
[ "${1:-}" = "exec" ] || exit 64

printf '%s\n' '{"type":"thread.started","thread_id":"community-preview-e2e-session"}'
printf '%s\n' '{"type":"item.completed","item":{"id":"message-1","type":"agent_message","text":"Community preview first task completed."}}'
printf '%s\n' '{"type":"turn.completed","usage":{"input_tokens":10,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":8,"reasoning_output_tokens":0}}'
