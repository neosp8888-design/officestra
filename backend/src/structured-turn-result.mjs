// 이 파일은 직원의 자연어 응답과 근거·위키 제안 메타데이터를 분리한다.

import { createHash, randomUUID } from "node:crypto";
import {
  chmodSync,
  lstatSync,
  mkdirSync,
  readFileSync,
  renameSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { normalizedWikiProposals } from "./agent-event-parser.mjs";
import { normalizeResponseSources } from "./work-record-provenance.mjs";

const RESULT_VERSION = 1;
const MAX_RESULT_BYTES = 256 * 1024;
const LEGACY_PROTOCOL_PREFIX = "\n근거를 쓴 응답";
const LEGACY_SOURCES_MARKER = "\n[OFFICE_SOURCES]";

export const STRUCTURED_RESULT_ENV = "OFFICESTRA_RESULT_PATH";
export const STRUCTURED_RESULT_GUIDANCE =
  "답변 본문에 근거·위키 JSON을 쓰지 않는다. 실제 사용한 근거나 지속 지식 제안이 있을 때만 완료 전에 `officestra-result` 명령으로 별도 제출한다(`officestra-result --help`).";
export const structuredResultToolDirectory = dirname(
  fileURLToPath(import.meta.url),
);

export function identityPromptWithStructuredResult(value, characterID = null) {
  const raw = String(value ?? "");
  const sourcesMarkerIndex = raw.indexOf(LEGACY_SOURCES_MARKER);
  const legacyPrefixIndex = raw.indexOf(LEGACY_PROTOCOL_PREFIX);
  const legacyIndex = sourcesMarkerIndex >= 0 &&
      legacyPrefixIndex >= 0 &&
      legacyPrefixIndex < sourcesMarkerIndex
    ? legacyPrefixIndex
    : sourcesMarkerIndex;
  const guidanceIndex = raw.indexOf(`\n${STRUCTURED_RESULT_GUIDANCE}`);
  const protocolIndexes = [legacyIndex, guidanceIndex]
    .filter((index) => index >= 0);
  const protocolIndex = protocolIndexes.length > 0
    ? Math.min(...protocolIndexes)
    : -1;
  const identity = (protocolIndex >= 0 ? raw.slice(0, protocolIndex) : raw)
    .trimEnd();
  const senderGuidance = characterID
    ? `직원에게 POST /api/agent-jobs로 메시지를 보낼 때 JSON에 senderCharacterId: ${JSON.stringify(characterID)}를 포함한다. 수신 직원은 characterId에 지정한다. 이는 말풍선 발신자 표시용이며 권한 증명이 아니다.`
    : null;
  return [identity, "", STRUCTURED_RESULT_GUIDANCE, ...(senderGuidance ? [senderGuidance] : [])]
    .filter((part, index) => index !== 0 || part.length > 0)
    .join("\n");
}

export function structuredTurnResultPath({
  workdir,
  characterID,
  runtimeID = process.pid,
}) {
  const workdirKey = createHash("sha256")
    .update(String(workdir ?? ""))
    .digest("hex")
    .slice(0, 20);
  const safeCharacterID = String(characterID ?? "unknown")
    .replace(/[^A-Za-z0-9._-]/g, "_")
    .slice(0, 80) || "unknown";
  return join(
    tmpdir(),
    "officestra-turn-results",
    String(runtimeID),
    workdirKey,
    `${safeCharacterID}.json`,
  );
}

export function prepareStructuredTurnResult(options) {
  const path = structuredTurnResultPath(options);
  rmSync(`${path}.lock`, { recursive: true, force: true });
  writeStoredState(path, emptyStoredState());
  return path;
}

export function discardStructuredTurnResult(path) {
  if (!path) return;
  rmSync(path, { force: true });
  rmSync(`${path}.lock`, { recursive: true, force: true });
}

export function submitStructuredResponseSource(path, value) {
  return withStoredStateLock(path, () => {
    const state = readStoredState(path);
    const sources = publicResponseSources(
      normalizeResponseSources([...state.sources, value], { maximum: 20 }),
    );
    writeStoredState(path, {
      ...state,
      sourcesSubmitted: true,
      sources,
    });
    return sources.length;
  });
}

export function submitStructuredWikiProposal(path, value) {
  return withStoredStateLock(path, () => {
    const state = readStoredState(path);
    const wikiProposals = normalizedWikiProposals([
      ...state.wikiProposals,
      value,
    ]);
    writeStoredState(path, {
      ...state,
      wikiProposalsSubmitted: true,
      wikiProposals,
    });
    return wikiProposals.length;
  });
}

export function inspectStructuredTurnResult(path) {
  return readStoredState(path);
}

export function readStructuredTurnResult(path) {
  if (!path) return emptyConsumedResult();
  try {
    const state = readStoredState(path);
    return {
      sourcesSubmitted: state.sourcesSubmitted,
      wikiProposalsSubmitted: state.wikiProposalsSubmitted,
      sources: normalizeResponseSources(state.sources, { maximum: 20 }),
      proposals: normalizedWikiProposals(state.wikiProposals),
      metadataError: null,
    };
  } catch (error) {
    return {
      ...emptyConsumedResult(),
      metadataError:
        "별도 응답 메타데이터를 읽지 못했습니다. " +
        (error instanceof Error ? error.message : String(error)),
    };
  }
}

export function consumeStructuredTurnResult(path) {
  try {
    return readStructuredTurnResult(path);
  } finally {
    discardStructuredTurnResult(path);
  }
}

export function applyStructuredTurnResult(decoded, structured) {
  const result = { ...decoded };
  if (structured?.metadataError) {
    result.sourceError = result.sourceError ?? structured.metadataError;
    result.wikiProposalError =
      result.wikiProposalError ?? structured.metadataError;
    return result;
  }
  if (structured?.sourcesSubmitted) {
    result.sources = structured.sources;
    delete result.sourceError;
  }
  if (structured?.wikiProposalsSubmitted) {
    result.proposals = structured.proposals;
    result.wikiProposalError = null;
  }
  return result;
}

function emptyStoredState() {
  return {
    version: RESULT_VERSION,
    sourcesSubmitted: false,
    wikiProposalsSubmitted: false,
    sources: [],
    wikiProposals: [],
  };
}

function emptyConsumedResult() {
  return {
    sourcesSubmitted: false,
    wikiProposalsSubmitted: false,
    sources: [],
    proposals: [],
    metadataError: null,
  };
}

function readStoredState(path) {
  if (!path) {
    throw new TypeError("OFFICESTRA_RESULT_PATH가 없습니다.");
  }
  const statistics = lstatSync(path);
  if (!statistics.isFile() || statistics.isSymbolicLink()) {
    throw new TypeError("응답 메타데이터 경로가 일반 파일이 아닙니다.");
  }
  if (statistics.size > MAX_RESULT_BYTES) {
    throw new TypeError("응답 메타데이터가 허용 크기를 초과했습니다.");
  }
  const parsed = JSON.parse(readFileSync(path, "utf8"));
  if (
    !parsed ||
    typeof parsed !== "object" ||
    Array.isArray(parsed) ||
    parsed.version !== RESULT_VERSION ||
    typeof parsed.sourcesSubmitted !== "boolean" ||
    typeof parsed.wikiProposalsSubmitted !== "boolean" ||
    !Array.isArray(parsed.sources) ||
    !Array.isArray(parsed.wikiProposals)
  ) {
    throw new TypeError("응답 메타데이터 파일 형식이 올바르지 않습니다.");
  }
  return {
    version: RESULT_VERSION,
    sourcesSubmitted: parsed.sourcesSubmitted,
    wikiProposalsSubmitted: parsed.wikiProposalsSubmitted,
    sources: publicResponseSources(
      normalizeResponseSources(parsed.sources, { maximum: 20 }),
    ),
    wikiProposals: normalizedWikiProposals(parsed.wikiProposals),
  };
}

function writeStoredState(path, state) {
  const directory = dirname(path);
  mkdirSync(directory, { recursive: true, mode: 0o700 });
  const temporaryPath = `${path}.${process.pid}.${randomUUID()}.tmp`;
  try {
    writeFileSync(temporaryPath, `${JSON.stringify(state)}\n`, {
      encoding: "utf8",
      flag: "wx",
      mode: 0o600,
    });
    renameSync(temporaryPath, path);
    chmodSync(path, 0o600);
  } finally {
    rmSync(temporaryPath, { force: true });
  }
}

function withStoredStateLock(path, operation) {
  const lockPath = `${path}.lock`;
  let locked = false;
  for (let attempt = 0; attempt < 200; attempt += 1) {
    try {
      mkdirSync(lockPath, { mode: 0o700 });
      locked = true;
      break;
    } catch (error) {
      if (error?.code !== "EEXIST") throw error;
      Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 10);
    }
  }
  if (!locked) {
    throw new Error("응답 메타데이터 제출 잠금을 얻지 못했습니다.");
  }
  try {
    return operation();
  } finally {
    rmSync(lockPath, { recursive: true, force: true });
  }
}

function publicResponseSources(sources) {
  return sources.map((source) => ({
    kind: source.sourceKind,
    title: source.title,
    locator: source.locator,
    ...(source.excerpt ? { excerpt: source.excerpt } : {}),
    ...(source.ragDocumentID
      ? { ragDocumentId: source.ragDocumentID }
      : {}),
    ...(source.workRecordID
      ? { workRecordId: source.workRecordID }
      : {}),
    ...(Object.keys(source.metadata ?? {}).length > 0
      ? { metadata: source.metadata }
      : {}),
  }));
}
