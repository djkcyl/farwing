// The signed-in layout: sidebar navigation (user centre, administration)
// and the page.
import { useState, type ReactNode } from 'react';
import { useAuth } from '../lib/auth';
import { displayName } from '../lib/format';
import { Link, useLocation } from '../lib/router';
import { Icon, type IconName } from './icons';
import { IconButton } from './kit';
import { useOverlay } from './overlay';

interface NavItem {
  to: string;
  label: string;
  icon: IconName;
  /// Active for sub-paths too.
  prefix?: boolean;
}

const USER_NAV: NavItem[] = [
  { to: '/', label: '概览', icon: 'home' },
  { to: '/devices', label: '设备', icon: 'monitor', prefix: true },
  { to: '/friends', label: '好友', icon: 'users' },
  { to: '/favorites', label: '收藏', icon: 'star' },
  { to: '/account', label: '账号与安全', icon: 'shield' },
  { to: '/connect', label: '连接到本服务器', icon: 'link' },
];

const ADMIN_NAV: NavItem[] = [
  { to: '/admin', label: '服务器概览', icon: 'gauge' },
  { to: '/admin/users', label: '账号', icon: 'user', prefix: true },
  { to: '/admin/invites', label: '邀请码', icon: 'ticket' },
  { to: '/admin/devices', label: '全部设备', icon: 'server' },
  { to: '/admin/sessions', label: '会话', icon: 'activity' },
  { to: '/admin/updates', label: '客户端更新', icon: 'download' },
  { to: '/admin/settings', label: '服务器设置', icon: 'settings' },
  { to: '/admin/audit', label: '审计日志', icon: 'log' },
];

function active(item: NavItem, path: string): boolean {
  if (item.to === '/') return path === '/';
  return path === item.to || (!!item.prefix && path.startsWith(`${item.to}/`));
}

export function BrandMark({ size = 18 }: { size?: number }) {
  return (
    <span className="brand-mark">
      <Icon name="logo" size={size} strokeWidth={2} />
    </span>
  );
}

export function Shell({ children }: { children: ReactNode }) {
  const auth = useAuth();
  const { path } = useLocation();
  const { confirm } = useOverlay();
  const [open, setOpen] = useState(false);
  const name = displayName(auth.me);

  const nav = (items: NavItem[]) =>
    items.map((item) => (
      <Link key={item.to} to={item.to} className={active(item, path) ? 'active' : ''} onClick={() => setOpen(false)}>
        <Icon name={item.icon} size={17} />
        <span>{item.label}</span>
      </Link>
    ));

  return (
    <div className={`shell ${open ? 'nav-open' : ''}`}>
      <aside className="sidebar">
        <div className="brand">
          <BrandMark />
          <div className="grow">
            <div className="brand-name">Farwing</div>
            <div className="brand-sub ellipsis" title={auth.info?.instance_name}>
              {auth.info?.instance_name || 'Farwing Server'}
            </div>
          </div>
        </div>
        <nav className="nav">
          {nav(USER_NAV)}
          {auth.isAdmin && (
            <>
              <div className="nav-group">管理</div>
              {nav(ADMIN_NAV)}
            </>
          )}
        </nav>
        <div className="sidebar-foot">
          <span className="avatar">{(name || '?').slice(0, 1).toUpperCase()}</span>
          <div className="grow">
            <div className="ellipsis">{name}</div>
            <div className="small muted ellipsis">
              {auth.me?.username}
              {auth.isAdmin ? ' · 管理员' : ''}
            </div>
          </div>
          <IconButton
            icon="logout"
            label="退出登录"
            onClick={async () => {
              if (await confirm({ title: '退出登录', message: '这个浏览器将退出登录。', confirm: '退出' })) {
                await auth.logout();
              }
            }}
          />
        </div>
      </aside>
      <div className="main">
        <div className="topbar">
          <IconButton icon="menu" label="菜单" onClick={() => setOpen(true)} />
          <BrandMark size={16} />
          <span className="brand-name">Farwing</span>
        </div>
        <main className="content">{children}</main>
      </div>
    </div>
  );
}
