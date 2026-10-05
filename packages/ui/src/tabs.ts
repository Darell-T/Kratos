import { useCallback, useEffect, useState } from 'react';
import { z } from 'zod';

export interface TabState {
  openTabs: string[];
  activeTab: string | null;
}

export function openTab(state: TabState, id: string): TabState {
  return { openTabs: state.openTabs.includes(id) ? state.openTabs : [...state.openTabs, id], activeTab: id };
}

export function closeTab(state: TabState, id: string): TabState {
  const index = state.openTabs.indexOf(id);
  if (index === -1) return state;
  const openTabs = state.openTabs.filter((tab) => tab !== id);
  const activeTab = state.activeTab === id ? (openTabs[Math.min(index, openTabs.length - 1)] ?? null) : state.activeTab;
  return { openTabs, activeTab };
}

export function pruneTabs(state: TabState, knownThreadIds: ReadonlySet<string>): TabState {
  return state.openTabs.filter((id) => !knownThreadIds.has(id)).reduce(closeTab, state);
}

const openTabsKey = 'openTabs';
const activeTabKey = 'activeTab';

function loadTabs(): TabState {
  try {
    const stored: unknown = JSON.parse(localStorage.getItem(openTabsKey) ?? '[]');
    const openTabs = z.array(z.string()).catch([]).parse(stored);
    const active = localStorage.getItem(activeTabKey);
    return { openTabs, activeTab: active && openTabs.includes(active) ? active : (openTabs.at(-1) ?? null) };
  } catch {
    return { openTabs: [], activeTab: null };
  }
}

function saveTabs({ openTabs, activeTab }: TabState): void {
  try {
    localStorage.setItem(openTabsKey, JSON.stringify(openTabs));
    if (activeTab === null) localStorage.removeItem(activeTabKey);
    else localStorage.setItem(activeTabKey, activeTab);
  } catch {
    // Tabs still work for this session when storage is unavailable.
  }
}

export function useTabs() {
  const [state, setState] = useState(loadTabs);
  useEffect(() => saveTabs(state), [state]);
  return {
    ...state,
    open: useCallback((id: string) => setState((current) => openTab(current, id)), []),
    close: useCallback((id: string) => setState((current) => closeTab(current, id)), []),
    prune: useCallback((ids: ReadonlySet<string>) => setState((current) => pruneTabs(current, ids)), []),
  };
}
