// Why the server refused a settings document, in words: the setting and its
// problem when the server names them (`details.field`, `details.problem`).
import type { Messages } from '../i18n';
import { ApiError, errorText } from './api';

type M = Messages['admin']['settings'];

/// The label of a setting the server refused (`details.field`).
export function fieldLabel(m: M, field: string): string {
  const labels: Record<string, string> = {
    listen: m.listen,
    public_url: m.publicUrl,
    instance_name: m.instanceName,
    trusted_proxies: m.trustedProxies,
    'egress.limit_mbps': m.egressLimit,
    'egress.users.rate_mbps': `${m.egressUsers} · ${m.egressRate}`,
    'egress.users.quota_gb': `${m.egressUsers} · ${m.egressQuota}`,
    'egress.guests.rate_mbps': `${m.egressGuests} · ${m.egressRate}`,
    'egress.guests.quota_gb': `${m.egressGuests} · ${m.egressQuota}`,
    log: m.log,
    'tls.hostnames': m.hostnames,
    'tls.certificate': m.certPem,
    'tls.acme.domains': m.acmeDomains,
    'tls.acme.contact': m.acmeContact,
    'tls.acme.directory': m.acmeDirectory,
    'tls.acme.challenge': m.challenge,
    'tls.acme.http_listen': m.httpListen,
    'tls.acme.agree_tos': m.agreeTos,
    'turn.urls': m.turnUrls,
    'turn.shared_secret': m.sharedSecret,
    'turn.public_ip': m.publicIp,
    'turn.listen_udp': m.listenUdp,
    'turn.listen_tcp': m.listenTcp,
    'turn.listen_tls': m.listenTls,
    'turn.relay_port_min': m.relayPorts,
    'turn.relay_port_max': m.relayPorts,
    'turn.relay_bind': m.relayBind,
    'turn.relay_ip': m.relayIp,
    'turn.credential_ttl': m.credentialTtl,
    'turn.stun': m.stun,
    'sfu.threshold': m.sfuThreshold,
    'sfu.port': m.sfuPort,
    'web.caps': m.webCaps,
    'web.admin_allow': m.adminAllow,
    'web.session_idle_minutes': m.sessionIdle,
    'web.session_max_hours': m.sessionMax,
    'web.audit_days': m.auditDays,
  };
  const limit = field.startsWith('limits.') ? (m.limits as Record<string, string>)[field.slice(7)] : undefined;
  return labels[field] ?? limit ?? field;
}

/// Why saving failed: the setting and its problem when the server names them.
export function settingErrorText(m: M, error: unknown): string {
  if (error instanceof ApiError) {
    const d = error.details as { reason?: string; field?: string; problem?: string } | undefined;
    if (d?.reason === 'setting_invalid' && d.field && d.field !== 'settings') {
      const problems: Record<string, string> = m.problems;
      return m.fieldError(fieldLabel(m, d.field), problems[d.problem ?? ''] ?? problems.invalid);
    }
  }
  return errorText(error);
}
