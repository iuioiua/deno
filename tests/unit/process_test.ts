// Copyright 2018-2025 the Deno authors. MIT license.
import { assertEquals, assertThrows } from "./test_util.ts";

Deno.test({ permissions: { run: false } }, function killPermissions() {
  assertThrows(() => {
    // Unlike the other test cases, we don't have permission to spawn a
    // subprocess we can safely kill. Instead we send SIGCONT to the current
    // process - assuming that Deno does not have a special handler set for it
    // and will just continue even if a signal is erroneously sent.
    Deno.kill(Deno.pid, "SIGCONT");
  }, Deno.errors.NotCapable);
});

Deno.test(
  { ignore: Deno.build.os !== "windows", permissions: { run: true } },
  function negativePidInvalidWindows() {
    assertThrows(() => {
      Deno.kill(-1, "SIGTERM");
    }, TypeError);
  },
);

Deno.test(
  { ignore: Deno.build.os !== "windows", permissions: { run: true } },
  function invalidSignalNameWindows() {
    assertThrows(() => {
      Deno.kill(Deno.pid, "SIGUSR1");
    }, TypeError);
  },
);

Deno.test(
  { permissions: { run: true, read: true } },
  async function killSuccess() {
    const p = await new Deno.Command(Deno.execPath(), {
      args: ["eval", "setTimeout(() => {}, 10000)"],
    }).spawn();

    try {
      Deno.kill(p.pid, "SIGKILL");
      const status = await p.status;

      assertEquals(status.success, false);
      if (Deno.build.os === "windows") {
        assertEquals(status.code, 1);
        assertEquals(status.signal, null);
      } else {
        assertEquals(status.code, 137);
        assertEquals(status.signal, "SIGKILL");
      }
    } finally {
      await p.output();
    }
  },
);

Deno.test(
  { permissions: { run: true, read: true } },
  async function killFailed() {
    const p = await new Deno.Command(Deno.execPath(), {
      args: ["eval", "setTimeout(() => {}, 10000)"],
    }).spawn();
    assertThrows(
      () => p.stdin,
      TypeError,
      "Cannot get 'stdin': 'stdin' is not piped",
    );
    assertThrows(
      () => p.stdout,
      TypeError,
      "Cannot get 'stdout': 'stdout' is not piped",
    );

    assertThrows(() => {
      // @ts-expect-error testing runtime error of bad signal
      Deno.kill(p.pid, "foobar");
    }, TypeError);

    await p.output();
  },
);

Deno.test({
  name: "process.ppid matches parent process",
  permissions: { run: true, read: true },
  ignore: Deno.build.os === "windows",
  async fn() {
    const command = new Deno.Command(Deno.execPath(), {
      args: [
        "eval",
        "import { ppid } from 'node:process'; console.log(ppid);",
      ],
      stdout: "piped",
    });

    const { stdout } = await command.output();
    const stdoutPpid = parseInt(
      new TextDecoder().decode(stdout).trim(),
    );

    assertEquals(stdoutPpid, Deno.pid);
  },
});
