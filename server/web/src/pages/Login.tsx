// Sign in (password, then the second step for accounts with two-step
// verification) and invitation registration.
import { useState, type FormEvent, type ReactNode } from 'react';
import { ApiError, errorText } from '../lib/api';
import { useAuth } from '../lib/auth';
import { Link, navigate, useLocation } from '../lib/router';
import { Alert, Button, Field, Input } from '../ui/kit';
import { BrandMark } from '../ui/Shell';

export function AuthFrame({
  title,
  subtitle,
  children,
  foot,
}: {
  title: string;
  subtitle?: ReactNode;
  children: ReactNode;
  foot?: ReactNode;
}) {
  const { info } = useAuth();
  return (
    <div className="auth-page">
      <div className="auth-card">
        <div className="auth-brand">
          <BrandMark size={22} />
          <div>
            <div className="brand-name">Farwing</div>
            <div className="small muted">{info?.instance_name || 'Farwing Server'}</div>
          </div>
        </div>
        <div className="auth-panel">
          <div className="stack-sm">
            <h1>{title}</h1>
            {subtitle && <p className="muted">{subtitle}</p>}
          </div>
          {children}
        </div>
        {foot && <div className="auth-foot">{foot}</div>}
      </div>
    </div>
  );
}

export function LoginPage() {
  const auth = useAuth();
  const [username, setUsername] = useState('');
  const [password, setPassword] = useState('');
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);

  async function submit(e: FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(undefined);
    try {
      await auth.login(username.trim(), password);
    } catch (err) {
      setError(
        err instanceof ApiError && err.code === 'unauthorized' ? '用户名或密码不正确，或账号已停用' : errorText(err),
      );
    } finally {
      setBusy(false);
    }
  }

  return (
    <AuthFrame
      title="登录"
      subtitle="使用 Farwing 账号登录网页端。"
      foot={
        <>
          {auth.info?.registration ? <Link to="/register">用邀请码注册</Link> : <span />}
          {auth.info?.guests && <Link to="/visit">用短号与访问码观看</Link>}
        </>
      }
    >
      <form className="stack" onSubmit={submit}>
        {auth.notice && !error && <Alert tone="amber">{auth.notice}</Alert>}
        {error && <Alert>{error}</Alert>}
        <Field label="用户名">
          <Input
            id="username"
            autoComplete="username"
            value={username}
            onChange={(e) => setUsername(e.target.value)}
            autoFocus
            required
          />
        </Field>
        <Field label="密码">
          <Input
            id="password"
            type="password"
            autoComplete="current-password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            required
          />
        </Field>
        <Button id="login" type="submit" variant="primary" className="btn-block" loading={busy}>
          登录
        </Button>
      </form>
    </AuthFrame>
  );
}

export function SecondFactorPage() {
  const auth = useAuth();
  const [code, setCode] = useState('');
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);
  const [recovery, setRecovery] = useState(false);

  async function submit(e: FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(undefined);
    try {
      await auth.verify(code.replace(/\s/g, ''));
    } catch (err) {
      setError(errorText(err));
      setCode('');
    } finally {
      setBusy(false);
    }
  }

  return (
    <AuthFrame
      title="二次验证"
      subtitle={recovery ? '输入一个未用过的恢复码，每个恢复码只能用一次。' : '打开身份验证器，输入 6 位验证码。'}
      foot={
        <>
          <a
            href="#"
            onClick={(e) => {
              e.preventDefault();
              setRecovery(!recovery);
              setCode('');
              setError(undefined);
            }}
          >
            {recovery ? '使用验证码' : '使用恢复码'}
          </a>
          <a
            href="#"
            onClick={(e) => {
              e.preventDefault();
              void auth.logout();
            }}
          >
            换个账号
          </a>
        </>
      }
    >
      <form className="stack" onSubmit={submit}>
        {error && <Alert>{error}</Alert>}
        <Input
          id="totp"
          className={recovery ? 'mono' : 'code'}
          inputMode={recovery ? 'text' : 'numeric'}
          autoComplete="one-time-code"
          maxLength={recovery ? 32 : 6}
          placeholder={recovery ? 'xxxxx-xxxxx' : '000000'}
          value={code}
          onChange={(e) => setCode(recovery ? e.target.value : e.target.value.replace(/\D/g, ''))}
          autoFocus
          required
        />
        <Button
          id="verify"
          type="submit"
          variant="primary"
          className="btn-block"
          loading={busy}
          disabled={!recovery && code.length !== 6}
        >
          验证
        </Button>
      </form>
    </AuthFrame>
  );
}

export function RegisterPage() {
  const auth = useAuth();
  const { query } = useLocation();
  const [invite, setInvite] = useState(query.get('code') ?? '');
  const [username, setUsername] = useState('');
  const [nickname, setNickname] = useState('');
  const [password, setPassword] = useState('');
  const [password2, setPassword2] = useState('');
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);

  if (auth.info && !auth.info.registration) {
    return (
      <AuthFrame title="注册" foot={<Link to="/login">返回登录</Link>}>
        <Alert tone="amber">本服务器没有开放注册，请联系管理员创建账号。</Alert>
      </AuthFrame>
    );
  }

  async function submit(e: FormEvent) {
    e.preventDefault();
    if (password !== password2) {
      setError('两次输入的密码不一致');
      return;
    }
    setBusy(true);
    setError(undefined);
    try {
      await auth.register(invite.trim(), username.trim(), password, nickname.trim());
      navigate('/', true);
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <AuthFrame
      title="用邀请码注册"
      subtitle="邀请码由服务器管理员提供。"
      foot={<Link to="/login">已有账号？登录</Link>}
    >
      <form className="stack" onSubmit={submit}>
        {error && <Alert>{error}</Alert>}
        <Field label="邀请码">
          <Input
            className="mono"
            value={invite}
            onChange={(e) => setInvite(e.target.value.toUpperCase())}
            placeholder="XXXX-XXXX-XXXX"
            required
            autoFocus={!invite}
          />
        </Field>
        <Field label="用户名" hint="1–64 位字母、数字或 . _ - @，登录时使用">
          <Input
            autoComplete="username"
            value={username}
            onChange={(e) => setUsername(e.target.value)}
            required
            autoFocus={!!invite}
          />
        </Field>
        <Field label="昵称" hint="可选，好友看到的名字">
          <Input value={nickname} onChange={(e) => setNickname(e.target.value)} maxLength={64} />
        </Field>
        <Field label="密码" hint="至少 8 位">
          <Input
            type="password"
            autoComplete="new-password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            minLength={8}
            required
          />
        </Field>
        <Field label="确认密码">
          <Input
            type="password"
            autoComplete="new-password"
            value={password2}
            onChange={(e) => setPassword2(e.target.value)}
            required
          />
        </Field>
        <Button type="submit" variant="primary" className="btn-block" loading={busy}>
          注册并登录
        </Button>
      </form>
    </AuthFrame>
  );
}
