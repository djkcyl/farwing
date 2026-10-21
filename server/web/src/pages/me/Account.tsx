// The account: profile, password, two-step verification (administrators)
// and where it is signed in.
import { useState, type FormEvent } from 'react';
import { encode } from 'uqr';
import { call, errorText } from '../../lib/api';
import { useAuth } from '../../lib/auth';
import { ago, dateTime, platformText } from '../../lib/format';
import type { AuthSession } from '../../lib/types';
import { useApi } from '../../lib/useApi';
import { Alert, Badge, Button, Card, Copy, Empty, Field, Input, Loading, PageHeader, Row, Rows } from '../../ui/kit';
import { Dialog, useOverlay } from '../../ui/overlay';

function QrCode({ text }: { text: string }) {
  const qr = encode(text, { ecc: 'M', border: 2 });
  const path = qr.data.flatMap((row, y) => row.map((on, x) => (on ? `M${x} ${y}h1v1h-1z` : ''))).join('');
  return (
    <svg className="qr" viewBox={`0 0 ${qr.size} ${qr.size}`} shapeRendering="crispEdges" aria-label="二维码">
      <rect width={qr.size} height={qr.size} fill="#fff" />
      <path d={path} fill="#000" />
    </svg>
  );
}

function Profile() {
  const auth = useAuth();
  const { toast } = useOverlay();
  const [nickname, setNickname] = useState(auth.me?.nickname ?? '');
  const [busy, setBusy] = useState(false);
  return (
    <Card title="个人资料">
      <form
        className="stack"
        onSubmit={async (e) => {
          e.preventDefault();
          setBusy(true);
          try {
            await call('PATCH', '/api/v2/me', { nickname: nickname.trim() });
            toast('已保存');
            await auth.refresh();
          } catch (err) {
            toast(errorText(err), 'red');
          } finally {
            setBusy(false);
          }
        }}
      >
        <Rows>
          <Row label="用户名">{auth.me?.username}</Row>
          <Row label="角色">{auth.isAdmin ? <Badge tone="blue">管理员</Badge> : '用户'}</Row>
        </Rows>
        <Field label="昵称">
          <Input value={nickname} onChange={(e) => setNickname(e.target.value)} maxLength={64} />
        </Field>
        <div>
          <Button
            type="submit"
            variant="primary"
            loading={busy}
            disabled={nickname.trim() === (auth.me?.nickname ?? '')}
          >
            保存
          </Button>
        </div>
      </form>
    </Card>
  );
}

function Password() {
  const { toast } = useOverlay();
  const [old, setOld] = useState('');
  const [next, setNext] = useState('');
  const [again, setAgain] = useState('');
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);

  async function submit(e: FormEvent) {
    e.preventDefault();
    if (next !== again) {
      setError('两次输入的新密码不一致');
      return;
    }
    setBusy(true);
    setError(undefined);
    try {
      await call('POST', '/api/v2/me/password', { old_password: old, new_password: next });
      setOld('');
      setNext('');
      setAgain('');
      toast('密码已修改，其他登录已注销');
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Card title="修改密码">
      <form className="stack" onSubmit={submit}>
        {error && <Alert>{error}</Alert>}
        <Field label="当前密码">
          <Input
            type="password"
            autoComplete="current-password"
            value={old}
            onChange={(e) => setOld(e.target.value)}
            required
          />
        </Field>
        <Field label="新密码" hint="至少 8 位">
          <Input
            type="password"
            autoComplete="new-password"
            value={next}
            onChange={(e) => setNext(e.target.value)}
            minLength={8}
            required
          />
        </Field>
        <Field label="确认新密码">
          <Input
            type="password"
            autoComplete="new-password"
            value={again}
            onChange={(e) => setAgain(e.target.value)}
            required
          />
        </Field>
        <div>
          <Button type="submit" variant="primary" loading={busy}>
            修改密码
          </Button>
        </div>
      </form>
    </Card>
  );
}

function TwoStep() {
  const auth = useAuth();
  const { toast } = useOverlay();
  const security = auth.security;
  const [step, setStep] = useState<'idle' | 'password' | 'scan' | 'codes' | 'disable'>('idle');
  const [password, setPassword] = useState('');
  const [code, setCode] = useState('');
  const [setup, setSetup] = useState<{ secret: string; uri: string }>();
  const [codes, setCodes] = useState<string[]>([]);
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);

  if (!security) return null;
  if (!auth.isAdmin && !security.totp) return null;

  const close = () => {
    setStep('idle');
    setPassword('');
    setCode('');
    setError(undefined);
    setSetup(undefined);
  };

  async function run(fn: () => Promise<void>) {
    setBusy(true);
    setError(undefined);
    try {
      await fn();
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Card title="二次验证">
      {security.totp ? (
        <div className="row gap wrap">
          <Badge tone="green">已开启</Badge>
          <span className="small muted">剩余 {security.recovery_codes_left} 个恢复码</span>
          <span className="grow" />
          <Button variant="danger" onClick={() => setStep('disable')}>
            关闭二次验证
          </Button>
        </div>
      ) : (
        <div>
          <Button variant="primary" icon="lock" onClick={() => setStep('password')}>
            开启二次验证
          </Button>
        </div>
      )}

      <Dialog
        open={step === 'password' || step === 'scan'}
        title="开启二次验证"
        onClose={close}
        footer={
          <>
            <Button onClick={close}>取消</Button>
            {step === 'password' ? (
              <Button
                variant="primary"
                loading={busy}
                onClick={() =>
                  run(async () => {
                    setSetup(await call('POST', '/api/v2/me/totp/setup', { password }));
                    setStep('scan');
                  })
                }
              >
                下一步
              </Button>
            ) : (
              <Button
                variant="primary"
                loading={busy}
                disabled={code.length !== 6}
                onClick={() =>
                  run(async () => {
                    const r = await call<{ recovery_codes: string[] }>('POST', '/api/v2/me/totp/enable', { code });
                    setCodes(r.recovery_codes);
                    setStep('codes');
                    setPassword('');
                    setCode('');
                    await auth.refresh();
                  })
                }
              >
                开启
              </Button>
            )}
          </>
        }
      >
        {error && <Alert>{error}</Alert>}
        {step === 'password' ? (
          <Field label="当前密码">
            <Input
              type="password"
              autoComplete="current-password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
            />
          </Field>
        ) : (
          setup && (
            <>
              <div className="row gap wrap" style={{ alignItems: 'flex-start' }}>
                <QrCode text={setup.uri} />
                <div className="stack-sm grow">
                  <span className="small muted">密钥</span>
                  <div className="secret small">
                    <Copy text={setup.secret} />
                  </div>
                </div>
              </div>
              <Field label="验证码">
                <Input
                  className="code"
                  inputMode="numeric"
                  maxLength={6}
                  value={code}
                  onChange={(e) => setCode(e.target.value.replace(/\D/g, ''))}
                  placeholder="000000"
                />
              </Field>
            </>
          )
        )}
      </Dialog>

      <Dialog
        open={step === 'codes'}
        title="保存恢复码"
        onClose={close}
        footer={
          <Button variant="primary" onClick={close}>
            我已保存
          </Button>
        }
      >
        <div className="secret codes">
          {codes.map((c) => (
            <span key={c}>{c}</span>
          ))}
        </div>
        <div>
          <Copy text={codes.join('\n')} display="复制全部" mono={false} />
        </div>
        <Alert tone="amber">只显示这一次</Alert>
      </Dialog>

      <Dialog
        open={step === 'disable'}
        title="关闭二次验证"
        onClose={close}
        footer={
          <>
            <Button onClick={close}>取消</Button>
            <Button
              variant="danger"
              loading={busy}
              onClick={() =>
                run(async () => {
                  await call('POST', '/api/v2/me/totp/disable', { password, code: code.trim() });
                  toast('二次验证已关闭');
                  close();
                  await auth.refresh();
                })
              }
            >
              关闭
            </Button>
          </>
        }
      >
        {error && <Alert>{error}</Alert>}
        <Field label="当前密码">
          <Input
            type="password"
            autoComplete="current-password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
          />
        </Field>
        <Field label="验证码或恢复码">
          <Input value={code} onChange={(e) => setCode(e.target.value)} />
        </Field>
      </Dialog>
    </Card>
  );
}

function Logins() {
  const { confirm, toast } = useOverlay();
  const { data, error, reload } = useApi<{ sessions: AuthSession[] }>('/api/v2/auth/sessions');
  return (
    <Card title="我的登录" pad={false}>
      {error && <Alert>{error}</Alert>}
      {!data ? (
        <Loading />
      ) : data.sessions.length === 0 ? (
        <Empty icon="lock" title="没有登录" />
      ) : (
        <div className="list">
          {[...data.sessions]
            .sort((a, b) => Number(b.current) - Number(a.current) || b.last_used_at - a.last_used_at)
            .map((s) => (
              <div className="list-item" key={s.device_id}>
                <div className="grow">
                  <div className="row gap-sm">
                    <span className="ellipsis">{s.device_alias}</span>
                    {s.current && <Badge tone="blue">这个浏览器</Badge>}
                  </div>
                  <div className="small muted">
                    {platformText(s.platform)} · 登录于 {dateTime(s.created_at)} · {ago(s.last_used_at)}使用
                  </div>
                </div>
                {!s.current && (
                  <Button
                    size="sm"
                    variant="ghost"
                    onClick={async () => {
                      if (
                        await confirm({
                          title: '注销登录',
                          message: `${s.device_alias} 将退出这个账号。`,
                          confirm: '注销',
                          danger: true,
                        })
                      ) {
                        try {
                          await call('DELETE', `/api/v2/auth/sessions/${s.device_id}`);
                          toast('已注销');
                          await reload();
                        } catch (err) {
                          toast(errorText(err), 'red');
                        }
                      }
                    }}
                  >
                    注销
                  </Button>
                )}
              </div>
            ))}
        </div>
      )}
    </Card>
  );
}

export default function Account() {
  return (
    <div className="stack">
      <PageHeader title="账号与安全" />
      <div className="grid grid-2">
        <Profile />
        <Password />
      </div>
      <TwoStep />
      <Logins />
    </div>
  );
}
