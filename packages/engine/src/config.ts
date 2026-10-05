import { homedir } from 'node:os';
import { fileURLToPath } from 'node:url';
import { join, resolve } from 'node:path';

export interface Config {
  port: number;
  home: string;
  uiDist: string;
}

const defaultPort = 4200;

function parsePort(raw: string | undefined): number {
  if (raw === undefined || raw === '') return defaultPort;
  const port = Number(raw);
  if (!Number.isInteger(port) || port < 0 || port > 65535) {
    throw new Error(`KRATOS_PORT must be an integer between 0 and 65535, got "${raw}"`);
  }
  return port;
}

export function loadConfig(env: NodeJS.ProcessEnv = process.env): Config {
  return {
    port: parsePort(env.KRATOS_PORT),
    home: resolve(env.KRATOS_HOME || join(homedir(), '.kratos')),
    // Both src/ (tsx) and dist/ (bundle) sit one level below the engine package, so the same relative path works.
    uiDist: fileURLToPath(new URL('../../ui/dist', import.meta.url)),
  };
}
