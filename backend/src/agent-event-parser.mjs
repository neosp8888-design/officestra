// 이 파일은 Codex와 Claude CLI JSONL을 공개 가능한 실시간 업무 이벤트로 변환한다.

import { isAbsolute, relative } from "node:path";

import {
  ProvenanceValidationError,
  normalizeResponseSources,
} from "./work-record-provenance.mjs";

const MAX_REASONING_LENGTH = 6_000;
const MAX_COLLABORATION_PROMPT_LENGTH = 4_000;
const MAX_COLLABORATION_RESULT_LENGTH = 12_000;
const NEED_INPUT_MARKER = "[NEED_INPUT]";
const RESPONSE_SOURCES_MARKER = "[OFFICE_SOURCES]";
const WIKI_PROPOSALS_MARKER = "[OFFICE_WIKI_PROPOSALS]";
const MAX_WIKI_PROPOSALS = 3;
const MAX_WIKI_PROPOSAL_TITLE_LENGTH = 120;
const MAX_WIKI_PROPOSAL_BODY_LENGTH = 12_000;
const WIKI_PROPOSAL_FIELDS = new Set([
  "pageKey",
  "kind",
  "title",
  "body",
  "approvalTier",
]);
const WIKI_PROPOSAL_KINDS = new Set([
  "decision",
  "constraint",
  "incident",
]);
const WIKI_PROPOSAL_APPROVAL_TIERS = new Set(["peer", "user"]);
const WIKI_PROPOSAL_PAGE_KEY_PATTERN = /^[a-z0-9][a-z0-9-]{0,79}$/;

export function parseAgentEvent(line, backend, workdir = null) {
  let object;
  try {
    object = JSON.parse(line);
  } catch {
    return null;
  }
  if (!object || typeof object !== "object" || Array.isArray(object)) {
    return null;
  }

  if (backend === "claude" && object.type === "prompt_suggestion") {
    const suggestion = cleanText(object.suggestion);
    return suggestion
      ? {
          activity: activity("suggestion", suggestion, {
            eventKey: cleanText(object.uuid)
              ? `suggestion:${cleanText(object.uuid)}`
              : null,
          }),
        }
      : null;
  }

  switch (backend) {
    case "codex":
      return parseCodexEvent(object, workdir);
    case "claude":
      return parseClaudeEvent(object, workdir);
    case "antigravity":
      return parseAntigravityEvent(object, workdir);
    default:
      return null;
  }
}

export function decodeAgentResponse(value) {
  const decodedBlocks = responseMachineBlocks(String(value ?? "").trim());
  const text = decodedBlocks.text;
  const decodedNeedInput = needInputResponse(text);
  const response = {
    text: decodedNeedInput.text,
    needsInput: decodedNeedInput.needsInput,
    sources: decodedBlocks.sources,
    proposals: decodedBlocks.proposals,
    wikiProposalError: decodedBlocks.wikiProposalError,
  };
  if (decodedBlocks.sourceError) {
    response.sourceError = decodedBlocks.sourceError;
  }
  return response;
}

function needInputResponse(text) {
  if (text.startsWith(NEED_INPUT_MARKER)) {
    return {
      text: text.slice(NEED_INPUT_MARKER.length).replace(/^\s+/, ""),
      needsInput: true,
    };
  }

  let lastMarkerLine = null;
  for (const match of text.matchAll(
    /^[\t ]*\[NEED_INPUT\][\t ]*(?:\r?\n|$)/gm,
  )) {
    lastMarkerLine = match;
  }
  if (!lastMarkerLine) {
    return { text, needsInput: false };
  }

  const markerIndex = lastMarkerLine.index;
  return {
    text: `${text.slice(0, markerIndex)}${text.slice(
      markerIndex + lastMarkerLine[0].length,
    )}`.trim(),
    needsInput: true,
  };
}

function responseMachineBlocks(text) {
  let visibleText = text;
  const blocks = [];
  while (true) {
    const block = trailingResponseMachineBlock(visibleText);
    if (!block) {
      break;
    }
    blocks.unshift(block);
    visibleText = block.text;
  }

  let sources = [];
  let sourceError;
  const sourceBlocks = blocks.filter(
    (block) => block.marker === RESPONSE_SOURCES_MARKER,
  );
  if (sourceBlocks.length === 1) {
    try {
      sources = normalizeResponseSources(
        JSON.parse(unfencedMachineBlock(sourceBlocks[0].encoded)),
        { maximum: 20 },
      );
    } catch (error) {
      // 검증 규칙 위반과 JSON 문법 오류를 같은 문구로 뭉치면 직원이
      // 무엇을 고쳐야 할지 알 수 없어 다음 턴에도 같은 실수를 한다.
      // 위반 사유가 있으면 그대로 전한다.
      sourceError = error instanceof ProvenanceValidationError
        ? `응답 근거를 저장하지 못했습니다. ${error.message}`
        : "응답 근거 형식을 읽지 못했습니다.";
    }
  } else if (sourceBlocks.length > 1) {
    sourceError = "응답 근거 블록은 하나만 사용할 수 있습니다.";
  } else if (strandedMachineBlockMarker(visibleText, RESPONSE_SOURCES_MARKER)) {
    sourceError = misplacedBlockMessage("응답 근거");
  }

  let proposals = [];
  let wikiProposalError = null;
  const proposalBlocks = blocks.filter(
    (block) => block.marker === WIKI_PROPOSALS_MARKER,
  );
  if (proposalBlocks.length === 1) {
    try {
      proposals = normalizedWikiProposals(
        JSON.parse(unfencedMachineBlock(proposalBlocks[0].encoded)),
      );
    } catch {
      wikiProposalError = "위키 수정안 형식을 읽지 못했습니다.";
    }
  } else if (proposalBlocks.length > 1) {
    wikiProposalError = "위키 수정안 블록은 하나만 사용할 수 있습니다.";
  } else if (strandedMachineBlockMarker(visibleText, WIKI_PROPOSALS_MARKER)) {
    wikiProposalError = misplacedBlockMessage("위키 수정안");
  }

  return {
    text: visibleText.trim(),
    sources,
    proposals,
    wikiProposalError,
    ...(sourceError ? { sourceError } : {}),
  };
}

function misplacedBlockMessage(label) {
  return `${label} 블록은 응답 맨 끝에 두고 뒤에 아무것도 쓰지 마십시오.`;
}

/// 마커와 JSON을 적은 뒤 인사말을 더 붙이면 블록으로 인식되지 않는다.
/// 그대로 두면 근거가 조용히 사라지고 마커와 JSON이 화면에 날것으로
/// 노출되므로, 남은 본문에 그런 마커가 있는지 확인해 알린다. 정상 블록을
/// 이미 찾은 경우에는 호출하지 않으므로 형식을 설명한 글은 걸리지 않는다.
function strandedMachineBlockMarker(text, marker) {
  const pattern = new RegExp(
    `(?:^|\\n)${marker.replace(/[[\]]/g, "\\$&")}[ \\t]*(?:\\r?\\n|$)`,
    "g",
  );
  let match;
  let lastMatch = null;
  while ((match = pattern.exec(text)) !== null) {
    lastMatch = match;
  }
  if (!lastMatch) {
    return false;
  }
  const encoded = text.slice(lastMatch.index + lastMatch[0].length).trim();
  const payload = unfencedMachineBlock(encoded).trimStart();
  // 빈 배열은 형식을 설명하려고 적은 예시다. 잃을 내용이 없으므로
  // 알리지 않는다. 항목이 실제로 들어 있을 때만 사라진 것이 있다.
  return /^\[\s*\{/.test(payload) || /^\{\s*"/.test(payload);
}

function trailingResponseMachineBlock(text) {
  const markerPattern = /(?:^|\n)(\[OFFICE_(?:SOURCES|WIKI_PROPOSALS)\])[ \t]*(?:\r?\n|$)/g;
  let match;
  let lastMatch = null;
  while ((match = markerPattern.exec(text)) !== null) {
    lastMatch = {
      marker: match[1],
      markerIndex: match.index + (match[0].startsWith("\n") ? 1 : 0),
      encodedIndex: markerPattern.lastIndex,
    };
  }
  if (!lastMatch) {
    return null;
  }
  const encoded = text.slice(lastMatch.encodedIndex).trim();
  if (hasContentAfterMachinePayload(encoded)) {
    return null;
  }
  return {
    marker: lastMatch.marker,
    encoded,
    text: text.slice(0, lastMatch.markerIndex).trim(),
  };
}

function hasContentAfterMachinePayload(encoded) {
  if (encoded.startsWith("```")) {
    const openingEnd = encoded.indexOf("\n");
    const closingStart = encoded.indexOf("```", openingEnd + 1);
    return closingStart >= 0 && encoded.slice(closingStart + 3).trim() !== "";
  }
  if (!encoded.startsWith("[") && !encoded.startsWith("{")) {
    return false;
  }
  let depth = 0;
  let inString = false;
  let escaped = false;
  for (let index = 0; index < encoded.length; index += 1) {
    const character = encoded[index];
    if (inString) {
      if (escaped) {
        escaped = false;
      } else if (character === "\\") {
        escaped = true;
      } else if (character === '"') {
        inString = false;
      }
      continue;
    }
    if (character === '"') {
      inString = true;
    } else if (character === "[" || character === "{") {
      depth += 1;
    } else if (character === "]" || character === "}") {
      depth -= 1;
      if (depth === 0) {
        return encoded.slice(index + 1).trim() !== "";
      }
    }
  }
  return false;
}

function unfencedMachineBlock(encoded) {
  if (encoded.startsWith("```")) {
    return encoded
      .replace(/^```(?:json)?\s*/i, "")
      .replace(/\s*```$/, "");
  }
  return encoded;
}

export function normalizedWikiProposals(value) {
  // 이 파서는 직원이 제안한 공개 필드의 형식만 검증한다. pageKey가 실제
  // 프로젝트에 속하는지와 원본 근거가 유효한지는 이후 백엔드 저장 단계 책임이다.
  if (!Array.isArray(value) || value.length > MAX_WIKI_PROPOSALS) {
    throw new TypeError("Invalid wiki proposals array.");
  }
  return value.map((proposal) => normalizedWikiProposal(proposal));
}

function normalizedWikiProposal(value) {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new TypeError("Invalid wiki proposal.");
  }
  const fields = Object.keys(value);
  if (
    fields.length !== WIKI_PROPOSAL_FIELDS.size ||
    fields.some((field) => !WIKI_PROPOSAL_FIELDS.has(field))
  ) {
    throw new TypeError("Invalid wiki proposal fields.");
  }
  if (
    typeof value.pageKey !== "string" ||
    !WIKI_PROPOSAL_PAGE_KEY_PATTERN.test(value.pageKey) ||
    typeof value.kind !== "string" ||
    !WIKI_PROPOSAL_KINDS.has(value.kind) ||
    typeof value.title !== "string" ||
    Array.from(value.title).length > MAX_WIKI_PROPOSAL_TITLE_LENGTH ||
    typeof value.body !== "string" ||
    Array.from(value.body).length > MAX_WIKI_PROPOSAL_BODY_LENGTH ||
    typeof value.approvalTier !== "string" ||
    !WIKI_PROPOSAL_APPROVAL_TIERS.has(value.approvalTier)
  ) {
    throw new TypeError("Invalid wiki proposal values.");
  }
  return {
    pageKey: value.pageKey,
    kind: value.kind,
    title: value.title,
    body: value.body,
    approvalTier: value.approvalTier,
  };
}

function parseCodexEvent(object, workdir) {
  const type = object.type;
  if (type === "thread.started") {
    return {
      sessionID: cleanText(object.thread_id),
    };
  }
  if (type === "turn.started") {
    return null;
  }
  if (type === "turn.completed") {
    const usage = normalizedUsage(object.usage, "codex");
    return usage ? { usage } : null;
  }
  if (type === "turn.failed") {
    return {
      failure:
        failureText(object.error) ??
        "Codex 작업이 완료되지 못했습니다.",
    };
  }
  if (type === "error") {
    return {
      warning:
        failureText(object.message) ??
        failureText(object.error),
    };
  }

  if (
    !["item.started", "item.updated", "item.completed"].includes(type) ||
    !object.item
  ) {
    return null;
  }

  const item = object.item;
  const eventKey = codexEventKey(item);
  switch (item.type) {
    case "reasoning": {
      const reasoningText = codexReasoningText(item);
      if (!reasoningText) {
        return null;
      }
      return {
        activity: {
          ...activity("thinking", reasoningText, {
            eventKey,
            status: codexActivityStatus(item, type),
          }),
          isCodexReasoning: true,
        },
      };
    }
    case "agent_message": {
      const text = cleanText(item.text);
      return text
        ? { agentMessage: text, agentMessageKey: eventKey }
        : null;
    }
    case "command_execution":
      return {
        activity: activity(
          "command",
          commandActivityText(item),
          {
            eventKey,
            status: codexActivityStatus(item, type),
          },
        ),
      };
    case "file_change":
      return {
        activity: activity(
          "tool",
          type === "item.completed"
            ? fileChangeActivityText(item.changes, null, workdir)
            : fileChangeRunningText(item.changes),
          {
            eventKey,
            status: codexActivityStatus(item, type),
          },
        ),
        fileChange: {
          eventKey,
          phase: type,
          changes: fileChangeMetadata(item.changes),
        },
      };
    case "mcp_tool_call":
      return {
        activity: activity(
          "tool",
          mcpActivityText(item),
          {
            eventKey,
            status: codexActivityStatus(item, type),
          },
        ),
      };
    case "collab_tool_call":
      {
        const activities = collabActivities(item, type, eventKey);
        return activities.length > 0 ? { activities } : null;
      }
    case "web_search":
      return {
        activity: activity(
          "tool",
          webSearchActivityText(item),
          {
            eventKey,
            status: codexActivityStatus(item, type),
          },
        ),
      };
    case "error": {
      const text = cleanText(item.message ?? item.error);
      return text
        ? {
            activity: activity("tool", `오류 · ${text}`, {
              eventKey,
              status: "failed",
            }),
          }
        : null;
    }
    case "todo_list":
      return type !== "item.updated"
        ? {
            activity: activity(
              "thinking",
              todoActivityText(item),
              {
                eventKey,
                status: "completed",
              },
            ),
          }
        : null;
    default:
      return null;
  }
}

function parseClaudeEvent(object, workdir) {
  if (object.type === "system" && object.subtype === "init") {
    return {
      sessionID: cleanText(object.session_id),
      resolvedModel: cleanText(object.model),
    };
  }

  if (object.type === "stream_event") {
    const event = object.event ?? {};
    if (event.type === "message_start") {
      return {
        streamMessageID: cleanText(event.message?.id),
      };
    }
    if (
      event.type === "content_block_delta" &&
      event.delta?.type === "text_delta"
    ) {
      const delta = String(event.delta.text ?? "");
      return delta ? { responseDelta: delta } : null;
    }
    if (
      event.type === "content_block_start" &&
      event.content_block?.type === "thinking"
    ) {
      return {
        activity: activity("thinking", "추론 중", {
          eventKey: `block:${event.index ?? 0}`,
          messageScoped: true,
          status: "running",
        }),
      };
    }
    if (
      event.type === "content_block_start" &&
      event.content_block?.type === "tool_use"
    ) {
      return {
        activity: activity(
          "tool",
          claudeToolActivityText(event.content_block, workdir),
          {
            eventKey: claudeToolEventKey(event.content_block),
            status: "running",
          },
        ),
      };
    }
    return null;
  }

  if (object.type === "assistant") {
    const content = object.message?.content;
    if (!Array.isArray(content)) {
      return null;
    }
    const messageID = cleanText(object.message?.id);
    const activities = [];
    content.forEach((item, index) => {
      if (item.type === "thinking") {
        const text = reasoningText(item.thinking);
        if (text) {
          activities.push(
            activity("thinking", text, {
              eventKey: messageID
                ? `${messageID}:block:${index}`
                : null,
              status: "completed",
            }),
          );
        }
      } else if (item.type === "tool_use") {
        activities.push(
          activity("tool", claudeToolActivityText(item, workdir), {
            eventKey: claudeToolEventKey(item),
            status: "running",
          }),
        );
      }
    });
    const publicText = content
      .filter((item) => item.type === "text")
      .map((item) => String(item.text ?? ""))
      .join("");
    const result = {};
    const resolvedModel = claudeMessageModel(object);
    if (resolvedModel) {
      result.resolvedModel = resolvedModel;
    }
    if (activities.length > 0) {
      result.activities = activities;
    }
    if (publicText.trim()) {
      result.agentMessage = publicText.trim();
      result.agentMessageKey = messageID;
    }
    return Object.keys(result).length > 0 ? result : null;
  }

  if (object.type === "user") {
    const content = object.message?.content;
    if (!Array.isArray(content)) {
      return null;
    }
    const activities = content
      .filter((item) => item.type === "tool_result")
      .map((item) => activity("tool", "도구 완료", {
        eventKey: cleanText(item.tool_use_id),
        preserveText: true,
        status: item.is_error === true ? "failed" : "completed",
      }));
    return activities.length > 0 ? { activities } : null;
  }

  if (object.type === "result") {
    const sessionID = cleanText(object.session_id);
    const usage = normalizedClaudeResultUsage(object);
    if (object.is_error === true) {
      return {
        sessionID,
        usage,
        failure:
          cleanText(object.result) ??
          "Claude Code 작업이 완료되지 못했습니다.",
      };
    }
    return {
      sessionID,
      usage,
      responseText: cleanText(object.result),
    };
  }

  return null;
}

function parseAntigravityEvent(object, workdir) {
  if (object.event === "init") {
    return {
      sessionID: cleanText(
        object.conversation_id ?? object.init?.conversation_id,
      ),
    };
  }

  if (object.event === "step_update") {
    const update = object.step_update;
    if (!update || typeof update !== "object" || Array.isArray(update)) {
      return null;
    }
    const stepType = cleanText(update.step_type)?.toLowerCase();
    const state = cleanText(update.state)?.toUpperCase();
    if (
      stepType === "thinking" ||
      stepType === "thought" ||
      stepType === "reasoning"
    ) {
      const text = reasoningText(
        update.thinking ??
          update.thought ??
          update.text ??
          update.text_delta ??
          update.reasoning,
      );
      if (!text) {
        return null;
      }
      return {
        activity: activity("thinking", text, {
          eventKey: [
            "antigravity",
            cleanText(update.conversation_id),
            update.step_index,
            "thinking",
          ]
            .filter((value) => value !== null && value !== undefined)
            .join(":"),
          status: antigravityActivityStatus(state),
        }),
      };
    }
    if (stepType === "agent_response") {
      const delta = String(update.text_delta ?? "");
      const usage = state === "DONE"
        ? normalizedAntigravityUsage(update.usage)
        : null;
      const thinking = cleanText(update.thinking ?? update.thought);
      const activities = thinking
        ? [
            activity("thinking", reasoningText(thinking), {
              eventKey: [
                "antigravity",
                cleanText(update.conversation_id),
                update.step_index,
                "thinking",
              ]
                .filter((value) => value !== null && value !== undefined)
                .join(":"),
              status: antigravityActivityStatus(state),
            }),
          ]
        : [];
      if (!delta && !usage && activities.length === 0) {
        return null;
      }
      // 한 응답 단계는 ACTIVE 조각 여러 개와 DONE 조각 하나로 온다. 도구 사이에
      // 낀 중간 메시지도 단계 단위로 끊어 발생한 자리에 남길 수 있게
      // 단계 열쇠와 종료 여부를 함께 넘긴다.
      const responseStepKey = [
        "antigravity",
        cleanText(update.conversation_id),
        update.step_index,
      ]
        .filter((value) => value !== null && value !== undefined)
        .join(":");
      return {
        ...(delta ? { responseDelta: delta, responseStepKey } : {}),
        ...(state === "DONE" ? { responseStepKey, responseStepDone: true } : {}),
        ...(usage ? { usage, usageIsDelta: true } : {}),
        ...(activities.length > 0 ? { activities } : {}),
      };
    }
    if (stepType === "tool") {
      const toolInfo = update.tool_info;
      const name = cleanText(
        update.tool_name ?? toolInfo?.name,
      ) ?? "도구";
      const status = antigravityActivityStatus(state, toolInfo);
      return {
        activity: activity(
          name === "run_command" ? "command" : "tool",
          antigravityToolActivityText(name, toolInfo, workdir),
          {
            eventKey: [
              "antigravity",
              cleanText(update.conversation_id),
              update.step_index,
            ].filter((value) => value !== null && value !== undefined)
              .join(":"),
            status,
          },
        ),
      };
    }
    return null;
  }

  if (object.event === "result") {
    const result = object.result;
    if (!result || typeof result !== "object" || Array.isArray(result)) {
      return null;
    }
    const sessionID = cleanText(
      result.conversation_id ?? object.conversation_id,
    );
    const usageFallback = normalizedAntigravityUsage(result.usage);
    const status = cleanText(result.status)?.toUpperCase();
    if (status && status !== "SUCCESS") {
      return {
        sessionID,
        usageFallback,
        failure: cleanText(result.error ?? result.response) ??
          "Antigravity 작업이 완료되지 못했습니다.",
      };
    }
    // 최종 결과의 response는 단계별 메시지를 모두 이어 붙인 전문이다.
    // 단계 메시지를 하나도 받지 못했을 때만 쓰는 fallback으로 넘긴다.
    return {
      sessionID,
      usageFallback,
      responseFallback: cleanText(result.response),
    };
  }

  if (object.event === "error") {
    return {
      failure: failureText(object.error ?? object.message) ??
        "Antigravity 작업이 완료되지 못했습니다.",
    };
  }
  if (object.event === "warning") {
    return { warning: failureText(object.warning ?? object.message) };
  }
  return null;
}

function normalizedAntigravityUsage(value) {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    return null;
  }
  const usage = {
    inputTokens: tokenCount(value.input_tokens),
    outputTokens: tokenCount(value.output_tokens),
    cachedInputTokens: tokenCount(value.cache_read_tokens),
    cacheWriteInputTokens: null,
    cacheWrite5mInputTokens: null,
    cacheWrite1hInputTokens: null,
    reasoningOutputTokens: tokenCount(value.thinking_tokens),
    serviceTier: null,
    speed: null,
    inferenceGeo: null,
    reportedCostUsd: null,
  };
  return [
    usage.inputTokens,
    usage.outputTokens,
    usage.cachedInputTokens,
    usage.reasoningOutputTokens,
  ].some((entry) => entry !== null)
    ? usage
    : null;
}

function antigravityActivityStatus(state, toolInfo) {
  if (state !== "DONE") {
    return "running";
  }
  if (
    toolInfo?.error ||
    ["FAILED", "ERROR", "CANCELLED", "CANCELED"].includes(
      cleanText(toolInfo?.status)?.toUpperCase(),
    )
  ) {
    return "failed";
  }
  return "completed";
}

function antigravityToolActivityText(name, toolInfo, workdir) {
  const info = toolInfo && typeof toolInfo === "object" &&
      !Array.isArray(toolInfo)
    ? toolInfo
    : {};
  const parameters = info.parameters && typeof info.parameters === "object" &&
      !Array.isArray(info.parameters)
    ? info.parameters
    : {};
  if (name === "run_command") {
    return safeCommand(
      parameters.CommandLine ?? parameters.command_line ?? parameters.command,
    ) ?? "명령";
  }
  const path = cleanText(
    parameters.TargetFile ??
      parameters.target_file ??
      parameters.Path ??
      parameters.path ??
      parameters.FilePath ??
      parameters.file_path,
  );
  if (path) {
    return `${name} · ${compactPath(path, workdir)}`;
  }
  const query = cleanText(parameters.Query ?? parameters.query);
  return query
    ? `${name} · ${safePublicText(query, 220)}`
    : name;
}

function normalizedClaudeResultUsage(result) {
  const mainLoopUsage = normalizedUsage(result.usage, "claude", {
    reportedCostUsd: result.total_cost_usd,
  });
  const perModel = result.modelUsage ?? result.model_usage;
  if (
    !perModel ||
    typeof perModel !== "object" ||
    Array.isArray(perModel)
  ) {
    return mainLoopUsage;
  }

  const totals = {
    inputTokens: 0,
    outputTokens: 0,
    cachedInputTokens: 0,
    cacheWriteInputTokens: 0,
  };
  let hasUsage = false;
  let reportedCostUsd = nonnegativeNumber(result.total_cost_usd);
  let summedCostUsd = 0;
  let reportedSonnet5CostUsd = 0;
  let hasReportedSonnet5Cost = false;
  let hasSummedCost = false;
  for (const [model, usage] of Object.entries(perModel)) {
    if (!usage || typeof usage !== "object" || Array.isArray(usage)) {
      continue;
    }
    const fields = {
      inputTokens: usage.inputTokens ?? usage.input_tokens,
      outputTokens: usage.outputTokens ?? usage.output_tokens,
      cachedInputTokens:
        usage.cacheReadInputTokens ?? usage.cache_read_input_tokens,
      cacheWriteInputTokens:
        usage.cacheCreationInputTokens ??
        usage.cache_creation_input_tokens,
    };
    for (const [field, value] of Object.entries(fields)) {
      const count = tokenCount(value);
      if (count !== null) {
        totals[field] += count;
        hasUsage = true;
      }
    }
    const modelCost = nonnegativeNumber(
      usage.costUSD ?? usage.cost_usd,
    );
    if (modelCost !== null) {
      summedCostUsd += modelCost;
      hasSummedCost = true;
      if (String(model).startsWith("claude-sonnet-5")) {
        reportedSonnet5CostUsd += modelCost;
        hasReportedSonnet5Cost = true;
      }
    }
  }
  if (reportedCostUsd === null && hasSummedCost) {
    reportedCostUsd = summedCostUsd;
  }
  if (!hasUsage && reportedCostUsd === null) {
    return mainLoopUsage;
  }

  return {
    inputTokens: hasUsage ? totals.inputTokens : null,
    outputTokens: hasUsage ? totals.outputTokens : null,
    cachedInputTokens: hasUsage ? totals.cachedInputTokens : null,
    cacheWriteInputTokens: hasUsage
      ? totals.cacheWriteInputTokens
      : null,
    // modelUsage는 캐시 생성 총량만 제공하므로 5분/1시간 세부값을
    // 본체 루프 값으로 채워 전체 사용량처럼 보이게 하지 않는다.
    cacheWrite5mInputTokens: null,
    cacheWrite1hInputTokens: null,
    reasoningOutputTokens: mainLoopUsage?.reasoningOutputTokens ?? null,
    serviceTier: mainLoopUsage?.serviceTier ?? null,
    speed: mainLoopUsage?.speed ?? null,
    inferenceGeo: mainLoopUsage?.inferenceGeo ?? null,
    reportedCostUsd,
    reportedSonnet5CostUsd: hasReportedSonnet5Cost
      ? reportedSonnet5CostUsd
      : null,
  };
}

function activity(kind, text, options = {}) {
  const value = {
    kind,
    text,
    eventKey: options.eventKey ?? null,
    status: options.status ?? "completed",
    preserveText: options.preserveText === true,
    messageScoped: options.messageScoped === true,
  };
  if (options.collaboration) {
    value.collaboration = options.collaboration;
  }
  return value;
}

function normalizedUsage(value, backend, options = {}) {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    return null;
  }
  const cacheCreation = value.cache_creation &&
      typeof value.cache_creation === "object"
    ? value.cache_creation
    : {};
  const usage = {
    inputTokens: tokenCount(value.input_tokens),
    outputTokens: tokenCount(value.output_tokens),
    cachedInputTokens: tokenCount(
      backend === "codex"
        ? value.cached_input_tokens
        : value.cache_read_input_tokens,
    ),
    cacheWriteInputTokens: tokenCount(
      backend === "codex"
        ? value.cache_write_input_tokens
        : value.cache_creation_input_tokens,
    ),
    cacheWrite5mInputTokens: tokenCount(
      cacheCreation.ephemeral_5m_input_tokens,
    ),
    cacheWrite1hInputTokens: tokenCount(
      cacheCreation.ephemeral_1h_input_tokens,
    ),
    reasoningOutputTokens: tokenCount(value.reasoning_output_tokens),
    serviceTier: cleanText(value.service_tier),
    speed: cleanText(value.speed),
    inferenceGeo: cleanText(value.inference_geo),
    reportedCostUsd: nonnegativeNumber(options.reportedCostUsd),
  };
  const hasTokenOrCost = [
    usage.inputTokens,
    usage.outputTokens,
    usage.cachedInputTokens,
    usage.cacheWriteInputTokens,
    usage.cacheWrite5mInputTokens,
    usage.cacheWrite1hInputTokens,
    usage.reasoningOutputTokens,
    usage.reportedCostUsd,
  ].some((entry) => entry !== null);
  return hasTokenOrCost ? usage : null;
}

// Claude 세션 기록 한 줄에서 사용량을 읽는다. 중단으로 결과 이벤트를
// 받지 못한 턴의 사용량을 디스크에서 복구할 때 쓴다.
export function claudeSessionUsage(line) {
  let object;
  try {
    object = JSON.parse(line);
  } catch {
    return null;
  }
  return claudeMessageUsage(object);
}

export function claudeMessageUsage(object) {
  if (!object || typeof object !== "object" || Array.isArray(object)) {
    return null;
  }
  return normalizedUsage(object.message?.usage, "claude");
}

// 응답 메시지에 기록된 실제 모델 ID를 읽는다. 하위 에이전트 응답과 CLI가
// 스스로 만든 `<synthetic>` 메시지는 이 턴의 모델이 아니므로 건너뛴다.
export function claudeMessageModel(object) {
  if (
    !object || typeof object !== "object" || Array.isArray(object) ||
    object.isSidechain === true || cleanText(object.parent_tool_use_id)
  ) {
    return null;
  }
  const model = cleanText(object.message?.model);
  return model && model !== "<synthetic>" ? model : null;
}

// Claude 세션 기록은 같은 응답을 여러 줄로 남기므로, 합산할 때 중복을 걸러낼
// 열쇠가 필요하다. 응답 식별자가 없으면 줄 자체의 식별자를 쓴다.
export function claudeSessionUsageKey(object) {
  if (!object || typeof object !== "object" || Array.isArray(object)) {
    return null;
  }
  return cleanText(object.message?.id) ?? cleanText(object.uuid);
}

// Codex rollout의 token_count 한 줄에서 그 요청 한 번의 사용량을 읽는다.
// 누적 총량은 압축으로 되돌아가므로 턴 합산에는 요청 단위 값을 쓴다.
export function codexRequestUsage(object) {
  if (!object || typeof object !== "object" || Array.isArray(object)) {
    return null;
  }
  const payload = object.payload;
  if (object.type !== "event_msg" || payload?.type !== "token_count") {
    return null;
  }
  return normalizedUsage(payload.info?.last_token_usage, "codex");
}

function tokenCount(value) {
  const number = nonnegativeNumber(value);
  return number === null ? null : Math.trunc(number);
}

function nonnegativeNumber(value) {
  if (typeof value === "number" && Number.isFinite(value) && value >= 0) {
    return value;
  }
  if (typeof value === "string" && /^\d+(?:\.\d+)?$/.test(value)) {
    return Number(value);
  }
  return null;
}

function commandActivityText(item) {
  return safeCommand(item.command) ?? "명령";
}

function codexActivityStatus(item, eventType) {
  if (eventType !== "item.completed") {
    return "running";
  }
  const exitCode = numericValue(item.exit_code ?? item.exitCode);
  if (exitCode !== null) {
    return exitCode === 0 ? "completed" : "failed";
  }
  const status = cleanText(item.status)?.toLowerCase();
  return [
    "failed",
    "error",
    "cancelled",
    "canceled",
    "declined",
  ].includes(status) ||
      item.error
    ? "failed"
    : "completed";
}

export function fileChangeActivityText(
  changesValue,
  statistics = null,
  workdir = null,
) {
  const changes = Array.isArray(changesValue) ? changesValue : [];
  const summaries = changes
    .map((change) => {
      if (!change || typeof change !== "object") {
        return null;
      }
      const path = cleanText(
        change.path ??
          change.file_path ??
          change.file ??
          change.move_path,
      );
      if (!path) {
        return null;
      }
      return `${fileChangeLabel(change.kind)} ${compactPath(path, workdir)}`;
    })
    .filter(Boolean);
  if (summaries.length === 0) {
    return "파일 변경 완료";
  }

  const visible = summaries.slice(0, 40);
  const remainder = summaries.length - visible.length;
  return [
    `파일 ${summaries.length}개를 편집했습니다`,
    statistics
      ? `+${statistics.additions} -${statistics.deletions}`
      : null,
    ...visible,
    remainder > 0 ? `외 ${remainder}개` : null,
  ]
    .filter(Boolean)
    .join("\n");
}

function fileChangeRunningText(changesValue) {
  const changes = fileChangeMetadata(changesValue);
  if (changes.length === 0) {
    return "파일 변경을 적용하는 중";
  }
  return `파일 변경을 적용하는 중 · ${changes.length}개`;
}

function fileChangeMetadata(changesValue) {
  const changes = Array.isArray(changesValue) ? changesValue : [];
  return changes.flatMap((change) => {
    if (!change || typeof change !== "object") {
      return [];
    }
    const path = cleanText(
      change.path ??
        change.file_path ??
        change.file ??
        change.move_path,
    );
    return path ? [{ path, kind: cleanText(change.kind) }] : [];
  });
}

function mcpActivityText(item) {
  const server = cleanText(
    item.server ?? item.server_name ?? item.appName ?? item.app_name,
  );
  const tool = cleanText(
    item.tool ?? item.name ?? item.actionName ?? item.action_name,
  );
  const target = [server, tool].filter(Boolean).join("/") || "연결 도구";
  return target;
}

function collabActivities(item, eventType, eventKey) {
  if (eventType !== "item.completed") {
    return [];
  }

  const tool = normalizedCollabTool(item.tool ?? item.name ?? item.action);
  if (tool === "close_agent") {
    return [];
  }

  const callStatus = codexActivityStatus(item, eventType);
  const prompt = limitedCollabText(
    item.prompt ?? item.message ?? item.input,
    MAX_COLLABORATION_PROMPT_LENGTH,
  );
  const agentStates = collabAgentStates(item);
  const threadIDs = collabThreadIDs(item, agentStates);
  const agentLabel = threadIDs.length === 1
    ? cleanText(
      item.receiver_agent_nickname ??
        item.receiver_agent ??
        item.agent_name ??
        item.agent,
    )
    : null;

  if (tool === "wait") {
    return threadIDs.flatMap((threadID) => {
      const state = agentStates[threadID] ?? {};
      const agentStatus = normalizedCollabAgentStatus(state.status);
      const message = limitedCollabText(
        state.message ?? state.result ?? state.output,
        MAX_COLLABORATION_RESULT_LENGTH,
      );
      if (!message && ["pending_init", "running"].includes(agentStatus)) {
        return [];
      }
      const status = collabAgentActivityStatus(
        agentStatus,
        callStatus,
        "result",
      );
      return [activity(
        "collaboration",
        message
          ? collaborationSummaryText(message)
          : collaborationStatusText(status),
        {
          eventKey: collaborationEventKey(eventKey, threadID),
          status,
          collaboration: {
            action: "result",
            agentThreadId: threadID,
            agentLabel,
            message,
            agentStatus,
          },
        },
      )];
    });
  }

  if (tool === "spawn_agent") {
    if (threadIDs.length === 0) {
      if (callStatus !== "failed") {
        return [];
      }
      return [activity(
        "collaboration",
        "협업 검토를 시작하지 못했습니다.",
        {
          eventKey,
          status: "failed",
          collaboration: {
            action: "spawn",
            agentLabel,
            prompt,
            agentStatus: "errored",
          },
        },
      )];
    }
    return threadIDs.map((threadID) => {
      const state = agentStates[threadID] ?? {};
      const agentStatus = normalizedCollabAgentStatus(state.status) ??
        (callStatus === "failed" ? "errored" : "running");
      const status = collabAgentActivityStatus(
        agentStatus,
        callStatus,
        "spawn",
      );
      return activity(
        "collaboration",
        prompt
          ? collaborationSummaryText(prompt)
          : collaborationStatusText(status),
        {
          eventKey: collaborationEventKey(eventKey, threadID),
          status,
          collaboration: {
            action: "spawn",
            agentThreadId: threadID,
            agentLabel,
            prompt,
            agentStatus,
          },
        },
      );
    });
  }

  if (tool === "send_input") {
    if (threadIDs.length === 0) {
      return [];
    }
    return threadIDs.map((threadID) => {
      const state = agentStates[threadID] ?? {};
      const agentStatus = normalizedCollabAgentStatus(state.status) ??
        (callStatus === "failed" ? "errored" : "running");
      const status = collabAgentActivityStatus(
        agentStatus,
        callStatus,
        "follow_up",
      );
      return activity(
        "collaboration",
        prompt
          ? collaborationSummaryText(prompt)
          : collaborationStatusText(status),
        {
          eventKey: collaborationEventKey(eventKey, threadID),
          status,
          collaboration: {
            action: "follow_up",
            agentThreadId: threadID,
            agentLabel,
            prompt,
            agentStatus,
          },
        },
      );
    });
  }

  if (threadIDs.length === 0) {
    return [];
  }
  return threadIDs.map((threadID) => activity(
    "collaboration",
    prompt ? collaborationSummaryText(prompt) : "협업 작업",
    {
      eventKey: collaborationEventKey(eventKey, threadID),
      status: callStatus,
      collaboration: {
        action: "other",
        agentThreadId: threadID,
        agentLabel,
        prompt,
      },
    },
  ));
}

function normalizedCollabTool(value) {
  const tool = cleanText(value)?.replaceAll("-", "_").toLowerCase();
  switch (tool) {
    case "spawnagent":
      return "spawn_agent";
    case "sendinput":
      return "send_input";
    case "closeagent":
      return "close_agent";
    default:
      return tool ?? "collaboration";
  }
}

function collabAgentStates(item) {
  const value = item.agents_states ?? item.agent_states ?? item.agentsStates;
  return value && typeof value === "object" && !Array.isArray(value)
    ? value
    : {};
}

function collabThreadIDs(item, states) {
  const values = [
    ...(Array.isArray(item.receiver_thread_ids)
      ? item.receiver_thread_ids
      : []),
    ...(Array.isArray(item.receiverThreadIds)
      ? item.receiverThreadIds
      : []),
    cleanText(item.receiver_thread_id ?? item.receiverThreadId),
    ...Object.keys(states),
  ];
  return [...new Set(values.map(cleanText).filter(Boolean))];
}

function normalizedCollabAgentStatus(value) {
  const status = cleanText(value)?.replaceAll("-", "_").toLowerCase();
  return status || null;
}

function collabAgentActivityStatus(agentStatus, callStatus, action) {
  if (callStatus === "failed") {
    return "failed";
  }
  if (["interrupted", "errored", "not_found"].includes(agentStatus)) {
    return "failed";
  }
  if (["completed", "shutdown"].includes(agentStatus)) {
    return "completed";
  }
  if (["pending_init", "running"].includes(agentStatus)) {
    return "running";
  }
  return action === "spawn" || action === "follow_up"
    ? "running"
    : callStatus;
}

function collaborationEventKey(eventKey, threadID) {
  return ["collaboration", eventKey, threadID]
    .filter(Boolean)
    .join(":");
}

function collaborationSummaryText(value) {
  return safePublicText(value, 320);
}

function collaborationStatusText(status) {
  switch (status) {
    case "running":
      return "협업 검토 중";
    case "failed":
      return "협업 검토에 실패했습니다.";
    default:
      return "협업 검토를 마쳤습니다.";
  }
}

function limitedCollabText(value, limit) {
  const text = cleanText(value)?.replaceAll("\r\n", "\n");
  if (!text) {
    return null;
  }
  return text.length <= limit
    ? text
    : `${text.slice(0, limit - 1)}…`;
}

function webSearchActivityText(item) {
  const action = item.action && typeof item.action === "object"
    ? item.action
    : {};
  const detail = cleanText(
    item.query ?? action.query ?? action.pattern ?? action.url,
  );
  return detail
    ? `검색 · ${safePublicText(detail, 220)}`
    : "웹 검색";
}

function todoActivityText(item) {
  const entries = Array.isArray(item.items)
    ? item.items
    : Array.isArray(item.todos)
    ? item.todos
    : [];
  const pending = entries.find((entry) => {
    if (!entry || typeof entry !== "object") {
      return false;
    }
    return entry.completed !== true && entry.status !== "completed";
  });
  const text = pending && typeof pending === "object"
    ? cleanText(pending.text ?? pending.step ?? pending.title)
    : null;
  return text
    ? `계획 · ${safePublicText(text, 220)}`
    : entries.length > 0
    ? `계획 · ${entries.length}단계`
    : "작업 계획 정리";
}

function claudeToolActivityText(tool, workdir) {
  if (!tool || typeof tool !== "object") {
    return "도구 호출";
  }
  const name = cleanText(tool.name) ?? "도구";
  const input = tool.input && typeof tool.input === "object"
    ? tool.input
    : {};
  const loweredName = name.toLowerCase();
  if (["bash", "shell", "terminal"].includes(loweredName)) {
    const command = safeCommand(input.command);
    return command ? `도구 · ${name} · ${command}` : `도구 · ${name}`;
  }
  if (loweredName === "todowrite") {
    return claudePlanActivityText(name, input);
  }
  if (loweredName === "task") {
    const detail = [
      cleanText(input.subagent_type ?? input.subagentType),
      cleanText(input.description),
    ]
      .filter(Boolean)
      .join(" · ");
    return detail
      ? `도구 · ${name} · ${safePublicText(detail, 180)}`
      : `도구 · ${name}`;
  }

  const path = cleanText(
    input.file_path ??
      input.notebook_path ??
      input.path ??
      input.cwd ??
      input.directory,
  );
  if (path) {
    const header = `도구 · ${name} · ${compactPath(path, workdir)}`;
    const stats = claudeEditStats(loweredName, input);
    return stats ? `${header}\n+${stats.additions} -${stats.deletions}` : header;
  }
  if (["grep", "glob", "websearch"].includes(loweredName)) {
    const query = cleanText(input.pattern ?? input.query);
    if (query) {
      return `도구 · ${name} · ${safePublicText(query, 180)}`;
    }
  }
  return `도구 · ${name}`;
}

// Claude CLI는 Codex와 달리 편집 통계를 따로 주지 않는다. 도구 입력에
// 담긴 편집 전후 문자열로 줄 수를 직접 센다. 파일 전체를 새로 쓰는
// Write는 이전 내용을 알 수 없으므로 추가 줄만 센다.
function claudeEditStats(loweredName, input) {
  const countLines = (value) => {
    if (typeof value !== "string" || value.length === 0) {
      return 0;
    }
    return value.split("\n").length;
  };

  if (loweredName === "edit") {
    if (
      typeof input.old_string !== "string" ||
      typeof input.new_string !== "string"
    ) {
      return null;
    }
    return {
      additions: countLines(input.new_string),
      deletions: countLines(input.old_string),
    };
  }

  if (loweredName === "multiedit") {
    if (!Array.isArray(input.edits) || input.edits.length === 0) {
      return null;
    }
    let additions = 0;
    let deletions = 0;
    for (const edit of input.edits) {
      if (!edit || typeof edit !== "object") {
        return null;
      }
      if (
        typeof edit.old_string !== "string" ||
        typeof edit.new_string !== "string"
      ) {
        return null;
      }
      additions += countLines(edit.new_string);
      deletions += countLines(edit.old_string);
    }
    return { additions, deletions };
  }

  if (loweredName === "write") {
    if (typeof input.content !== "string") {
      return null;
    }
    return { additions: countLines(input.content), deletions: 0 };
  }

  if (loweredName === "notebookedit") {
    if (typeof input.new_source !== "string") {
      return null;
    }
    const isDelete = cleanText(input.edit_mode) === "delete";
    return isDelete
      ? { additions: 0, deletions: countLines(input.new_source) }
      : { additions: countLines(input.new_source), deletions: 0 };
  }

  return null;
}

function claudePlanActivityText(name, input) {
  const entries = Array.isArray(input.todos)
    ? input.todos
    : Array.isArray(input.items)
    ? input.items
    : [];
  const steps = entries.flatMap((entry) => {
    if (!entry || typeof entry !== "object") {
      return [];
    }
    const text = cleanText(entry.content ?? entry.text ?? entry.title);
    if (!text) {
      return [];
    }
    const status = cleanText(entry.status)?.toLowerCase();
    const done = entry.completed === true || status === "completed";
    const marker = done ? "x" : status === "in_progress" ? "~" : " ";
    return [`[${marker}] ${safePublicText(text, 160)}`];
  });
  if (steps.length === 0) {
    return `도구 · ${name}`;
  }

  const done = steps.filter((step) => step.startsWith("[x]")).length;
  const visible = steps.slice(0, 12);
  const remainder = steps.length - visible.length;
  return [
    `도구 · ${name} · ${done}/${steps.length}단계`,
    ...visible,
    remainder > 0 ? `외 ${remainder}개` : null,
  ]
    .filter(Boolean)
    .join("\n");
}

function codexEventKey(item) {
  return cleanText(item.id ?? item.item_id ?? item.itemId);
}

function claudeToolEventKey(tool) {
  return cleanText(tool?.id ?? tool?.tool_use_id);
}

function codexReasoningText(item) {
  const raw = nestedReasoningText(
    item.raw_reasoning ??
      item.rawReasoning ??
      item.raw_content ??
      item.rawContent ??
      item.content,
  );
  return reasoningText(raw) ??
    reasoningText(item.text) ??
    reasoningText(item.summary);
}

function nestedReasoningText(value) {
  if (typeof value === "string") {
    return value;
  }
  if (Array.isArray(value)) {
    return value
      .map((entry) => nestedReasoningText(entry))
      .filter(Boolean)
      .join("\n");
  }
  if (!value || typeof value !== "object") {
    return null;
  }
  return nestedReasoningText(
    value.text ?? value.content ?? value.reasoning ?? value.summary,
  );
}

function reasoningText(value) {
  const text = cleanText(value);
  if (!text) {
    return null;
  }
  return text.length <= MAX_REASONING_LENGTH
    ? text
    : `${text.slice(0, MAX_REASONING_LENGTH - 1)}…`;
}

function safeCommand(value) {
  const source = cleanText(value);
  if (!source) {
    return null;
  }
  const command = source.replace(/\s*\n\s*/g, " ↳ ");
  if (containsSensitiveCommandData(command)) {
    return `${commandProgram(command)} [민감 인자 숨김]`;
  }
  return safePublicText(command, 280);
}

function containsSensitiveCommandData(command) {
  return /(?:api[_-]?key|token|password|passwd|secret|authorization|cookie|bearer)/i
    .test(command) ||
    /\b(?:sk|rk|pk)-[a-z0-9_-]{8,}/i.test(command) ||
    /:\/\/[^/\s:@]+:[^@\s/]+@/.test(command);
}

function commandProgram(command) {
  const tokens = command.trim().split(/\s+/);
  const token = tokens.find((candidate) => !/^[A-Za-z_][A-Za-z0-9_]*=/.test(candidate));
  return cleanText(token) ?? "명령";
}

function safePublicText(value, limit) {
  const source = String(value ?? "")
    .replaceAll("\r\n", "\n")
    .replace(/\s*\n\s*/g, " ↳ ")
    .trim();
  if (source.length <= limit) {
    return source;
  }
  return `${source.slice(0, limit - 1)}…`;
}

function compactPath(value, workdir = null) {
  const source = String(value);
  const base = String(workdir ?? "");
  const relativePath = base
    ? isAbsolute(source) ? relative(base, source) : source
    : null;
  const normalizedRelativePath = relativePath?.replaceAll("\\", "/");
  if (
    normalizedRelativePath &&
    normalizedRelativePath !== ".." &&
    !normalizedRelativePath.startsWith("../") &&
    !isAbsolute(relativePath)
  ) {
    return normalizedRelativePath;
  }

  const path = source.replaceAll("\\", "/");
  if (path.length <= 96) {
    return path;
  }
  const components = path.split("/").filter(Boolean);
  return components.length > 3
    ? `…/${components.slice(-3).join("/")}`
    : `…${path.slice(-92)}`;
}

function fileChangeLabel(value) {
  switch (cleanText(value)?.toLowerCase()) {
    case "add":
    case "create":
      return "추가";
    case "delete":
    case "remove":
      return "삭제";
    case "move":
    case "rename":
      return "이동";
    default:
      return "수정";
  }
}

function numericValue(value) {
  if (typeof value === "number" && Number.isFinite(value)) {
    return value;
  }
  if (typeof value === "string" && /^-?\d+$/.test(value)) {
    return Number(value);
  }
  return null;
}

function cleanText(value) {
  const text = typeof value === "string" ? value.trim() : "";
  return text || null;
}

function failureText(value) {
  if (typeof value === "string") {
    return cleanText(value);
  }
  if (!value || typeof value !== "object") {
    return null;
  }

  const parts = ["message", "detail", "code", "type", "error"]
    .map((key) => failureText(value[key]))
    .filter(Boolean);
  return [...new Set(parts)].join("\n") || null;
}
