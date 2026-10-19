// Routes of the web console.
import { lazy, Suspense, useEffect, type ComponentType, type LazyExoticComponent } from 'react';
import { AuthProvider, useAuth } from './lib/auth';
import { match, navigate, RouterProvider, useLocation } from './lib/router';
import { Button, Empty, Loading } from './ui/kit';
import { OverlayProvider } from './ui/overlay';
import { Shell } from './ui/Shell';
import { LoginPage, RegisterPage, SecondFactorPage } from './pages/Login';

type Page =
  | ComponentType<{ params: Record<string, string> }>
  | LazyExoticComponent<ComponentType<{ params: Record<string, string> }>>;

const page = (load: () => Promise<{ default: ComponentType<{ params: Record<string, string> }> }>) => lazy(load);

const USER_ROUTES: [string, Page][] = [
  ['/', page(() => import('./pages/me/Home'))],
  ['/devices', page(() => import('./pages/me/Devices'))],
  ['/devices/:id', page(() => import('./pages/me/DeviceDetail'))],
  ['/friends', page(() => import('./pages/me/Friends'))],
  ['/favorites', page(() => import('./pages/me/Favorites'))],
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
  ['/admin/audit', page(() => import('./pages/admin/Audit'))],
];

const ViewPage = page(() => import('./pages/View'));
const VisitPage = page(() => import('./pages/Visit'));

function find(routes: [string, Page][], path: string): [Page, Record<string, string>] | undefined {
  for (const [pattern, component] of routes) {
    const params = match(pattern, path);
    if (params) return [component, params];
  }
  return undefined;
}

function NotFound() {
  return <Empty icon="alert" title="页面不存在" hint="地址可能有误，或页面已经移动。" />;
}

function Routes() {
  const auth = useAuth();
  const { path } = useLocation();

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
  if (auth.status === 'loading') return <Loading text="正在连接服务器" />;
  if (auth.status === 'offline') {
    return (
      <div className="auth-page">
        <Empty
          icon="alert"
          title="无法连接服务器"
          hint="请检查网络，或稍后再试。"
          action={
            <Button variant="primary" icon="refresh" onClick={() => void auth.refresh()}>
              重试
            </Button>
          }
        />
      </div>
    );
  }
  if (auth.status === 'second_factor') return <SecondFactorPage />;
  if (auth.status === 'signed_out') return path === '/register' ? <RegisterPage /> : <LoginPage />;

  const view = match('/view/:id', path);
  if (view) {
    return (
      <Suspense fallback={<Loading />}>
        <ViewPage params={view} />
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
          <Empty icon="lock" title="没有权限" hint="只有管理员可以打开这个页面。" />
        ) : (
          <NotFound />
        )}
      </Suspense>
    </Shell>
  );
}

export default function App() {
  return (
    <RouterProvider>
      <OverlayProvider>
        <AuthProvider>
          <Routes />
        </AuthProvider>
      </OverlayProvider>
    </RouterProvider>
  );
}
