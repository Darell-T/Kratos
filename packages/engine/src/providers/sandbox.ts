import { z } from 'zod';

/** Keeps a thread's writes inside its folder. A thread without one behaves as it always has. */
export const threadSandboxSchema = z.object({
  /** Writable besides the thread's own folder. */
  extraWritableRoots: z.array(z.string().min(1)),
  /** A write outside the roots: ask the user first, or refuse without asking. */
  outsideWrites: z.enum(['ask', 'deny']),
  /** Set for every command the thread runs, for example a package cache inside a writable root. */
  env: z.record(z.string(), z.string()),
});
export type ThreadSandbox = z.infer<typeof threadSandboxSchema>;

export function writableRoots(folder: string, sandbox: ThreadSandbox): string[] {
  return [folder, ...sandbox.extraWritableRoots];
}

/** One command line, run through Codex's sandbox. */
export interface CommandRequest {
  commandLine: string;
  cwd: string;
  writableRoots: readonly string[];
  env: Readonly<Record<string, string>>;
  timeoutMs: number;
  /** Runs without the sandbox. Only after the user approved this exact command. */
  outsideSandbox: boolean;
}

export type CommandResult =
  | { kind: 'exited'; exitCode: number; stdout: string; stderr: string }
  | { kind: 'timedOut'; output: string }
  /** The sandbox refused the command, for example a write outside the roots. */
  | { kind: 'refused'; error: string };

export type RunCommand = (request: CommandRequest) => Promise<CommandResult>;

/** Agents write their commands for the platform shell: PowerShell on Windows, sh elsewhere. */
export function shellArgv(platform: NodeJS.Platform, commandLine: string): string[] {
  return platform === 'win32'
    ? ['powershell.exe', '-NoProfile', '-NonInteractive', '-Command', commandLine]
    : ['/bin/sh', '-c', commandLine];
}
