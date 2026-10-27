// The signed-in layout: sidebar navigation (user centre, administration)
// and the page.
import { useState, type ReactNode } from 'react';
import type { Messages } from '../i18n';
import { useT } from '../i18n/react';
import { useAuth } from '../lib/auth';
import { displayName, instanceText } from '../lib/format';
import { useUnreadNotices } from '../lib/notices';
import { Link, useLocation } from '../lib/router';
import { BrandLogo } from './BrandLogo';
import { Icon, type IconName } from './icons';
import { IconButton } from './kit';
import { LangSwitch } from './LangSwitch';
import { useOverlay } from './overlay';

interface NavItem {
  to: string;
  label: keyof Messages['nav'];
  icon: IconName;
  /// Active for sub-paths too.
  prefix?: boolean;
}

const USER_NAV: NavItem[] = [
  { to: '/', label: 'home', icon: 'home' },
  { to: '/devices', label: 'devices', icon: 'monitor', prefix: true },
  { to: '/friends', label: 'friends', icon: 'users' },
  { to: '/favorites', label: 'favorites', icon: 'star' },
  { to: '/records', label: 'records', icon: 'history' },
  { to: '/notifications', label: 'notifications', icon: 'bell' },
  { to: '/account', label: 'account', icon: 'shield' },
  { to: '/connect', label: 'connect', icon: 'link' },
];

const ADMIN_NAV: NavItem[] = [
  { to: '/admin', label: 'adminHome', icon: 'gauge' },
  { to: '/admin/users', label: 'users', icon: 'user', prefix: true },
  { to: '/admin/invites', label: 'invites', icon: 'ticket' },
  { to: '/admin/devices', label: 'allDevices', icon: 'server' },
  { to: '/admin/sessions', label: 'sessions', icon: 'activity' },
  { to: '/admin/records', label: 'allRecords', icon: 'history' },
  { to: '/admin/updates', label: 'updates', icon: 'download' },
  { to: '/admin/nodes', label: 'nodes', icon: 'globe' },
  { to: '/admin/settings', label: 'settings', icon: 'settings' },
  { to: '/admin/tokens', label: 'tokens', icon: 'key' },
  { to: '/admin/audit', label: 'audit', icon: 'log' },
];

function active(item: NavItem, path: string): boolean {
  if (item.to === '/') return path === '/';
  return path === item.to || (!!item.prefix && path.startsWith(`${item.to}/`));
}

export function Shell({ children }: { children: ReactNode }) {
  const t = useT();
  const auth = useAuth();
  const { path } = useLocation();
  const { confirm } = useOverlay();
  const [open, setOpen] = useState(false);
  const name = displayName(auth.me);
  const unread = useUnreadNotices(auth.status === 'signed_in');

  const nav = (items: NavItem[]) =>
    items.map((item) => (
      <Link key={item.to} to={item.to} className={active(item, path) ? 'active' : ''} onClick={() => setOpen(false)}>
        <Icon name={item.icon} size={17} />
        <span>{t.nav[item.label]}</span>
        {item.to === '/notifications' && unread > 0 && <span className="nav-count">{unread}</span>}
      </Link>
    ));

  return (
    <div className={`shell ${open ? 'nav-open' : ''}`}>
      <aside className="sidebar">
        <div className="brand">
          <BrandLogo height={22} />
          {instanceText(auth.info?.instance_name) && (
            <div className="brand-sub ellipsis" title={auth.info?.instance_name}>
              {instanceText(auth.info?.instance_name)}
            </div>
          )}
        </div>
        <nav className="nav">
          {nav(USER_NAV)}
          {auth.isAdmin && (
            <>
              <div className="nav-group">{t.nav.adminGroup}</div>
              {nav(ADMIN_NAV)}
            </>
          )}
        </nav>
        <div className="sidebar-foot">
          <span className="avatar">{(name || '?').slice(0, 1).toUpperCase()}</span>
          <div className="grow">
            <div className="ellipsis">{name}</div>
            {(auth.me?.nickname || auth.isAdmin) && (
              <div className="small muted ellipsis">
                {[auth.me?.nickname ? auth.me.username : '', auth.isAdmin ? t.auth.shell.admin : '']
                  .filter(Boolean)
                  .join(' · ')}
              </div>
            )}
          </div>
          <LangSwitch compact />
          <IconButton
            icon="logout"
            label={t.common.signOut}
            onClick={async () => {
              if (await confirm({ title: t.auth.shell.signOutQuestion, confirm: t.auth.shell.signOutConfirm })) {
                await auth.logout();
              }
            }}
          />
        </div>
      </aside>
      <div className="main">
        <div className="topbar">
          <IconButton icon="menu" label={t.common.menu} onClick={() => setOpen(true)} />
          <BrandLogo height={18} />
        </div>
        <main className="content">{children}</main>
      </div>
    </div>
  );
}
