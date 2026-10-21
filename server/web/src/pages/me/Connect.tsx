// How to point a Farwing client at this server, and where to get it.
import { useT } from '../../i18n/react';
import { useAuth } from '../../lib/auth';
import { dateTime, platformText } from '../../lib/format';
import { Alert, Button, Card, Copy, Empty, PageHeader, Row, Rows } from '../../ui/kit';

export default function Connect() {
  const t = useT();
  const m = t.account.connect;
  const { info } = useAuth();
  if (!info) return <Alert>{m.noInfo}</Alert>;
  return (
    <div className="stack">
      <PageHeader title={t.nav.connect} />
      <div className="grid grid-2">
        <Card title={m.server}>
          <Rows>
            <Row label={m.address}>
              <Copy text={info.public_url} />
            </Row>
            <Row label={t.account.name}>{info.instance_name}</Row>
            <Row label={t.account.version}>Farwing Server {info.version}</Row>
            {info.tls_fingerprint && (
              <Row label={m.fingerprint}>
                <Copy text={info.tls_fingerprint} display={<span className="small">{info.tls_fingerprint}</span>} />
              </Row>
            )}
          </Rows>
        </Card>
        <Card title={m.downloads} pad={false}>
          {info.downloads.length === 0 ? (
            <Empty icon="download" title={m.noDownloads} />
          ) : (
            <div className="list">
              {info.downloads.map((d) => (
                <div className="list-item" key={d.id}>
                  <div className="grow">
                    <div>
                      Farwing {platformText(d.platform)} {d.version}
                    </div>
                    <div className="small muted">{m.published(dateTime(d.published_at))}</div>
                  </div>
                  <a href={d.url} target="_blank" rel="noreferrer">
                    <Button variant="primary" icon="download" size="sm">
                      {m.download}
                    </Button>
                  </a>
                </div>
              ))}
            </div>
          )}
        </Card>
      </div>
    </div>
  );
}
