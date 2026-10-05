export class CodexRequestError extends Error {
  readonly code: number;

  constructor(code: number, message: string) {
    super(message);
    this.name = 'CodexRequestError';
    this.code = code;
  }
}

export class CodexExitedError extends Error {
  readonly exitCode: number | null;

  constructor(exitCode: number | null) {
    super(`Codex exited (code ${exitCode ?? 'unknown'})`);
    this.name = 'CodexExitedError';
    this.exitCode = exitCode;
  }
}

export class CodexNotFoundError extends Error {
  constructor(command: string) {
    super(`Could not find "${command}". Install Codex with "npm install -g @openai/codex" or set CODEX_EXE.`);
    this.name = 'CodexNotFoundError';
  }
}
