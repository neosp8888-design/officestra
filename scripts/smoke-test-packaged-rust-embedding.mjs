#!/usr/bin/env node
// 번들 Rust 백엔드의 임베딩 워커가 옆에 둔 ONNX Runtime으로 번들 Node 백엔드와 같은 벡터를 만드는지 확인한다.
// 서버 모드는 DB에 붙으므로 띄우지 않고 `embedding-worker` 하위 명령만 쓴다.

import { spawn } from "node:child_process";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

if (!process.argv[2]) {
  throw new Error("패키지 런타임 디렉터리가 필요합니다.");
}
const runtimeDirectory = resolve(process.argv[2]);
const backendDirectory = resolve(runtimeDirectory, "backend");
const rustExecutable = resolve(
  runtimeDirectory,
  "rust",
  "bin",
  "officestra-backend",
);
const { LocalEmbeddingService } = await import(pathToFileURL(
  resolve(backendDirectory, "src", "local-embedding.mjs"),
));
const { LOCAL_EMBEDDING_DIMENSIONS, localEmbeddingModelDirectory } =
  await import(pathToFileURL(
    resolve(backendDirectory, "src", "local-embedding-config.mjs"),
  ));
const text = "OFFICESTRA 패키지 Rust 백엔드 임베딩 스모크 테스트";

// 운영 실행과 같게 ORT_DYLIB_PATH 없이 실행 파일 옆 libonnxruntime.dylib을 쓰게 한다.
const environment = {
  ...process.env,
  OFFICE_EMBEDDING_MODEL_DIR: localEmbeddingModelDirectory(),
};
delete environment.ORT_DYLIB_PATH;
const worker = spawn(rustExecutable, ["embedding-worker"], {
  env: environment,
  stdio: ["pipe", "pipe", "inherit"],
});
let output = "";
worker.stdout.setEncoding("utf8");
worker.stdout.on("data", (chunk) => {
  output += chunk;
});
const timer = setTimeout(() => worker.kill("SIGTERM"), 180_000);
const closed = new Promise((resolveClose, reject) => {
  worker.once("error", reject);
  worker.once("close", (code, signal) => resolveClose({ code, signal }));
});
worker.stdin.end(`${JSON.stringify({ id: 1, texts: [text] })}\n`);
const { code, signal } = await closed;
clearTimeout(timer);
if (code !== 0) {
  throw new Error(
    `Rust 임베딩 워커가 실패했습니다. code=${code} signal=${signal}`,
  );
}
const reply = JSON.parse(output.split("\n")[0]);
if (reply.ok !== true) {
  throw new Error(`Rust 임베딩 워커 오류: ${reply.error}`);
}
const rustVector = reply.result?.vectors?.[0];
if (
  !Array.isArray(rustVector) ||
  rustVector.length !== LOCAL_EMBEDDING_DIMENSIONS ||
  rustVector.some((value) => !Number.isFinite(value))
) {
  throw new Error(
    `Rust 임베딩 출력이 ${LOCAL_EMBEDDING_DIMENSIONS}차원이 아닙니다.`,
  );
}
const norm = Math.sqrt(
  rustVector.reduce((sum, value) => sum + value * value, 0),
);
if (Math.abs(norm - 1) > 0.01) {
  throw new Error(`Rust 임베딩 정규화 값이 잘못됐습니다. norm=${norm}`);
}

const service = new LocalEmbeddingService();
let nodeVector;
try {
  [nodeVector] = await service.embed([text]);
} finally {
  service.close();
}
const cosine = rustVector.reduce(
  (sum, value, index) => sum + value * nodeVector[index],
  0,
);
if (!(cosine >= 0.999)) {
  throw new Error(`Rust와 Node 임베딩이 다릅니다. cosine=${cosine}`);
}
process.stdout.write(
  `Packaged Rust embedding smoke passed: ${rustVector.length} dimensions, ` +
    `norm=${norm.toFixed(6)}, cosine(Node)=${cosine.toFixed(6)}\n`,
);
