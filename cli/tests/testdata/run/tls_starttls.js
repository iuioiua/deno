import { assert, assertEquals } from "../../../../test_util/std/assert/mod.ts";
import { toText } from "../../../../test_util/std/streams/to_text.ts";

const encoder = new TextEncoder();

const { promise, resolve } = Promise.withResolvers();
const hostname = "localhost";
const port = 3504;

const listener = Deno.listenTls({
  hostname,
  port,
  certFile: "./tls/localhost.crt",
  keyFile: "./tls/localhost.key",
});

const response = encoder.encode(
  "HTTP/1.1 200 OK\r\nContent-Length: 12\r\n\r\nHello World\n",
);

listener.accept().then(
  async (conn) => {
    assert(conn.remoteAddr != null);
    assert(conn.localAddr != null);
    await conn.write(response);
    // TODO(bartlomieju): this might be a bug
    setTimeout(() => {
      conn.close();
      resolve();
    }, 0);
  },
);

let conn = await Deno.connect({ hostname, port });
conn = await Deno.startTls(conn, { hostname });
assert(conn.rid > 0);
const body = `GET / HTTP/1.1\r\nHost: ${hostname}:${port}\r\n\r\n`;
const writeResult = await conn.write(encoder.encode(body));
assertEquals(body.length, writeResult);
const actual = await toText(conn.readable);
const expected = "HTTP/1.1 200 OK\r\nContent-Length: 12\r\n\r\nHello World\n";
assertEquals(actual, expected);
listener.close();
await promise;

console.log("DONE");
