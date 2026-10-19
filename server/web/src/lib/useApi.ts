// Load a resource for a page, optionally polling while the page is visible.
import { useCallback, useEffect, useRef, useState } from 'react';
import { call, errorText } from './api';

export interface Resource<T> {
  data?: T;
  error?: string;
  loading: boolean;
  reload(): Promise<void>;
}

interface State<T> {
  path: string | null;
  data?: T;
  error?: string;
}

export function useApi<T>(path: string | null, pollMs = 0): Resource<T> {
  const [state, setState] = useState<State<T>>({ path: null });
  const latest = useRef(path);
  useEffect(() => {
    latest.current = path;
  }, [path]);

  const reload = useCallback(async () => {
    if (path === null) return;
    try {
      const data = await call<T>('GET', path);
      if (latest.current === path) setState({ path, data });
    } catch (e) {
      if (latest.current === path) {
        setState((s) => ({ path, data: s.path === path ? s.data : undefined, error: errorText(e) }));
      }
    }
  }, [path]);

  useEffect(() => {
    // State changes only after the request returns.
    // oxlint-disable-next-line react/set-state-in-effect
    void reload();
    if (!pollMs) return;
    const timer = window.setInterval(() => {
      if (document.visibilityState === 'visible') void reload();
    }, pollMs);
    return () => window.clearInterval(timer);
  }, [reload, pollMs]);

  const current = state.path === path;
  return {
    data: current ? state.data : undefined,
    error: current ? state.error : undefined,
    loading: path !== null && !current,
    reload,
  };
}
