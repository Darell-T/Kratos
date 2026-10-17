import { build } from 'esbuild';

await build({
  entryPoints: ['src/main.ts'],
  outfile: 'dist/main.js',
  bundle: true,
  platform: 'node',
  format: 'esm',
  target: 'node24',
  // The SDK locates its native claude.exe platform package relative to its own installed files.
  external: ['@anthropic-ai/claude-agent-sdk'],
  // ws is CommonJS and calls require() on Node builtins, which an ESM bundle does not define.
  banner: {
    js: "import { createRequire } from 'node:module'; const require = createRequire(import.meta.url);",
  },
});
