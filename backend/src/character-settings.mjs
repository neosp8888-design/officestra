// 이 파일은 직원 CLI 설정을 검증하고 여러 직원을 원자적으로 전환한다.

import { AgentBusyError } from "./agent-runtime.mjs";
import {
  AGENT_BACKENDS,
  backendEfforts,
  backendModels,
  backendPermissions,
  backendSupportsFastMode,
  normalizedBackendPermission,
} from "./agent-provider.mjs";
import { characterSettingsRequireNewSession } from "./configuration.mjs";

const MAX_BULK_CHARACTER_SETTINGS = 100;

export class CharacterSettingsValidationError extends Error {}

export class CharacterSettingsTargetsNotFoundError extends Error {
  constructor(characterIDs) {
    super(`캐릭터를 찾을 수 없습니다: ${characterIDs.join(", ")}`);
    this.characterIDs = characterIDs;
  }
}

export class CharacterSettingsRuntimeUnavailableError extends Error {}

export class CharacterSettingsDrainConflictError extends Error {}

export function normalizeCharacterSettingsUpdate(body, {
  requireCharacterID = false,
  modelCatalog = null,
} = {}) {
  if (!body || typeof body !== "object" || Array.isArray(body)) {
    throw new CharacterSettingsValidationError(
      "직원 설정은 JSON 객체여야 합니다.",
    );
  }
  const characterID = String(body.characterId ?? "").trim();
  if (requireCharacterID && !characterID) {
    throw new CharacterSettingsValidationError(
      "직원 식별자를 입력하세요.",
    );
  }

  const backend = String(body.backend ?? "");
  if (!AGENT_BACKENDS.includes(backend)) {
    throw new CharacterSettingsValidationError("지원하지 않는 CLI입니다.");
  }
  const model = String(body.model ?? "");
  const dynamicCapabilities = modelCatalog?.modelCapabilities?.(
    backend,
    model,
  ) ?? null;
  if (!dynamicCapabilities && !backendModels(backend).includes(model)) {
    throw new CharacterSettingsValidationError(
      "지원하지 않는 모델입니다.",
    );
  }
  const effort = String(body.effort ?? "");
  const supportedEfforts = dynamicCapabilities?.efforts?.length > 0
    ? dynamicCapabilities.efforts
    : backendEfforts(backend, model);
  if (!supportedEfforts.includes(effort)) {
    throw new CharacterSettingsValidationError(
      "지원하지 않는 추론 레벨입니다.",
    );
  }

  const fastMode = body.fastMode;
  if (typeof fastMode !== "boolean") {
    throw new CharacterSettingsValidationError(
      "Fast 모드 설정은 참 또는 거짓이어야 합니다.",
    );
  }
  const supportsFastMode = dynamicCapabilities
    ? dynamicCapabilities.supportsFastMode === true
    : backendSupportsFastMode(backend, model);
  if (fastMode && !supportsFastMode) {
    throw new CharacterSettingsValidationError(
      "선택한 CLI와 모델은 Fast 모드를 지원하지 않습니다.",
    );
  }

  const requestedPermission = String(body.permission ?? "");
  if (!backendPermissions(backend).includes(requestedPermission)) {
    throw new CharacterSettingsValidationError(
      "지원하지 않는 권한입니다.",
    );
  }

  return {
    characterID,
    backend,
    model,
    effort,
    fastMode,
    permission: normalizedBackendPermission(backend, requestedPermission),
  };
}

export function normalizeBulkCharacterSettings(body, {
  modelCatalog = null,
} = {}) {
  if (!body || !Array.isArray(body.updates)) {
    throw new CharacterSettingsValidationError(
      "updates 배열을 입력하세요.",
    );
  }
  if (
    body.updates.length === 0 ||
    body.updates.length > MAX_BULK_CHARACTER_SETTINGS
  ) {
    throw new CharacterSettingsValidationError(
      `직원 설정은 1개 이상 ${MAX_BULK_CHARACTER_SETTINGS}개 이하여야 합니다.`,
    );
  }

  const updates = body.updates.map((update) =>
    normalizeCharacterSettingsUpdate(update, {
      requireCharacterID: true,
      modelCatalog,
    })
  );
  const seen = new Set();
  for (const update of updates) {
    if (seen.has(update.characterID)) {
      throw new CharacterSettingsValidationError(
        `같은 직원 설정을 두 번 보낼 수 없습니다: ${update.characterID}`,
      );
    }
    seen.add(update.characterID);
  }
  return updates;
}

export async function withCharacterSessionLocks(pool, characterIDs, body) {
  const sortedCharacterIDs = [...new Set(characterIDs)].sort();
  const client = await pool.connect();
  const lockedCharacterIDs = [];
  let operationError = null;
  try {
    for (const characterID of sortedCharacterIDs) {
      await client.query(
        "SELECT pg_advisory_lock(hashtext($1))",
        [`officestra:character:${characterID}`],
      );
      lockedCharacterIDs.push(characterID);
    }
    return await body(client);
  } catch (error) {
    operationError = error;
    throw error;
  } finally {
    for (const characterID of lockedCharacterIDs.reverse()) {
      try {
        await client.query(
          "SELECT pg_advisory_unlock(hashtext($1))",
          [`officestra:character:${characterID}`],
        );
      } catch (error) {
        if (!operationError) {
          console.warn(
            "직원 설정 잠금을 해제하지 못했습니다.",
            error instanceof Error ? error.message : String(error),
          );
        }
      }
    }
    client.release();
  }
}

export async function updateCharacterSettingsAtomically({
  pool,
  runtime,
  body,
  modelCatalog = null,
}) {
  const updates = normalizeBulkCharacterSettings(body, { modelCatalog });
  if (!runtime) {
    throw new CharacterSettingsRuntimeUnavailableError(
      "CLI 실행기가 준비된 뒤 설정을 변경하세요.",
    );
  }
  if (runtime.draining) {
    throw new CharacterSettingsDrainConflictError(
      "백엔드가 이미 다른 안전 전환을 준비 중입니다.",
    );
  }

  runtime.beginDrain();
  try {
    if (!runtime.maintenanceStatus().idle) {
      throw new AgentBusyError(
        "실행 중인 업무와 승인·병합 후속 처리가 끝난 뒤 설정을 변경하세요.",
      );
    }

    const characterIDs = updates.map((update) => update.characterID);
    return await withCharacterSessionLocks(
      pool,
      characterIDs,
      async (client) => {
        let transactionStarted = false;
        let committed = false;
        let result;
        try {
          await client.query("BEGIN");
          transactionStarted = true;
          const currentResult = await client.query(
            `
              SELECT
                id,
                name,
                backend,
                model,
                effort,
                fast_mode AS "fastMode",
                permission,
                config,
                identity_prompt AS "identityPrompt"
              FROM characters
              WHERE id = ANY($1::text[])
              ORDER BY id
              FOR UPDATE
            `,
            [characterIDs],
          );
          const currentByID = new Map(
            (currentResult.rows ?? []).map((character) => [
              character.id,
              character,
            ]),
          );
          const missingCharacterIDs = characterIDs.filter(
            (characterID) => !currentByID.has(characterID),
          );
          if (missingCharacterIDs.length > 0) {
            throw new CharacterSettingsTargetsNotFoundError(
              missingCharacterIDs,
            );
          }

          const sessionEndPlans = [];
          for (const update of updates) {
            const previous = currentByID.get(update.characterID);
            if(previous.config?.localProfileId)throw new AgentBusyError('로컬 4090 메뉴에서 클라우드로 돌아간 뒤 설정을 변경하세요.');
            if (characterSettingsRequireNewSession(previous, update)) {
              const plan = await runtime.inspectWorkspaceForSessionEnd(
                update.characterID,
                client,
              );
              if (plan.review?.hasChanges) {
                throw new AgentBusyError(
                  "CLI를 변경하기 전에 현재 작업 공간의 변경사항을 승인하거나 거절하세요.",
                );
              }
              sessionEndPlans.push(plan);
            }
          }

          for (const plan of sessionEndPlans) {
            await runtime.applyWorkspaceSessionEndPlan(client, plan);
          }

          const updatedByID = new Map(currentByID);
          for (const update of updates) {
            const previous = currentByID.get(update.characterID);
            const changed =
              previous.backend !== update.backend ||
              previous.model !== update.model ||
              previous.effort !== update.effort ||
              previous.fastMode !== update.fastMode ||
              previous.permission !== update.permission;
            if (!changed) {
              continue;
            }
            const updated = await client.query(
              `
                UPDATE characters
                SET
                  backend = $2,
                  model = $3,
                  effort = $4,
                  fast_mode = $5,
                  permission = $6,
                  config = CASE
                    WHEN characters.backend <> $2
                      THEN COALESCE(characters.config, '{}'::jsonb)
                        - 'executablePath'
                    ELSE characters.config
                  END,
                  updated_at = now()
                WHERE id = $1
                RETURNING
                  id,
                  name,
                  backend,
                  model,
                  effort,
                  fast_mode AS "fastMode",
                  permission,
                  identity_prompt AS "identityPrompt"
              `,
              [
                update.characterID,
                update.backend,
                update.model,
                update.effort,
                update.fastMode,
                update.permission,
              ],
            );
            if (updated.rowCount !== 1) {
              throw new CharacterSettingsTargetsNotFoundError([
                update.characterID,
              ]);
            }
            updatedByID.set(update.characterID, updated.rows[0]);
          }

          await client.query("COMMIT");
          committed = true;
          result = {
            ok: true,
            characters: updates.map((update) =>
              updatedByID.get(update.characterID)
            ),
            warnings: [],
          };

          for (const plan of sessionEndPlans) {
            try {
              const warning = await runtime.finalizeWorkspaceSessionEndPlan(
                plan,
              );
              if (warning) {
                result.warnings.push(warning);
              }
            } catch (error) {
              result.warnings.push(
                `${plan.characterID}: ${
                  error instanceof Error ? error.message : String(error)
                }`,
              );
            }
          }
          return result;
        } catch (error) {
          if (transactionStarted && !committed) {
            try {
              await client.query("ROLLBACK");
            } catch {
              // 원래 검증·업데이트 오류를 호출자에게 유지한다.
            }
          }
          throw error;
        }
      },
    );
  } finally {
    runtime.cancelDrain();
  }
}
