import { useEffect, useRef, useState, type KeyboardEvent } from 'react';
import type { Thread, ThreadEvent } from '@kratos/protocol';
import { postMessage, subscribeToThread } from '../api';

interface ThreadViewProps {
  thread: Thread;
  active: boolean;
}

function Composer({ threadId, active }: { threadId: string; active: boolean }) {
  const [text, setText] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [sending, setSending] = useState(false);
  const input = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    if (active) input.current?.focus();
  }, [active]);

  const send = async () => {
    const trimmed = text.trim();
    if (!trimmed || sending) return;
    setSending(true);
    setError(null);
    try {
      await postMessage(threadId, trimmed);
      setText('');
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : String(failure));
    } finally {
      setSending(false);
    }
  };

  const onKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    // isComposing keeps Enter from sending while an input method editor is confirming text.
    if (event.key !== 'Enter' || event.shiftKey || event.nativeEvent.isComposing) return;
    event.preventDefault();
    void send();
  };

  return (
    <div className="composer">
      {error && <p className="error">{error}</p>}
      <textarea
        ref={input}
        rows={3}
        placeholder="Message (Enter to send, Shift+Enter for a new line)"
        value={text}
        onChange={(event) => setText(event.target.value)}
        onKeyDown={onKeyDown}
      />
    </div>
  );
}

function EventBubble({ event }: { event: ThreadEvent }) {
  switch (event.event.kind) {
    case 'user.message':
      return (
        <div className="bubble-row">
          <div className="bubble" title={new Date(event.at).toLocaleString()}>
            {event.event.text}
          </div>
        </div>
      );
  }
}

export function ThreadView({ thread, active }: ThreadViewProps) {
  const [events, setEvents] = useState<ThreadEvent[]>([]);
  const list = useRef<HTMLDivElement>(null);

  useEffect(() => subscribeToThread(thread.id, (event) => setEvents((current) => [...current, event])), [thread.id]);

  useEffect(() => {
    if (active && list.current) list.current.scrollTop = list.current.scrollHeight;
  }, [active, events.length]);

  return (
    <section className="thread-view" hidden={!active}>
      <div className="messages" ref={list}>
        {events.map((event) => (
          <EventBubble key={event.seq} event={event} />
        ))}
      </div>
      <Composer threadId={thread.id} active={active} />
    </section>
  );
}
