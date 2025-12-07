const { core } = Deno[Deno.internal];
// RID of `Deno.stdin` is 0
const opPromise = core.read(0, new Uint8Array(10));
core.unrefOpPromise(opPromise);

async function main() {
  console.log(1);
  await opPromise;
  console.log(2);
}

main();
