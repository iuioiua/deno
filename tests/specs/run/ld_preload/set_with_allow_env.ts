Deno.env.set("LD_PRELOAD", "./libpreload.so");

try {
  new Deno.Command("curl").spawn();
} catch (err) {
  console.log(err);
}
