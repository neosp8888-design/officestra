// 리스(RisuAI) 프리셋(.risup)·모듈(.risum) 파일을 풀어 일반 객체로 돌려준다.
import { createDecipheriv, createHash } from "node:crypto";
import { gunzipSync } from "node:zlib";

// RisuAI(GPL-3.0, kwaroran/RisuAI src/ts/rpack/rpack_map.bin)의 바이트 변환표 중 해독 절반.
const RPACK_DECODE_HEX =
  "2cf7848bc965fbb69faeb3032d0169741fe4a3ecee5c3421934a0f6ae262029e" +
  "229cfd3cfc71c7c6ad596705706d8a4412fa24865fafd17a47cefe5063dd5106" +
  "6f18e052a8099d56734cb8536cc3a00e19cf3e0d7e07326846ea48f9992eaba4" +
  "49205e5535380cbcd3b1581679280a1ae1f2cdc439dba2ba6072767d95ef7fc8" +
  "c0de3794bfb51481922545ace7f566a72b365ac113e34b3ae88d831b7c27b09a" +
  "42eb87aadc548e7826d25729d4b7f82f8f8975f04177c21effd81511e5049717" +
  "f331d09b00d7cab44f2a3bd9b26bda5da13f3061bd913d4ee6dfbe4d828c1d23" +
  "109864f485337b9043bba988f1d6a51cf6cc6eb95b0b96edd5e9c5cb08a68040";
const RPACK_DECODE = Buffer.from(RPACK_DECODE_HEX, "hex");

export class RisuFormatError extends Error {}

export function decodeRPack(data) {
  const result = Buffer.alloc(data.length);
  for (let i = 0; i < data.length; i++) result[i] = RPACK_DECODE[data[i]];
  return result;
}

// 리스가 쓰는 표준 msgpack만 읽는다. 확장 타입은 msgpackr의 undefined 표기만 받는다.
export function decodeMsgpack(buffer) {
  const data = Buffer.from(buffer);
  let pos = 0;
  const need = (length) => {
    if (pos + length > data.length) throw new RisuFormatError("msgpack 데이터가 잘렸습니다.");
  };
  const bytes = (length) => {
    need(length);
    const value = data.subarray(pos, pos + length);
    pos += length;
    return value;
  };
  const str = (length) => bytes(length).toString("utf8");
  const array = (length) => Array.from({ length }, () => read());
  const map = (length) => {
    const value = {};
    for (let i = 0; i < length; i++) {
      const key = read();
      value[String(key)] = read();
    }
    return value;
  };
  const number = (method, length) => {
    need(length);
    const value = data[method](pos);
    pos += length;
    return value;
  };
  const read = () => {
    need(1);
    const type = data[pos++];
    if (type <= 0x7f) return type;
    if (type <= 0x8f) return map(type & 0x0f);
    if (type <= 0x9f) return array(type & 0x0f);
    if (type <= 0xbf) return str(type & 0x1f);
    if (type >= 0xe0) return type - 0x100;
    switch (type) {
      case 0xc0: return null;
      case 0xc2: return false;
      case 0xc3: return true;
      case 0xc4: return Buffer.from(bytes(number("readUInt8", 1)));
      case 0xc5: return Buffer.from(bytes(number("readUInt16BE", 2)));
      case 0xc6: return Buffer.from(bytes(number("readUInt32BE", 4)));
      case 0xca: return number("readFloatBE", 4);
      case 0xcb: return number("readDoubleBE", 8);
      case 0xcc: return number("readUInt8", 1);
      case 0xcd: return number("readUInt16BE", 2);
      case 0xce: return number("readUInt32BE", 4);
      case 0xcf: return Number(number("readBigUInt64BE", 8));
      case 0xd0: return number("readInt8", 1);
      case 0xd1: return number("readInt16BE", 2);
      case 0xd2: return number("readInt32BE", 4);
      case 0xd3: return Number(number("readBigInt64BE", 8));
      case 0xd9: return str(number("readUInt8", 1));
      case 0xda: return str(number("readUInt16BE", 2));
      case 0xdb: return str(number("readUInt32BE", 4));
      case 0xdc: return array(number("readUInt16BE", 2));
      case 0xdd: return array(number("readUInt32BE", 4));
      case 0xde: return map(number("readUInt16BE", 2));
      case 0xdf: return map(number("readUInt32BE", 4));
      case 0xd4: {
        // msgpackr는 undefined를 fixext1(타입 0, 값 0)으로 쓴다.
        const [extType, value] = bytes(2);
        if (extType === 0 && value === 0) return undefined;
        throw new RisuFormatError("지원하지 않는 msgpack 확장 타입입니다.");
      }
      default:
        throw new RisuFormatError(`지원하지 않는 msgpack 타입입니다: 0x${type.toString(16)}`);
    }
  };
  const value = read();
  if (pos !== data.length) throw new RisuFormatError("msgpack 뒤에 남은 데이터가 있습니다.");
  return value;
}

function decryptPresetBuffer(data) {
  const buffer = Buffer.from(data);
  if (buffer.length < 16) throw new RisuFormatError("암호화된 프리셋이 너무 짧습니다.");
  const key = createHash("sha256").update("risupreset").digest();
  const decipher = createDecipheriv("aes-256-gcm", key, Buffer.alloc(12));
  decipher.setAuthTag(buffer.subarray(buffer.length - 16));
  return Buffer.concat([decipher.update(buffer.subarray(0, buffer.length - 16)), decipher.final()]);
}

export function decodeRisuPreset(fileData) {
  let outer;
  try {
    outer = decodeMsgpack(gunzipSync(decodeRPack(Buffer.from(fileData))));
  } catch (error) {
    if (error instanceof RisuFormatError) throw error;
    throw new RisuFormatError("리스 프리셋(.risup) 파일이 아닙니다.");
  }
  if (outer?.type !== "preset" || ![0, 2].includes(outer.presetVersion)) {
    throw new RisuFormatError("지원하지 않는 리스 프리셋 버전입니다.");
  }
  const encrypted = outer.preset ?? outer.pres;
  if (!encrypted) throw new RisuFormatError("프리셋 본문이 없습니다.");
  let preset;
  try {
    preset = decodeMsgpack(decryptPresetBuffer(encrypted));
  } catch (error) {
    if (error instanceof RisuFormatError) throw error;
    throw new RisuFormatError("프리셋 본문을 풀지 못했습니다.");
  }
  if (!preset || typeof preset !== "object" || !Array.isArray(preset.promptTemplate)) {
    throw new RisuFormatError("프롬프트 템플릿이 있는 프리셋만 지원합니다.");
  }
  return preset;
}

export function decodeRisuModule(fileData) {
  const buffer = Buffer.from(fileData);
  if (buffer.length < 6 || buffer[0] !== 111 || buffer[1] !== 0) {
    throw new RisuFormatError("리스 모듈(.risum) 파일이 아닙니다.");
  }
  const length = buffer.readUInt32LE(2);
  if (6 + length > buffer.length) throw new RisuFormatError("모듈 데이터가 잘렸습니다.");
  let main;
  try {
    main = JSON.parse(decodeRPack(buffer.subarray(6, 6 + length)).toString("utf8"));
  } catch {
    throw new RisuFormatError("모듈 본문을 풀지 못했습니다.");
  }
  if (main?.type !== "risuModule" || !main.module) {
    throw new RisuFormatError("리스 모듈 형식이 아닙니다.");
  }
  return main.module;
}

// 테스트용: 리스와 같은 방식으로 묶는다.
export function encodeRPack(data) {
  const encode = Buffer.alloc(256);
  for (let i = 0; i < 256; i++) encode[RPACK_DECODE[i]] = i;
  const result = Buffer.alloc(data.length);
  for (let i = 0; i < data.length; i++) result[i] = encode[data[i]];
  return result;
}
