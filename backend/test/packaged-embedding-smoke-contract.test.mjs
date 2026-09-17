import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const buildSource = readFileSync(
  new URL("../../scripts/build-app.sh", import.meta.url),
  "utf8",
);
const smokeSource = readFileSync(
  new URL("../../scripts/smoke-test-packaged-embedding.mjs", import.meta.url),
  "utf8",
);

test("Release 빌드는 번들 Node로 실제 로컬 임베딩 스모크를 실행한다", () => {
  assert.match(
    buildSource,
    /"\$NODE_RUNTIME_DIR\/bin\/node"\s+\\\s+"\$PACKAGED_EMBEDDING_SMOKE"\s+\\\s+"\$BACKEND_RUNTIME_DIR"/,
  );
  assert.match(smokeSource, /new LocalEmbeddingService\(\)/);
  assert.match(smokeSource, /await service\.embed\(/);
  assert.match(smokeSource, /vector\.length !== LOCAL_EMBEDDING_DIMENSIONS/);
  assert.match(smokeSource, /Math\.abs\(norm - 1\) > 0\.01/);
  assert.match(smokeSource, /service\.close\(\)/);
});

test("Release 빌드는 SwiftPM 평면 및 macOS 리소스 번들을 같은 앱 구조로 정규화한다", () => {
  assert.match(buildSource, /CORE_RESOURCE_BUNDLE\/Contents\/Resources/);
  assert.match(buildSource, /GAME_RESOURCE_BUNDLE\/Contents\/Resources/);
  assert.match(buildSource, /"\$CORE_RESOURCE_ROOT\/"/);
  assert.match(buildSource, /"\$GAME_RESOURCE_ROOT\/"/);
  assert.match(buildSource, /"\$RESOURCES_DIR\/OfficeLLM_OfficeCore\.bundle\/Info\.plist"/);
  assert.match(buildSource, /"\$RESOURCES_DIR\/OfficeLLM_OfficeGame\.bundle\/Info\.plist"/);
});

test("ad-hoc만 Hardened Runtime을 끄고 Developer ID는 유지한다", () => {
  assert.match(
    buildSource,
    /if \[\[ "\$CODESIGN_IDENTITY" == "-" \]\]; then\s+codesign \\\s+--force \\\s+--entitlements "\$NODE_ENTITLEMENTS" \\\s+--sign - \\\s+"\$NODE_RUNTIME_DIR\/bin\/node"\s+else\s+codesign \\\s+--force \\\s+--entitlements "\$NODE_ENTITLEMENTS" \\\s+--options runtime/,
  );
  assert.match(
    buildSource,
    /if \[\[ "\$CODESIGN_IDENTITY" == "-" \]\]; then\s+codesign --force --sign - "\$ORT_DYLIB"\s+codesign --force --sign - "\$ORT_BINDING"\s+else\s+codesign \\\s+--force \\\s+--options runtime/,
  );
});
