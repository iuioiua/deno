const entries = await Array.fromAsync(Deno.readDir("tests/util/std"));
const results = entries
  .filter((entry) => entry.isDirectory)
  .map((entry) => entry.name)
  .toSorted((a, b) => a.localeCompare(b))
  .map((name) => [`@std/${name}/`, `jsr:@std/${name}@^0`])

console.log(Object.fromEntries(results));