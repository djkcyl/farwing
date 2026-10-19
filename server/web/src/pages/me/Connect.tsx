// How to point a Farwing client at this server, and where to get it.
import { useAuth } from '../../lib/auth';
import { dateTime, platformText } from '../../lib/format';
import { Alert, Button, Card, Copy, Empty, PageHeader, Row, Rows } from '../../ui/kit';

export default function Connect() {
  const { info } = useAuth();
  if (!info) return <Alert>无法读取服务器信息</Alert>;
  return (
    <div className="stack">
      <PageHeader title="连接到本服务器" subtitle="在 Farwing 客户端的“服务器”设置中填写下面的地址。" />
      <div className="grid grid-2">
        <Card title="服务器">
          <Rows>
            <Row label="地址">
              <Copy text={info.public_url} />
            </Row>
            <Row label="名称">{info.instance_name}</Row>
            <Row label="版本">Farwing Server {info.version}</Row>
            <Row label="证书指纹（SHA-256）">
              {info.tls_fingerprint ? (
                <Copy text={info.tls_fingerprint} display={<span className="small">{info.tls_fingerprint}</span>} />
              ) : (
                <span className="muted">由反向代理提供 TLS</span>
              )}
            </Row>
          </Rows>
          {info.tls_fingerprint && (
            <p className="small muted" style={{ marginTop: 12 }}>
              服务器使用自签证书时，客户端第一次连接会显示证书指纹，核对与这里一致后再信任。
            </p>
          )}
        </Card>
        <Card title="步骤">
          <ol className="steps">
            <li>下载并安装 Farwing 客户端。</li>
            <li>打开客户端的“设置 → 服务器”，填入上面的地址，核对证书指纹。</li>
            <li>用你的账号登录。登录后，这台电脑会出现在“设备”中，可以被你的其他设备和好友连接。</li>
          </ol>
        </Card>
      </div>
      <Card title="下载客户端" pad={false}>
        {info.downloads.length === 0 ? (
          <Empty icon="download" title="管理员还没有发布客户端下载" hint="请向服务器管理员索取安装包。" />
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
  );
}
