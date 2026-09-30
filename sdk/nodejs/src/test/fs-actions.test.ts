/**
 * Regression: the NDJSON fs op codes are 1-based (ZBRT FS_OP_*: 1=ls 2=find
 * 3=grep 4=read 5=write). The FS_ACTIONS table once lacked the slot-0 entry,
 * so every mapped action landed one late — `ls` hit the wire as `find` and
 * the agent rejected it with "pattern must be a string" (find/grep/read
 * drifted too; only read/write survived on dedicated builders). This test
 * asserts the ACTION NAMES on the wire, not just the parsed results.
 */
import { describe, it, afterEach } from 'node:test';
import assert from 'node:assert/strict';
import net from 'node:net';
import { Sandbox, TRANSPORT_NDJSON } from '../client.js';

describe('fs op codes map to the right action names', () => {
  let server: net.Server | null = null;
  const sockets: net.Socket[] = [];

  afterEach(() => {
    for (const socket of sockets) socket.destroy();
    sockets.length = 0;
    server?.close();
    server = null;
  });

  it('ls / find / grep put their own action name on the wire', async () => {
    const seen: string[] = [];
    server = net.createServer((socket) => {
      sockets.push(socket);
      socket.on('error', () => {});
      socket.setNoDelay(true);
      let buffer = '';
      socket.on('data', (chunk: Buffer) => {
        buffer += chunk.toString('utf8');
        let nl = buffer.indexOf('\n');
        while (nl >= 0) {
          const line = buffer.slice(0, nl);
          buffer = buffer.slice(nl + 1);
          nl = buffer.indexOf('\n');
          if (!line.trim()) continue;
          const request = JSON.parse(line) as { action?: string };
          seen.push(request.action ?? '<none>');
          const reply =
            request.action === 'ls'
              ? { entries: [{ name: 'a.txt', is_dir: false, size: 1 }], truncated: false }
              : request.action === 'find'
                ? { matches: ['a.txt'], truncated: false }
                : request.action === 'grep'
                  ? { matches: [], truncated: false }
                  : { error: `unexpected action: ${request.action}`, exit_code: 1 };
          socket.write(JSON.stringify(reply) + '\n');
        }
      });
    });
    await new Promise<void>((resolve) => server?.listen(0, '127.0.0.1', resolve));
    (server as unknown as net.Server).unref();
    const port = (server!.address() as net.AddressInfo).port;

    const sandbox = new Sandbox(
      { id: 'sb-1', guest_addr: `127.0.0.1:${port}` },
      {} as never,
      TRANSPORT_NDJSON,
      5000,
    );
    await sandbox.ls('/workspace');
    await sandbox.find('/workspace', 'a*');
    await sandbox.grep('/workspace', 'needle');
    assert.deepEqual(seen, ['ls', 'find', 'grep']);
  });
});
