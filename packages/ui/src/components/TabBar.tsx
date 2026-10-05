import type { MouseEvent } from 'react';
import type { Thread } from '@kratos/protocol';

interface TabBarProps {
  tabs: Thread[];
  activeId: string | null;
  onSelect: (threadId: string) => void;
  onClose: (threadId: string) => void;
}

export function TabBar({ tabs, activeId, onSelect, onClose }: TabBarProps) {
  // Middle click fires auxclick after mouseup. Preventing mousedown stops Linux's middle-click autoscroll.
  const closeOnMiddleClick = (event: MouseEvent, threadId: string) => {
    if (event.button !== 1) return;
    event.preventDefault();
    onClose(threadId);
  };

  return (
    <div className="tab-bar" role="tablist">
      {tabs.map((thread) => (
        <div
          key={thread.id}
          role="tab"
          aria-selected={thread.id === activeId}
          className={thread.id === activeId ? 'tab active' : 'tab'}
          onMouseDown={(event) => {
            if (event.button === 1) event.preventDefault();
          }}
          onAuxClick={(event) => closeOnMiddleClick(event, thread.id)}
        >
          <button type="button" className="tab-label" onClick={() => onSelect(thread.id)}>
            {thread.title}
          </button>
          <button
            type="button"
            className="tab-close"
            aria-label={`Close ${thread.title}`}
            onClick={() => onClose(thread.id)}
          >
            ×
          </button>
        </div>
      ))}
    </div>
  );
}
