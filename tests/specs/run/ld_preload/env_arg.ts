try {
  new Deno.Command("curl", {
    env: {
      "LD_PRELOAD": "./libpreload.so",
    },
  }).spawn();
} catch (err) {
  console.log(err);
}
