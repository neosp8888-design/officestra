// 이 파일은 자연어 답변과 분리된 응답 메타데이터 통로를 검증한다.

import assert from "node:assert/strict";
import {
  existsSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { spawn, spawnSync } from "node:child_process";
import test from "node:test";

import {
  STRUCTURED_RESULT_ENV,
  applyStructuredTurnResult,
  consumeStructuredTurnResult,
  identityPromptWithStructuredResult,
  inspectStructuredTurnResult,
  prepareStructuredTurnResult,
  readStructuredTurnResult,
  structuredResultToolDirectory,
  submitStructuredResponseSource,
  submitStructuredWikiProposal,
} from "../src/structured-turn-result.mjs";

const RAG_DOCUMENT_ID = "11111111-1111-4111-8111-111111111111";
const WORK_RECORD_ID = "22222222-2222-4222-8222-222222222222";

test('직원 API 발신자 안내는 실제 직원 ID를 사용하고 중복되지 않는다', () => {
  const once = identityPromptWithStructuredResult('업무 지침', 'right-man');
  assert.match(once, /senderCharacterId: "right-man"/);
  assert.equal(identityPromptWithStructuredResult(once, 'right-man'), once);
  assert.doesNotMatch(identityPromptWithStructuredResult('사용자 지침'), /senderCharacterId/);
});

test("후속 응답용 근거 조회는 파일을 소비하지 않고 새 근거도 보존한다", () => {
  const path = prepareStructuredTurnResult({ workdir: "/followup-test", characterID: "followup-test" });
  try {
    submitStructuredResponseSource(path, { kind: "file", title: "첫 근거", locator: "first.mjs" });
    assert.equal(readStructuredTurnResult(path).sources.length, 1);
    assert.equal(existsSync(path), true);
    submitStructuredResponseSource(path, { kind: "file", title: "후속 근거", locator: "last.mjs" });
    assert.equal(consumeStructuredTurnResult(path).sources.length, 2);
    assert.equal(existsSync(path), false);
  } finally { rmSync(path, { force: true }); }
});

test("시스템 지침에서는 긴 JSON 계약을 제거하고 내부 결과 통로만 짧게 안내한다", () => {
  const prompt = `역할 지침

근거를 쓴 응답 끝에는 실제 쓴 근거만 남긴다. 형식이 틀리면 전체가 버려진다.
[OFFICE_SOURCES]
[{"kind":"file"}]
[OFFICE_WIKI_PROPOSALS]
[]`;
  const result = identityPromptWithStructuredResult(prompt);

  assert.match(result, /^역할 지침/);
  assert.match(result, /officestra-result/);
  assert.doesNotMatch(result, /OFFICE_SOURCES|OFFICE_WIKI_PROPOSALS/);
  assert.equal((result.match(/officestra-result/g) ?? []).length, 2);
  assert.equal(identityPromptWithStructuredResult(result), result);
});

test("표현이 다른 기존 근거 지침도 마커 앞에서 제거한다", () => {
  const result = identityPromptWithStructuredResult(`역할 지침

완료 보고 형식을 지킨다.

근거를 쓴 응답 맨 끝에는 실제 쓴 근거만 남긴다.
[OFFICE_SOURCES]
[]
[OFFICE_WIKI_PROPOSALS]
[]`);

  assert.match(result, /완료 보고 형식을 지킨다/);
  assert.doesNotMatch(result, /근거를 쓴 응답|OFFICE_SOURCES|OFFICE_WIKI/);
  assert.match(result, /officestra-result/);
});

test("근거와 위키 제안을 별도 파일에 검증해 저장하고 한 번만 소비한다", () => {
  const root = mkdtempSync(join(tmpdir(), "officestra-result-test-"));
  const path = prepareStructuredTurnResult({
    workdir: root,
    characterID: "right-woman",
    runtimeID: "unit",
  });
  try {
    assert.equal(submitStructuredResponseSource(path, {
      kind: "rag",
      title: "과거 기록",
      locator: `rag_documents:${RAG_DOCUMENT_ID}`,
      ragDocumentId: RAG_DOCUMENT_ID,
      excerpt: "0.65 1위",
    }), 1);
    assert.equal(submitStructuredResponseSource(path, {
      kind: "database",
      title: "업무 기록",
      locator: `work_records:${WORK_RECORD_ID}`,
      workRecordId: WORK_RECORD_ID,
    }), 2);
    assert.equal(submitStructuredWikiProposal(path, {
      pageKey: "response-metadata-channel",
      kind: "decision",
      title: "응답 메타데이터 분리",
      body: "근거와 위키 제안은 자연어 응답과 분리된 통로로 제출한다.",
      approvalTier: "user",
    }), 1);

    const stored = inspectStructuredTurnResult(path);
    assert.equal(stored.sourcesSubmitted, true);
    assert.equal(stored.wikiProposalsSubmitted, true);
    assert.equal(stored.sources.length, 2);
    assert.equal(stored.wikiProposals.length, 1);

    const consumed = consumeStructuredTurnResult(path);
    assert.equal(consumed.metadataError, null);
    assert.equal(consumed.sources[0].ragDocumentID, RAG_DOCUMENT_ID);
    assert.equal(consumed.sources[1].workRecordID, WORK_RECORD_ID);
    assert.equal(consumed.proposals[0].approvalTier, "user");
    assert.equal(existsSync(path), false);
  } finally {
    rmSync(root, { recursive: true, force: true });
    rmSync(dirname(path), { recursive: true, force: true });
  }
});

test("별도 제출값은 이전 마커 파싱값을 대체하고 미제출 채널은 그대로 둔다", () => {
  const decoded = {
    text: "완료했습니다.",
    needsInput: false,
    sources: [{ sourceKind: "file", title: "이전", locator: "old.md" }],
    proposals: [{ pageKey: "legacy" }],
    sourceError: "이전 오류",
    wikiProposalError: null,
  };
  const merged = applyStructuredTurnResult(decoded, {
    sourcesSubmitted: true,
    wikiProposalsSubmitted: false,
    sources: [{ sourceKind: "file", title: "새 근거", locator: "README.md" }],
    proposals: [],
    metadataError: null,
  });

  assert.equal(merged.sources[0].title, "새 근거");
  assert.equal(Object.hasOwn(merged, "sourceError"), false);
  assert.deepEqual(merged.proposals, decoded.proposals);
});

test("손상된 별도 메타데이터는 자연어 응답을 실패시키지 않고 경고로 격리한다", () => {
  const root = mkdtempSync(join(tmpdir(), "officestra-result-bad-"));
  const path = prepareStructuredTurnResult({
    workdir: root,
    characterID: "left-man",
    runtimeID: "bad",
  });
  try {
    writeFileSync(path, "not-json\n");
    const consumed = consumeStructuredTurnResult(path);
    const merged = applyStructuredTurnResult({
      text: "자연어 답변",
      needsInput: false,
      sources: [],
      proposals: [],
      wikiProposalError: null,
    }, consumed);

    assert.equal(merged.text, "자연어 답변");
    assert.match(merged.sourceError, /별도 응답 메타데이터/);
    assert.match(merged.wikiProposalError, /별도 응답 메타데이터/);
    assert.equal(existsSync(path), false);
  } finally {
    rmSync(root, { recursive: true, force: true });
    rmSync(dirname(path), { recursive: true, force: true });
  }
});

test("officestra-result 명령은 JSON 작성 없이 구조화 옵션을 제출한다", () => {
  const root = mkdtempSync(join(tmpdir(), "officestra-result-cli-"));
  const path = prepareStructuredTurnResult({
    workdir: root,
    characterID: "right-man",
    runtimeID: "cli",
  });
  const executable = join(structuredResultToolDirectory, "officestra-result");
  try {
    const result = spawnSync(executable, [
      "source",
      "--kind", "file",
      "--title", "README",
      "--locator", "README.md:12",
    ], {
      encoding: "utf8",
      env: { ...process.env, [STRUCTURED_RESULT_ENV]: path },
    });
    assert.equal(result.status, 0, result.stderr);
    assert.match(result.stdout, /응답 근거 1개/);
    assert.equal(inspectStructuredTurnResult(path).sources.length, 1);
  } finally {
    rmSync(root, { recursive: true, force: true });
    rmSync(dirname(path), { recursive: true, force: true });
  }
});

test("여러 근거 명령이 동시에 끝나도 제출값을 잃지 않는다", async () => {
  const root = mkdtempSync(join(tmpdir(), "officestra-result-parallel-"));
  const path = prepareStructuredTurnResult({
    workdir: root,
    characterID: "boss",
    runtimeID: "parallel",
  });
  const executable = join(structuredResultToolDirectory, "officestra-result");
  try {
    await Promise.all(Array.from({ length: 8 }, (_, index) =>
      new Promise((resolve, reject) => {
        const child = spawn(executable, [
          "source",
          "--kind", "file",
          "--title", `파일 ${index}`,
          "--locator", `docs/${index}.md`,
        ], {
          stdio: "ignore",
          env: { ...process.env, [STRUCTURED_RESULT_ENV]: path },
        });
        child.once("error", reject);
        child.once("close", (code) => {
          if (code === 0) resolve();
          else reject(new Error(`officestra-result 종료 코드: ${code}`));
        });
      })
    ));
    assert.equal(inspectStructuredTurnResult(path).sources.length, 8);
  } finally {
    rmSync(root, { recursive: true, force: true });
    rmSync(dirname(path), { recursive: true, force: true });
  }
});

test("DB 마이그레이션은 기존 직원 지침의 응답 JSON 계약만 제거한다", () => {
  const migration = readFileSync(
    new URL(
      "../../database/migrations/030_remove_response_metadata_prompt_protocol.sql",
      import.meta.url,
    ),
    "utf8",
  );
  assert.match(migration, /UPDATE characters/);
  assert.match(migration, /근거를 쓴 응답 끝에는 실제 쓴 근거만 남긴다/);
  assert.doesNotMatch(migration, /DELETE FROM characters/);

  const followupMigration = readFileSync(
    new URL(
      "../../database/migrations/031_remove_remaining_response_metadata_prompt_protocol.sql",
      import.meta.url,
    ),
    "utf8",
  );
  assert.match(followupMigration, /\[OFFICE_SOURCES\]/);
  assert.match(followupMigration, /근거를 쓴 응답/);
});
