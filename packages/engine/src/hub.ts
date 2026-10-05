import type { ThreadEvent } from '@kratos/protocol';

export type Listener = (event: ThreadEvent) => void;

export class Hub {
  private readonly listeners = new Map<string, Set<Listener>>();

  subscribe(threadId: string, listener: Listener): () => void {
    let forThread = this.listeners.get(threadId);
    if (!forThread) {
      forThread = new Set();
      this.listeners.set(threadId, forThread);
    }
    forThread.add(listener);
    return () => {
      forThread.delete(listener);
      if (forThread.size === 0) this.listeners.delete(threadId);
    };
  }

  publish(event: ThreadEvent): void {
    for (const listener of this.listeners.get(event.threadId) ?? []) listener(event);
  }
}
