// The signed-in layout: sidebar navigation (user centre, administration)
// and the page.
import { useState, type ReactNode } from 'react';
import type { Messages } from '../i18n';
import { useT } from '../i18n/react';
import { useAuth } from '../lib/auth';
import { displayName, instanceText } from '../lib/format';
import { Link, useLocation } from '../lib/router';
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
  { to: '/account', label: 'account', icon: 'shield' },
  { to: '/connect', label: 'connect', icon: 'link' },
];

const ADMIN_NAV: NavItem[] = [
  { to: '/admin', label: 'adminHome', icon: 'gauge' },
  { to: '/admin/users', label: 'users', icon: 'user', prefix: true },
  { to: '/admin/invites', label: 'invites', icon: 'ticket' },
  { to: '/admin/devices', label: 'allDevices', icon: 'server' },
  { to: '/admin/sessions', label: 'sessions', icon: 'activity' },
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

/** The application icon: the winged F on the brand blue tile. */
export function BrandMark({ size = 32 }: { size?: number }) {
  return (
    <svg className="brand-mark" width={size} height={size} viewBox="0 0 1024 1024" aria-hidden="true">
      <rect width="1024" height="1024" rx="160" fill="#4B88EB" />
      <path
        transform="translate(180 212) scale(1.7946)"
        fill="#fff"
        d="M 359 2 C 366 1 371 5 368 11 L 338 69 C 331 83 321 91 305 94 L 145 126 C 104 134 79 160 80 214 C 88 198 103 187 124 182 L 286 148 C 294 146 300 148 297 157 L 282 194 C 272 216 258 230 231 236 L 119 258 C 100 262 91 271 84 290 L 78 307 C 73 321 62 329 45 331 L 13 334 C 4 335 0 330 1 321 L 12 148 C 14 90 39 64 70 49 C 87 41 103 37 123 33 Z"
      />
    </svg>
  );
}

export function Shell({ children }: { children: ReactNode }) {
  const t = useT();
  const auth = useAuth();
  const { path } = useLocation();
  const { confirm } = useOverlay();
  const [open, setOpen] = useState(false);
  const name = displayName(auth.me);

  const nav = (items: NavItem[]) =>
    items.map((item) => (
      <Link key={item.to} to={item.to} className={active(item, path) ? 'active' : ''} onClick={() => setOpen(false)}>
        <Icon name={item.icon} size={17} />
        <span>{t.nav[item.label]}</span>
      </Link>
    ));

  return (
    <div className={`shell ${open ? 'nav-open' : ''}`}>
      <aside className="sidebar">
        <div className="brand">
          <BrandMark />
          <div className="grow">
            <div className="brand-name">Farwing</div>
            {instanceText(auth.info?.instance_name) && (
              <div className="brand-sub ellipsis" title={auth.info?.instance_name}>
                {instanceText(auth.info?.instance_name)}
              </div>
            )}
          </div>
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
          <BrandMark size={24} />
          <span className="brand-name">Farwing</span>
        </div>
        <main className="content">{children}</main>
      </div>
    </div>
  );
}
