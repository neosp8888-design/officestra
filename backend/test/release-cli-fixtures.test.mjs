import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { access, mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";

import { parseCodexModelCatalog } from "../src/model-catalog.mjs";

const run = promisify(execFile);

for (const stale of [false, true]) {
  test(`릴리즈 CLI 모형은 목록 조회와 업무 실행을 구분한다 (stale=${stale})`, async (context) => {
    const directory = await mkdtemp(join(tmpdir(), "officestra-release-cli-"));
    context.after(() => rm(directory, { recursive: true, force: true }));
    const marker = join(directory, "stale-cli-used");
    const fixture = fileURLToPath(new URL(
      `../../scripts/fixtures/fake-codex${stale ? "-stale" : ""}.sh`,
      import.meta.url,
    ));
    const options = {
      cwd: directory,
      env: { ...process.env, OFFICESTRA_STALE_CLI_MARKER: marker },
    };

    const catalog = await run(fixture, ["debug", "models"], options);
    assert.equal(parseCodexModelCatalog(catalog.stdout).models[0].id, "gpt-5.6-terra");
    await run(fixture, ["--version"], options);
    await assert.rejects(access(marker), { code: "ENOENT" });
    await assert.rejects(run(fixture, ["unsupported"], options));
    await assert.rejects(access(marker), { code: "ENOENT" });

    const task = await run(fixture, ["exec", "--json", "test prompt"], options);
    const events = task.stdout.trim().split("\n").map((line) => JSON.parse(line));
    assert.equal(events[0].thread_id, stale
      ? "community-preview-e2e-stale-session"
      : "community-preview-e2e-session");
    assert.equal(events.at(-1).type, "turn.completed");
    if (stale) {
      assert.equal(await readFile(marker, "utf8"), "stale CLI path invoked\n");
    } else {
      await assert.rejects(access(marker), { code: "ENOENT" });
    }
  });
}
