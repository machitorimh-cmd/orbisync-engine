import { defineConfig } from "vite";
import { fileURLToPath } from "node:url";
export default defineConfig({
    resolve: {
        alias: [
            {
                find: /^@bufbuild\/protobuf$/,
                replacement: fileURLToPath(
                    new URL(
                        "./node_modules/@bufbuild/protobuf/dist/esm/index.js",
                        import.meta.url,
                    ),
                ),
            },
            {
                find: /^@bufbuild\/protobuf\/(.*)$/,
                replacement:
                    fileURLToPath(
                        new URL(
                            "./node_modules/@bufbuild/protobuf/dist/esm/",
                            import.meta.url,
                        ),
                    ) + "$1/index.js",
            },
        ],
    },
    server: {
        host: "127.0.0.1",
        port: 5173,
        fs: { allow: [fileURLToPath(new URL("../..", import.meta.url))] },
    },
    build: {
        rollupOptions: { output: { manualChunks: { three: ["three"] } } },
    },
});
