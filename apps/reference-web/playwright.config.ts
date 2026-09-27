import { defineConfig } from "@playwright/test";
export default defineConfig({
  tsconfig: "./tsconfig.test.json",
    testDir: "./tests",
    timeout: 60000,
    fullyParallel: false,
    use: {
        baseURL: "http://127.0.0.1:5173",
        viewport: { width: 1440, height: 900 },
        launchOptions: {
            args: ["--use-angle=swiftshader", "--enable-unsafe-swiftshader"],
        },
        screenshot: "only-on-failure",
    },
    webServer: {
        command: "npm run dev -- --strictPort",
        url: "http://127.0.0.1:5173",
        reuseExistingServer: !process.env.CI,
        timeout: 60000,
    },
});
