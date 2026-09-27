import { rm, copyFile, mkdir } from "node:fs/promises";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
const root = fileURLToPath(new URL("../", import.meta.url));
const dist = resolve(root, "dist");
if (dirname(dist) !== resolve(root)) throw new Error("Build output must be inside the SDK");
await rm(dist, { recursive: true, force: true });
execFileSync(process.execPath, ["node_modules/typescript/bin/tsc", "-p", "tsconfig.build.json"], { cwd: root, stdio: "inherit" });
for (const name of ["LICENSE-MIT", "LICENSE-APACHE"]) {
  await copyFile(new URL(`../../${name}`, new URL("../", import.meta.url)), new URL(`../dist/${name}`, import.meta.url));
}
for (const [source, target] of [["README.md", "INTEGRATION.md"], ["LLM-INTEGRATION.md", "LLM-INTEGRATION.md"], ["EXTERNAL-RULES.md", "EXTERNAL-RULES.md"], ["CLIENT-WIRE.md", "CLIENT-WIRE.md"]]) {
  await copyFile(new URL(`../../../docs/consumer-kit/${source}`, import.meta.url), new URL(`../dist/${target}`, import.meta.url));
}
await mkdir(new URL("../dist/external-input-python/", import.meta.url));
for (const name of ["service.py", "input-rules.example.json"]) {
  await copyFile(new URL(`../../../examples/external-input-python/${name}`, import.meta.url), new URL(`../dist/external-input-python/${name}`, import.meta.url));
}
await copyFile(new URL("../../../orbisync.toml.example", import.meta.url), new URL("../dist/orbisync.toml.example", import.meta.url));
await mkdir(new URL("../dist/protocol/orbisync/v1/", import.meta.url), { recursive: true });
await mkdir(new URL("../dist/protocol/http/", import.meta.url));
await copyFile(new URL("../../../proto/orbisync/v1/realtime.proto", import.meta.url), new URL("../dist/protocol/orbisync/v1/realtime.proto", import.meta.url));
for (const name of ["orbisync-v1.yaml", "errors.yaml"]) {
  await copyFile(new URL(`../../../openapi/${name}`, import.meta.url), new URL(`../dist/protocol/http/${name}`, import.meta.url));
}
