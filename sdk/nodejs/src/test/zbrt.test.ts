import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import net from 'node:net';
import { ZbrtFrame, decode } from '../zbrt-frame.js';
import { DecodeError } from '../errors.js';
import * as codec from '../zbrt-codec.js';
function frameBytes(kind: number, requestId: Buffer, payload: Buffer): Buffer {
  const header = Buffer.alloc(28);
  header.write('ZBRT', 0, 'ascii');
  header[4] = 1;
  header[5] = kind;
  header.writeUInt16BE(0, 6);
  requestId.copy(header, 8);
  header.writeUInt32BE(payload.length, 24);
  return Buffer.concat([header, payload]);
}

describe('zbrt-codec', () => {
  it('hello/helloAck roundtrips', () => {
    const caps = ['execute', 'stream'];
    const payload = codec.encodeHello('test-client', caps);
    const ack = codec.decodeHelloAck(payload);
    assert.equal(ack.server, 'test-client');
    assert.deepEqual(ack.capabilities, caps);
  });
  it('execute encodes argv, cwd, stdin, timeout', () => {
    const payload = codec.encodeExecute(['echo', 'hi'], '/workspace', Buffer.from('in'), 5000);
    const exec = codec.decodeExecute(payload);
    assert.deepEqual(exec.argv, ['echo', 'hi']);
    assert.equal(exec.cwd, '/workspace');
    assert.deepEqual(exec.stdin, Buffer.from('in'));
    assert.equal(exec.timeoutMs, 5000);
  });
  it('exit roundtrips with and without signal', () => {
    const e1 = codec.decodeExit(codec.encodeExit(0, null));
    assert.equal(e1.code, 0);
    assert.equal(e1.signal, null);
    const e2 = codec.decodeExit(codec.encodeExit(-1, 9));
    assert.equal(e2.code, -1);
    assert.equal(e2.signal, 9);
  });
  it('fs roundtrips op/path/data', () => {
    const payload = codec.encodeFs(4, '/workspace/f.txt', Buffer.from('data'));
    const fs = codec.decodeFs(payload);
    assert.equal(fs.op, 4);
    assert.equal(fs.path, '/workspace/f.txt');
    assert.deepEqual(fs.data, Buffer.from('data'));
  });
  it('health roundtrips', () => {
    const payload = codec.encodeHealth(true, 'ready');
    const h = codec.decodeHealth(payload);
    assert.equal(h.healthy, true);
    assert.equal(h.message, 'ready');
  });
  it('error roundtrips', () => {
    const payload = codec.encodeError(42, 'boom');
    const e = codec.decodeError(payload);
    assert.equal(e.code, 42);
    assert.equal(e.message, 'boom');
  });
  it('rejects trailing bytes', () => {
    const payload = codec.encodeHealth(true, 'ok');
    assert.throws(() => codec.decodeHealth(Buffer.concat([payload, Buffer.from([0])])));
  });
});

describe('zbrt-frame', () => {
  const requestId = Buffer.alloc(16, 0xab);

  it('encodes correct header', () => {
    const payload = Buffer.from('test');
    const frame = new ZbrtFrame(3, 0, requestId, payload);
    const wire = frame.encode();
    assert.equal(wire.subarray(0, 4).toString('ascii'), 'ZBRT');
    assert.equal(wire[4], 1);
    assert.equal(wire[5], 3);
    assert.equal(wire.readUInt16BE(6), 0);
    assert.ok(wire.subarray(8, 24).equals(requestId));
    assert.equal(wire.readUInt32BE(24), 4);
  });
  it('rejects wrong request_id length', () => {
    assert.throws(() => new ZbrtFrame(3, 0, Buffer.alloc(8), Buffer.alloc(0)), DecodeError);
  });
  it('rejects oversized payload', () => {
    const big = Buffer.alloc(17 * 1024 * 1024);
    assert.throws(() => new ZbrtFrame(3, 0, Buffer.alloc(16), big), DecodeError);
  });
  it('roundtrips through decode', () => {
    const payload = Buffer.from('data');
    const frame = new ZbrtFrame(4, 0, requestId, payload);
    const decoded = ZbrtFrame.decode(frame.encode());
    assert.equal(decoded.kind, 4);
    assert.deepEqual(decoded.requestId, requestId);
    assert.deepEqual(decoded.payload, payload);
  });
});
