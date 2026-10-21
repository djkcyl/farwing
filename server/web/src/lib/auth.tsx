// Who is signed in: the account, its role and whether this login passed
// two-step verification. Pages read it through `useAuth()`.
import { createContext, useCallback, useContext, useEffect, useMemo, useState, type ReactNode } from 'react';
import {
  ApiError,
  call,
  hasToken,
  login as apiLogin,
  logout as apiLogout,
  register as apiRegister,
  setUnauthorizedHandler,
  syncClock,
} from './api';
import type { Me, Security, WebInfo } from './types';

export type AuthStatus = 'loading' | 'offline' | 'signed_out' | 'second_factor' | 'signed_in';

interface AuthState {
  status: AuthStatus;
  me?: Me;
  security?: Security;
  info?: WebInfo;
  /// Why the user was signed out (shown on the login page).
  notice?: string;
}

interface AuthContextValue extends AuthState {
  isAdmin: boolean;
  login(username: string, password: string): Promise<void>;
  register(invite: string, username: string, password: string, nickname: string): Promise<void>;
  verify(code: string): Promise<void>;
  logout(): Promise<void>;
  refresh(): Promise<void>;
}

const AuthContext = createContext<AuthContextValue | null>(null);

export function AuthProvider({ children }: { children: ReactNode }) {
  const [state, setState] = useState<AuthState>({ status: 'loading' });

  const load = useCallback(async (): Promise<void> => {
    const info = await call<WebInfo>('GET', '/api/v2/web/info', undefined, { sign: false }).catch(() => undefined);
    if (!hasToken()) {
      setState({ status: 'signed_out', info });
      return;
    }
    try {
      const [me, security] = await Promise.all([
        call<Me>('GET', '/api/v2/me'),
        call<Security>('GET', '/api/v2/me/security'),
      ]);
      setState({
        status: security.totp && !security.verified ? 'second_factor' : 'signed_in',
        me,
        security,
        info,
      });
    } catch (error) {
      if (!(error instanceof ApiError)) {
        // The server cannot be reached; the login may still be fine.
        setState((s) => ({ ...s, status: 'offline', info }));
        return;
      }
      setState((s) => ({
        status: 'signed_out',
        info,
        notice: hasToken() ? undefined : (s.notice ?? '登录已过期'),
      }));
    }
  }, []);

  useEffect(() => {
    setUnauthorizedHandler(() =>
      setState((s) => (s.status === 'signed_out' ? s : { status: 'signed_out', info: s.info, notice: '登录已过期' })),
    );
    void syncClock()
      .catch(() => undefined)
      .then(load);
    return () => setUnauthorizedHandler(undefined);
  }, [load]);

  const value = useMemo<AuthContextValue>(
    () => ({
      ...state,
      isAdmin: state.status === 'signed_in' && state.security?.role === 'admin',
      async login(username, password) {
        await syncClock().catch(() => undefined);
        await apiLogin(username, password);
        await load();
      },
      async register(invite, username, password, nickname) {
        await syncClock().catch(() => undefined);
        await apiRegister(invite, username, password, nickname);
        await load();
      },
      async verify(code) {
        await call('POST', '/api/v2/auth/totp', { code });
        await load();
      },
      async logout() {
        await apiLogout().catch(() => undefined);
        setState((s) => ({ status: 'signed_out', info: s.info }));
      },
      refresh: load,
    }),
    [state, load],
  );

  return <AuthContext.Provider value={value}>{children}</AuthContext.Provider>;
}

export function useAuth(): AuthContextValue {
  const value = useContext(AuthContext);
  if (!value) throw new Error('useAuth outside AuthProvider');
  return value;
}
