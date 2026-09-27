import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
const root = fileURLToPath(new URL("../../..", import.meta.url));
const buf = fileURLToPath(
    new URL("../node_modules/@bufbuild/buf/bin/buf", import.meta.url),
);
const plugin = fileURLToPath(
    new URL(
        "../node_modules/@bufbuild/protoc-gen-es/bin/protoc-gen-es",
        import.meta.url,
    ),
);
const result = spawnSync(
    process.execPath,
    [
        buf,
        "generate",
        "--template",
        JSON.stringify({
            version: "v2",
            plugins: [
                {
                    local: [process.execPath, plugin],
                    out: "sdk/typescript/src/generated",
                    opt: ["target=ts"],
                },
            ],
        }),
    ],
    { cwd: root, stdio: "inherit" },
);
if (result.error) console.error(result.error.message);
process.exit(result.status ?? 1);
