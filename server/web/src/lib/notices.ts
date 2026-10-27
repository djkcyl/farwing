// Notifications (`docs/notifications.md`): the unread count the sidebar
// shows, and a notification in words.
import { useEffect, useState } from 'react';
import { t } from '../i18n';
import { call } from './api';
import type { Notice, NoticeList } from './types';

const POLL_MS = 30_000;
const changes = new EventTarget();

/// The list was read: counts refresh at once.
export function noticesChanged() {
  changes.dispatchEvent(new Event('changed'));
}

/// Unread notifications, refreshed every 30 seconds, when the page comes
/// back, and when they are read.
export function useUnreadNotices(enabled: boolean): number {
  const [count, setCount] = useState(0);
  useEffect(() => {
    if (!enabled) return;
    let stopped = false;
    const load = () =>
      call<NoticeList>('GET', '/api/v2/me/notices?limit=0', undefined, { quiet: true })
        .then((v) => !stopped && setCount(v.unread))
        .catch(() => undefined);
    void load();
    const timer = window.setInterval(() => document.visibilityState === 'visible' && void load(), POLL_MS);
    const visible = () => document.visibilityState === 'visible' && void load();
    document.addEventListener('visibilitychange', visible);
    changes.addEventListener('changed', load);
    return () => {
      stopped = true;
      window.clearInterval(timer);
      document.removeEventListener('visibilitychange', visible);
      changes.removeEventListener('changed', load);
    };
  }, [enabled]);
  return enabled ? count : 0;
}

/// Title and line of a notification in the current language.
export function noticeText(n: Notice): { title: string; body: string } {
  const text = t().notices.text[n.kind];
  return text ? text((n.data ?? {}) as Record<string, string | number | undefined>) : { title: n.kind, body: '' };
}

/// The settings' groups, in order.
export const NOTICE_GROUPS: [keyof ReturnType<typeof t>['notices']['groups'], string[]][] = [
  ['security', ['login.new_device', 'login.failures', 'security.changed', 'device.removed']],
  ['connections', ['connection.incoming', 'connection.refused', 'connection.own']],
  ['friends', ['friend.request', 'friend.accepted', 'friend.grants']],
  ['devices', ['power.remote', 'device.offline', 'access.changed']],
  ['admin', ['admin.certificate', 'admin.node_offline', 'admin.registered']],
];
