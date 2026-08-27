import tailwindcss from '@tailwindcss/vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';
import { svelteTesting } from '@testing-library/svelte/vite';
import { defineConfig, loadEnv } from 'vite';

export default defineConfig(({ mode }) => {
  const env = loadEnv(mode, process.cwd(), '');
  const modelEndpoint =
    env.BLACKWALL_DEV_MODEL_ENDPOINT || env.OLLAMA_HOST || 'http://127.0.0.1:11434';

  return {
    plugins: [tailwindcss(), svelte(), svelteTesting({ autoCleanup: false })],
    clearScreen: false,
    server: {
      host: '127.0.0.1',
      port: 1420,
      strictPort: true,
      proxy: {
        '/local-llm': {
          target: modelEndpoint,
          changeOrigin: true,
          rewrite: (path) => path.replace(/^\/local-llm/, ''),
        },
      },
      watch: {
        ignored: ['**/src/app/**', '**/src/core/**', '**/src/bw/**', '**/src/relay/**'],
      },
    },
    build: {
      target: ['es2022', 'chrome105', 'safari13'],
      minify: process.env.TAURI_DEBUG ? false : 'oxc',
      sourcemap: Boolean(process.env.TAURI_DEBUG),
    },
    test: {
      environment: 'jsdom',
      setupFiles: ['./src/test/setup.ts'],
      include: ['src/**/*.test.ts'],
      css: true,
      restoreMocks: true,
    },
  };
});
