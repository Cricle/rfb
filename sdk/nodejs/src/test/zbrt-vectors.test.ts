/**
 * Golden wire vectors and strict-decode rejection tests for the ZBRT codec.
 *
 * The vectors live in `sdk/shared/conformance/zbrt_vectors.json` and are
 * shared byte-for-byte across all four SDKs; this test loads them from disk
 * (found by walking up from the compiled test directory to the repository
 * root) and asserts the codec reproduces the exact bytes in BOTH directions
 * (PROTOCOL.md §4). The semantic assertions (decoded field values) stay in
 * code below.
 */
import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { ZbrtFrame, decode } from '../zbrt-frame.js';
import { DecodeError } from '../errors.js';
import * as codec from '../zbrt-codec.js';
import { V1_CAPABILITIES } from '../zbrt-connection.js';

/** Walk up from the compiled test directory until the shared vectors appear. */
function findVectorsFile(): string {
  const start = path.dirname(fileURLToPath(import.meta.url));
  let dir = start;
  for (let i = 0; i < 12; i++) {
    for (const candidate of [
      path.join(dir, 'sdk', 'shared', 'conformance', 'zbrt_vectors.json'),
      path.join(dir, 'shared', 'conformance', 'zbrt_vectors.json'),
    ]) {
      if (fs.existsSync(candidate)) return candidate;
    }
    const parent = path.dirname(dir);
    if (parent === dir) break;
    dir = parent;
  }
  throw new Error(`sdk/shared/conformance/zbrt_vectors.json not found from ${start}`);
}

interface VectorFrame {
  name: string;
  kind: number;
  hex: string;
}

interface RejectVector {
  name: string;
  byte_offset: number;
  byte_value: number;
}

interface VectorFile {
  request_id_hex: string;
  frames: VectorFrame[];
  rejects: RejectVector[];
}

const VECTORS = JSON.parse(fs.readFileSync(findVectorsFile(), 'utf8')) as VectorFile;
const RID = Buffer.from(VECTORS.request_id_hex, 'hex');
const FRAMES = new Map<string, VectorFrame>(
  VECTORS.frames.map((frame): [string, VectorFrame] => [frame.name, frame]),
);

const EXPECTED_FRAME_NAMES = [
  'hello',
  'helloack',
  'execute',
  'output',
  'exit',
  'cancel',
  'cancel_legacy',
  'error',
] as const;

/**
 * name -> payload encoder, payload decoder, expected decoded value. The
 * expected decode values ARE the semantic contract of each vector frame; they
 * also feed the encode direction so encode/decode must both hit the shared
 * bytes.
 */
const SEMANTICS: Record<
  (typeof EXPECTED_FRAME_NAMES)[number],
  { encode: () => Buffer; decode: (payload: Buffer) => unknown; expected: unknown }
> = {
  hello: {
    encode: () => codec.encodeHello('sdk-test', ['execute', 'stream']),
    decode: (payload) => codec.decodeHello(payload),
    expected: { client: 'sdk-test', capabilities: ['execute', 'stream'] },
  },
  helloack: {
    encode: () => codec.encodeHelloAck('rfb-zeroboot-guest', V1_CAPABILITIES),
    decode: (payload) => codec.decodeHelloAck(payload),
    expected: { server: 'rfb-zeroboot-guest', capabilities: [...V1_CAPABILITIES] },
  },
  execute: {
    encode: () => codec.encodeExecute(['echo', 'hi'], '/workspace', Buffer.from('abc'), 1500),
    decode: (payload) => codec.decodeExecute(payload),
    expected: {
      argv: ['echo', 'hi'],
      cwd: '/workspace',
      stdin: Buffer.from('abc'),
      timeoutMs: 1500,
    },
  },
  output: {
    encode: () => codec.encodeOutput(1, Buffer.from('err line\n')),
    decode: (payload) => codec.decodeOutput(payload),
    expected: { stream: 1, data: Buffer.from('err line\n') },
  },
  exit: {
    encode: () => codec.encodeExit(0, null),
    decode: (payload) => codec.decodeExit(payload),
    expected: { code: 0, signal: null },
  },
  cancel: {
    encode: () => codec.encodeCancel('user', null), // modern form: explicit 0 target flag
    decode: (payload) => codec.decodeCancel(payload),
    expected: { reason: 'user', target: null },
  },
  cancel_legacy: {
    encode: () => codec.encodeCancel('user', null, false), // payload ends after the reason
    decode: (payload) => codec.decodeCancel(payload),
    expected: { reason: 'user', target: null },
  },
  error: {
    encode: () => codec.encodeError(1, 'argv is empty'),
    decode: (payload) => codec.decodeError(payload),
    expected: { code: 1, message: 'argv is empty' },
  },
};

describe('ZBRT shared golden vectors', () => {
  it('vector file has the expected shape', () => {
    assert.deepEqual([...FRAMES.keys()].sort(), [...EXPECTED_FRAME_NAMES].sort());
    assert.equal(RID.length, 16);
  });

  it('every frame hits on encode AND decode, byte for byte', () => {
    for (const name of EXPECTED_FRAME_NAMES) {
      const vector = FRAMES.get(name);
      assert.ok(vector, `missing vector ${name}`);
      const golden = vector.hex;
      const { encode, decode: decodePayload, expected } = SEMANTICS[name];

      // Encode direction: canonical payload + frame header.
      const payload = encode();
      assert.equal(payload.toString('hex'), golden.slice(56), `${name} payload bytes`);
      assert.equal(
        new ZbrtFrame(vector.kind, 0, RID, payload).encode().toString('hex'),
        golden,
        `${name} full frame bytes`,
      );

      // Decode direction: strict frame split + semantic fields.
      const frame = decode(Buffer.from(golden, 'hex'));
      assert.equal(frame.kind, vector.kind);
      assert.deepEqual(frame.requestId, RID);
      assert.deepEqual(
        decodePayload(frame.payload),
        expected,
        `${name} decoded fields`,
      );
    }
  });

  it('helloack carries the full v1 capability set', () => {
    const vector = FRAMES.get('helloack');
    assert.ok(vector);
    const ack = codec.decodeHelloAck(Buffer.from(vector.hex, 'hex').subarray(28));
    assert.equal(ack.capabilities.length, 6);
    assert.equal(ack.capabilities[0], 'execute');
    assert.equal(ack.capabilities[5], 'filesystem');
    assert.deepEqual(ack.capabilities, V1_CAPABILITIES);
  });

  it('shared reject vectors fail strict decode', () => {
    assert.ok(VECTORS.rejects.length > 0, 'reject vectors must be present');
    const good = FRAMES.get('hello');
    assert.ok(good);
    for (const reject of VECTORS.rejects) {
      const mutated = Buffer.from(good.hex, 'hex');
      mutated[reject.byte_offset] = reject.byte_value;
      assert.throws(
        () => decode(mutated),
        DecodeError,
        `reject vector ${reject.name} must fail decode`,
      );
    }
  });
});
