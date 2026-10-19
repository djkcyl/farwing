// A small history router: the console has a dozen flat pages, no nested
// layouts, so a dependency is not worth it.
import { createContext, useContext, useEffect, useState, type AnchorHTMLAttributes, type ReactNode } from 'react';
import { BASE, sitePath } from './base';

interface Location {
  path: string;
  query: URLSearchParams;
}

/// The page path without the site's base (`/admin`, not `/farwing/admin`).
function current(): Location {
  const path = location.pathname.startsWith(BASE) ? location.pathname.slice(BASE.length) : location.pathname;
  return { path: path || '/', query: new URLSearchParams(location.search) };
}

const listeners = new Set<() => void>();

export function navigate(to: string, replace = false) {
  const target = sitePath(to);
  if (target === location.pathname + location.search) return;
  if (replace) history.replaceState(null, '', target);
  else history.pushState(null, '', target);
  listeners.forEach((l) => l());
  window.scrollTo(0, 0);
}

window.addEventListener('popstate', () => listeners.forEach((l) => l()));

const LocationContext = createContext<Location>(current());

export function RouterProvider({ children }: { children: ReactNode }) {
  const [loc, setLoc] = useState(current);
  useEffect(() => {
    const update = () => setLoc(current());
    listeners.add(update);
    return () => {
      listeners.delete(update);
    };
  }, []);
  return <LocationContext.Provider value={loc}>{children}</LocationContext.Provider>;
}

export function useLocation(): Location {
  return useContext(LocationContext);
}

/// Match `/admin/users/:id` against a path; returns the parameters.
export function match(pattern: string, path: string): Record<string, string> | undefined {
  const a = pattern.split('/').filter(Boolean);
  const b = path.split('/').filter(Boolean);
  if (a.length !== b.length) return undefined;
  const params: Record<string, string> = {};
  for (let i = 0; i < a.length; i++) {
    if (a[i].startsWith(':')) params[a[i].slice(1)] = decodeURIComponent(b[i]);
    else if (a[i] !== b[i]) return undefined;
  }
  return params;
}

export function Link({ to, children, ...rest }: AnchorHTMLAttributes<HTMLAnchorElement> & { to: string }) {
  return (
    <a
      href={sitePath(to)}
      {...rest}
      onClick={(event) => {
        rest.onClick?.(event);
        if (event.defaultPrevented || event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey) return;
        event.preventDefault();
        navigate(to);
      }}
    >
      {children}
    </a>
  );
}
