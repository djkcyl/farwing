// How to point a Farwing client at this server, and where to get it.
import { useAuth } from '../../lib/auth';
import { dateTime, platformText } from '../../lib/format';
import { Alert, Button, Card, Copy, Empty, PageHeader, Row, Rows } from '../../ui/kit';

export default function Connect() {
  const { info } = useAuth();
  if (!info) return <Alert>无法读取服务器信息</Alert>;
  return (
    <div className="stack">
      <PageHeader title="连接到本服务器" />
      <div className="grid grid-2">
        <Card title="服务器">
          <Rows>
            <Row label="地址">
              <Copy text={info.public_url} />
            </Row>
            <Row label="名称">{info.instance_name}</Row>
            <Row label="版本">Farwing Server {info.version}</Row>
            {info.tls_fingerprint && (
              <Row label="证书指纹（SHA-256）">
                <Copy text={info.tls_fingerprint} display={<span className="small">{info.tls_fingerprint}</span>} />
              </Row>
            )}
          </Rows>
        </Card>
        <Card title="下载客户端" pad={false}>
          {info.downloads.length === 0 ? (
            <Empty icon="download" title="暂无客户端下载" />
          ) : (
            <div className="list">
              {info.downloads.map((d) => (
                <div className="list-item" key={d.id}>
                  <div className="grow">
                    <div>
                      Farwing {platformText(d.platform)} {d.version}
                    </div>
                    <div className="small muted">发布于 {dateTime(d.published_at)}</div>
                  </div>
                  <a href={d.url} target="_blank" rel="noreferrer">
                    <Button variant="primary" icon="download" size="sm">
                      下载
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
