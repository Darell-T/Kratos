import { useEffect, useMemo, useState } from 'react';
import type { CreateThreadBody, CreateWorkspaceBody, Thread, Workspace } from '@kratos/protocol';
import { createThread, createWorkspace, listThreads, listWorkspaces } from './api';
import { Sidebar } from './components/Sidebar';
import { TabBar } from './components/TabBar';
import { ThreadView } from './components/ThreadView';
import { useTabs } from './tabs';

export function App() {
  const [workspaces, setWorkspaces] = useState<Workspace[]>([]);
  const [threads, setThreads] = useState<Thread[]>([]);
  const [loadError, setLoadError] = useState<string | null>(null);
  const { openTabs, activeTab, open, close, prune } = useTabs();

  useEffect(() => {
    let cancelled = false;
    const load = async () => {
      try {
        const loadedWorkspaces = await listWorkspaces();
        const loadedThreads = (
          await Promise.all(loadedWorkspaces.map((workspace) => listThreads(workspace.id)))
        ).flat();
        if (cancelled) return;
        setWorkspaces(loadedWorkspaces);
        setThreads(loadedThreads);
        prune(new Set(loadedThreads.map((thread) => thread.id)));
      } catch (error) {
        if (!cancelled) setLoadError(error instanceof Error ? error.message : String(error));
      }
    };
    void load();
    return () => {
      cancelled = true;
    };
  }, [prune]);

  const threadsById = useMemo(() => new Map(threads.map((thread) => [thread.id, thread])), [threads]);
  const openThreads = openTabs.flatMap((id) => threadsById.get(id) ?? []);

  const addWorkspace = async (body: CreateWorkspaceBody) => {
    const workspace = await createWorkspace(body);
    setWorkspaces((current) => [...current, workspace]);
    return workspace;
  };

  const addThread = async (body: CreateThreadBody) => {
    const thread = await createThread(body);
    setThreads((current) => [...current, thread]);
    open(thread.id);
  };

  return (
    <div className="app">
      <Sidebar
        workspaces={workspaces}
        threads={threads}
        activeThreadId={activeTab}
        loadError={loadError}
        onOpenThread={open}
        onCreateWorkspace={addWorkspace}
        onCreateThread={addThread}
      />
      <main className="main">
        <TabBar tabs={openThreads} activeId={activeTab} onSelect={open} onClose={close} />
        <div className="thread-area">
          {openThreads.length === 0 && <p className="empty">Open a thread from the sidebar or start a new one.</p>}
          {openThreads.map((thread) => (
            <ThreadView key={thread.id} thread={thread} active={thread.id === activeTab} />
          ))}
        </div>
      </main>
    </div>
  );
}
