// Routes of the web console.
import { lazy, Suspense, useEffect, type ComponentType, type LazyExoticComponent } from 'react';
import { LangProvider, useT } from './i18n/react';
import { AuthProvider, useAuth } from './lib/auth';
import { match, navigate, RouterProvider, useLocation } from './lib/router';
import { chat } from './lib/chat/store';
import { Button, Empty, Loading } from './ui/kit';
import { OverlayProvider } from './ui/overlay';
import { LangSwitch } from './ui/LangSwitch';
import { Shell } from './ui/Shell';
import { LoginPage, RegisterPage, SecondFactorPage } from './pages/Login';

type Page =
  | ComponentType<{ params: Record<string, string> }>
  | LazyExoticComponent<ComponentType<{ params: Record<string, string> }>>;

const page = (load: () => Promise<{ default: ComponentType<{ params: Record<string, string> }> }>) => lazy(load);

const ChatPage = page(() => import('./pages/chat/Chat'));

const USER_ROUTES: [string, Page][] = [
  ['/', page(() => import('./pages/me/Home'))],
  ['/devices', page(() => import('./pages/me/Devices'))],
  ['/devices/:id', page(() => import('./pages/me/DeviceDetail'))],
  ['/friends', page(() => import('./pages/me/Friends'))],
  ['/chat', ChatPage],
  ['/chat/dm', ChatPage],
  ['/chat/dm/:channel', ChatPage],
  ['/chat/invite/:code', ChatPage],
  ['/chat/:group', ChatPage],
  ['/chat/:group/:channel', ChatPage],
  ['/favorites', page(() => import('./pages/me/Favorites'))],
  ['/records', page(() => import('./pages/me/Records'))],
  ['/notifications', page(() => import('./pages/me/Notices'))],
  ['/account', page(() => import('./pages/me/Account'))],
  ['/connect', page(() => import('./pages/me/Connect'))],
];

const ADMIN_ROUTES: [string, Page][] = [
  ['/admin', page(() => import('./pages/admin/Overview'))],
  ['/admin/users', page(() => import('./pages/admin/Users'))],
  ['/admin/users/:id', page(() => import('./pages/admin/UserDetail'))],
  ['/admin/invites', page(() => import('./pages/admin/Invites'))],
  ['/admin/devices', page(() => import('./pages/admin/Devices'))],
  ['/admin/sessions', page(() => import('./pages/admin/Sessions'))],
  ['/admin/updates', page(() => import('./pages/admin/Updates'))],
  ['/admin/settings', page(() => import('./pages/admin/Settings'))],
  ['/admin/tokens', page(() => import('./pages/admin/Tokens'))],
  ['/admin/nodes', page(() => import('./pages/admin/Nodes'))],
  ['/admin/records', page(() => import('./pages/admin/Records'))],
  ['/admin/audit', page(() => import('./pages/admin/Audit'))],
  ['/admin/groups', page(() => import('./pages/admin/Groups'))],
];

const ViewPage = page(() => import('./pages/View'));
const SetupPage = page(() => import('./pages/Setup'));
const VisitPage = page(() => import('./pages/Visit'));

function find(routes: [string, Page][], path: string): [Page, Record<string, string>] | undefined {
  for (const [pattern, component] of routes) {
    const params = match(pattern, path);
    if (params) return [component, params];
  }
  return undefined;
}

function NotFound() {
  const t = useT();
  return <Empty icon="alert" title={t.common.notFound} />;
}

function Routes() {
  const t = useT();
  const auth = useAuth();
  const { path } = useLocation();

  // One chat connection while signed in, on every page.
  useEffect(() => {
    if (auth.status !== 'signed_in') return;
    chat.start();
    return () => chat.stop();
  }, [auth.status]);

  // Pages for signed-out visitors send signed-in users home.
  useEffect(() => {
    if (auth.status === 'signed_in' && (path === '/login' || path === '/register')) navigate('/', true);
  }, [auth.status, path]);

  if (path === '/visit' || path.startsWith('/visit/')) {
    return (
      <Suspense fallback={<Loading />}>
        <VisitPage params={{}} />
      </Suspense>
    );
  }
  if (path === '/setup' || auth.status === 'setup') {
    return (
      <Suspense fallback={<Loading />}>
        <SetupPage params={{}} />
      </Suspense>
    );
  }
  if (auth.status === 'loading') return <Loading text={t.common.connectingServer} />;
  if (auth.status === 'offline') {
    return (
      <div className="auth-page">
        <LangSwitch className="lang-corner" />
        <Empty
          icon="alert"
          title={t.common.cannotConnect}
          action={
            <Button variant="primary" icon="refresh" onClick={() => void auth.refresh()}>
              {t.common.retry}
            </Button>
          }
        />
      </div>
    );
  }
  if (auth.status === 'second_factor') return <SecondFactorPage />;
  if (auth.status === 'signed_out') return path === '/register' ? <RegisterPage /> : <LoginPage />;

  const view = match('/view/:id', path);
  const camera = match('/camera/:id', path);
  if (view || camera) {
    return (
      <Suspense fallback={<Loading />}>
        <ViewPage params={view ?? { ...camera, camera: '1' }} />
      </Suspense>
    );
  }

  const found = find(USER_ROUTES, path) ?? (auth.isAdmin ? find(ADMIN_ROUTES, path) : undefined);
  const forbidden = !found && !auth.isAdmin && find(ADMIN_ROUTES, path);
  const [Component, params] = found ?? [undefined, {}];
  return (
    <Shell>
      <Suspense fallback={<Loading />}>
        {Component ? (
          <Component params={params} />
        ) : forbidden ? (
          <Empty icon="lock" title={t.common.forbidden} />
        ) : (
          <NotFound />
        )}
      </Suspense>
    </Shell>
  );
}

export default function App() {
  return (
    <LangProvider>
      <RouterProvider>
        <OverlayProvider>
          <AuthProvider>
            <Routes />
          </AuthProvider>
        </OverlayProvider>
      </RouterProvider>
    </LangProvider>
  );
}
