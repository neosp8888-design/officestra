// 이 파일은 캐릭터 JSON 설정을 읽어 PostgreSQL 기본 데이터와 동기화한다.

import { constants } from "node:fs";
import { access, readFile } from "node:fs/promises";
import { basename, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import {
  agentProvider,
  backendExecutableName,
} from "./agent-provider.mjs";

const backendDirectory = resolve(
  fileURLToPath(new URL("..", import.meta.url)),
  "..",
);

export function characterConfigPath() {
  return resolve(
    backendDirectory,
    process.env.CHARACTER_CONFIG_PATH ??
      "Sources/OfficeCore/Resources/characters.json",
  );
}

export async function readCharacterConfiguration() {
  const source = await readFile(characterConfigPath(), "utf8");
  return configurationWithRuntimeWorkdir(
    JSON.parse(source),
    process.env.OFFICE_WORKDIR,
  );
}

export function configurationWithRuntimeWorkdir(
  configuration,
  runtimeWorkdir,
) {
  const normalizedWorkdir = String(runtimeWorkdir ?? "").trim();
  if (!normalizedWorkdir) return configuration;
  return {
    ...configuration,
    workdir: resolve(normalizedWorkdir),
  };
}

export function characterSettingsRequireNewSession(previous, next) {
  return previous.backend !== next.backend
    || (previous.config?.localProfileId ?? previous.localProfileId ?? null)
      !== (next.config?.localProfileId ?? next.localProfileId ?? null);
}

export async function characterConfigurationForSync(
  character,
  executableAccess = access,
) {
  const executablePath = String(character.executablePath ?? "").trim();
  const expectedExecutable = agentProvider(character.backend)
    ? backendExecutableName(character.backend)
    : null;
  if (
    !executablePath ||
    !expectedExecutable ||
    basename(executablePath) !== expectedExecutable
  ) {
    const { executablePath: _ignored, ...withoutExecutable } = character;
    return withoutExecutable;
  }

  try {
    await executableAccess(executablePath, constants.X_OK);
    return {
      ...character,
      executablePath,
    };
  } catch {
    const { executablePath: _ignored, ...withoutExecutable } = character;
    return withoutExecutable;
  }
}

export async function syncCharacters(client, configuration) {
  for (const character of configuration.characters) {
    const synchronizedConfiguration = await characterConfigurationForSync(
      character,
    );
    await client.query(
      `
        INSERT INTO characters (
          id,
          name,
          seat,
          backend,
          identity_prompt,
          model,
          effort,
          fast_mode,
          permission,
          config
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10::jsonb)
        ON CONFLICT (id) DO UPDATE
        SET
          seat = EXCLUDED.seat,
          config = CASE
            WHEN characters.backend = EXCLUDED.backend
              AND EXCLUDED.config ? 'executablePath'
              THEN jsonb_set(
                COALESCE(characters.config, '{}'::jsonb),
                '{executablePath}',
                EXCLUDED.config -> 'executablePath',
                true
              )
            ELSE characters.config
          END,
          updated_at = now()
      `,
      [
        character.id,
        character.name,
        character.seat,
        character.backend,
        character.identityPrompt,
        character.model,
        character.effort,
        character.fastMode ?? true,
        character.permission,
        JSON.stringify(synchronizedConfiguration),
      ],
    );
  }
}
