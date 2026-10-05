import { useState, type FormEvent } from 'react';
import {
  providers,
  type CreateThreadBody,
  type CreateWorkspaceBody,
  type Provider,
  type Thread,
  type Workspace,
} from '@kratos/protocol';

interface SidebarProps {
  workspaces: Workspace[];
  threads: Thread[];
  activeThreadId: string | null;
  loadError: string | null;
  onOpenThread: (threadId: string) => void;
  onCreateWorkspace: (body: CreateWorkspaceBody) => Promise<Workspace>;
  onCreateThread: (body: CreateThreadBody) => Promise<void>;
}

function useFormAction(action: () => Promise<void>) {
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const submit = async (event: FormEvent) => {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await action();
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : String(failure));
    } finally {
      setBusy(false);
    }
  };
  return { error, busy, submit };
}

function WorkspaceForm({ onCreate }: { onCreate: (body: CreateWorkspaceBody) => Promise<void> }) {
  const [name, setName] = useState('');
  const [path, setPath] = useState('');
  const { error, busy, submit } = useFormAction(async () => {
    await onCreate({ name, path });
    setName('');
    setPath('');
  });

  return (
    <form className="form" onSubmit={(event) => void submit(event)}>
      <h2>Add workspace</h2>
      <input placeholder="Name" value={name} onChange={(event) => setName(event.target.value)} required />
      <input
        placeholder="Full path to the project folder"
        value={path}
        onChange={(event) => setPath(event.target.value)}
        required
      />
      <button type="submit" disabled={busy}>
        Add workspace
      </button>
      {error && <p className="error">{error}</p>}
    </form>
  );
}

function ThreadForm({
  workspace,
  onCreate,
}: {
  workspace: Workspace | undefined;
  onCreate: (body: CreateThreadBody) => Promise<void>;
}) {
  const [title, setTitle] = useState('');
  const [provider, setProvider] = useState<Provider>(providers[0]);
  const { error, busy, submit } = useFormAction(async () => {
    if (!workspace) return;
    await onCreate({ workspaceId: workspace.id, title, provider });
    setTitle('');
  });

  return (
    <form className="form" onSubmit={(event) => void submit(event)}>
      <h2>Start thread{workspace ? ` in ${workspace.name}` : ''}</h2>
      <input placeholder="Title" value={title} onChange={(event) => setTitle(event.target.value)} required />
      <select value={provider} onChange={(event) => setProvider(event.target.value as Provider)}>
        {providers.map((name) => (
          <option key={name} value={name}>
            {name}
          </option>
        ))}
      </select>
      <button type="submit" disabled={busy || !workspace}>
        Start thread
      </button>
      {error && <p className="error">{error}</p>}
    </form>
  );
}

export function Sidebar({
  workspaces,
  threads,
  activeThreadId,
  loadError,
  onOpenThread,
  onCreateWorkspace,
  onCreateThread,
}: SidebarProps) {
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const selected = workspaces.find((workspace) => workspace.id === selectedId) ?? workspaces[0];

  return (
    <aside className="sidebar">
      <h1>Kratos</h1>
      {loadError && <p className="error">Could not load workspaces: {loadError}</p>}
      <nav className="workspaces">
        {workspaces.map((workspace) => (
          <section key={workspace.id}>
            <button
              type="button"
              className={workspace.id === selected?.id ? 'workspace selected' : 'workspace'}
              title={workspace.path}
              onClick={() => setSelectedId(workspace.id)}
            >
              {workspace.name}
            </button>
            <ul>
              {threads
                .filter((thread) => thread.workspaceId === workspace.id)
                .map((thread) => (
                  <li key={thread.id}>
                    <button
                      type="button"
                      className={thread.id === activeThreadId ? 'thread active' : 'thread'}
                      onClick={() => {
                        setSelectedId(workspace.id);
                        onOpenThread(thread.id);
                      }}
                    >
                      <span className="thread-title">{thread.title}</span>
                      <span className={`badge badge-${thread.provider}`}>{thread.provider}</span>
                    </button>
                  </li>
                ))}
            </ul>
          </section>
        ))}
      </nav>
      <ThreadForm workspace={selected} onCreate={onCreateThread} />
      <WorkspaceForm
        onCreate={async (body) => {
          const workspace = await onCreateWorkspace(body);
          setSelectedId(workspace.id);
        }}
      />
    </aside>
  );
}
