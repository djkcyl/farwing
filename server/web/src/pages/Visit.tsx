// Visitor viewing (`docs/web-console.md` §7.3): a host's short number or
// device ID, with its access code or the host user's confirmation. Works
// without an account (as a guest) and signed in (as the account).
import { useRef, useState, type FormEvent } from 'react';
import { useAuth } from '../lib/auth';
import { connectIdText } from '../lib/format';
import { Link, navigate, useLocation } from '../lib/router';
import { parseTarget, VisitError, visitAdmission, type VisitStep } from '../lib/visit';
import type { Admission } from '../lib/viewer';
import { Alert, Button, Field, Input, Spinner } from '../ui/kit';
import { AuthFrame } from './Login';
import { ViewerScreen } from '../ui/ViewerScreen';

const STEPS: Record<VisitStep, string> = {
  asking: '正在联系对方',
  checking: '正在验证访问码',
  confirming: '等待对方在设备上允许本次连接',
  joining: '正在建立会话',
};

export default function Visit() {
  const auth = useAuth();
  const { query } = useLocation();
  const [target, setTarget] = useState(() => {
    const code = query.get('code') ?? query.get('id') ?? '';
    return /^\d{9}$/.test(code) ? connectIdText(code) : code;
  });
  const [mode, setMode] = useState<'code' | 'confirm'>('code');
  const [code, setCode] = useState('');
  const [step, setStep] = useState<VisitStep>();
  const [error, setError] = useState<string>();
  const [watching, setWatching] = useState(false);
  const first = useRef<Admission | undefined>(undefined);
  const [confirmed, setConfirmed] = useState(false);
  const secret = useRef('');

  async function submit(e: FormEvent) {
    e.preventDefault();
    setError(undefined);
    if (!parseTarget(target)) {
      setError('请输入 9 位短号或 16 位设备 ID');
      return;
    }
    secret.current = mode === 'code' ? code.trim() : '';
    setConfirmed(false);
    try {
      first.current = await visitAdmission(target, secret.current, (s) => {
        if (s === 'confirming') setConfirmed(true);
        setStep(s);
      });
      setWatching(true);
    } catch (err) {
      setError(err instanceof VisitError ? err.message : String(err));
    } finally {
      setStep(undefined);
    }
  }

  if (watching) {
    return (
      <ViewerScreen
        title={parseTarget(target)?.connect_id ? connectIdText(parseTarget(target)!.connect_id) : target}
        subtitle="访客观看 · 只看不控"
        // A session needing the host user's confirmation is not re-asked
        // automatically.
        autoRetry={!confirmed}
        admit={async () => {
          const ready = first.current;
          first.current = undefined;
          return ready ?? visitAdmission(target, secret.current, () => undefined);
        }}
        onLeave={() => {
          setWatching(false);
          if (auth.status !== 'signed_in') setCode('');
        }}
      />
    );
  }

  const busy = !!step;
  return (
    <AuthFrame
      title="访客观看"
      subtitle="输入对方设备的短号或设备 ID。网页端只看不控。"
      foot={
        auth.status === 'signed_in' ? (
          <Link to="/">返回用户中心</Link>
        ) : (
          <>
            <Link to="/login">用账号登录</Link>
            <span />
          </>
        )
      }
    >
      <form className="stack" onSubmit={submit}>
        {error && <Alert>{error}</Alert>}
        {auth.info && !auth.info.guests && auth.status !== 'signed_in' && (
          <Alert tone="amber">本服务器没有开启游客功能，请先登录账号。</Alert>
        )}
        <Field label="短号或设备 ID">
          <Input
            className="mono"
            value={target}
            onChange={(e) => setTarget(e.target.value)}
            placeholder="123 456 789"
            autoFocus={!target}
            disabled={busy}
            required
          />
        </Field>
        <div className="row gap-sm">
          <button type="button" className={`chip ${mode === 'code' ? 'on' : ''}`} onClick={() => setMode('code')}>
            使用访问码
          </button>
          <button type="button" className={`chip ${mode === 'confirm' ? 'on' : ''}`} onClick={() => setMode('confirm')}>
            请对方确认
          </button>
        </div>
        {mode === 'code' ? (
          <Field label="访问码" hint="访问码只在这个浏览器中使用，不会发送给服务器">
            <Input
              type="password"
              autoComplete="off"
              value={code}
              onChange={(e) => setCode(e.target.value)}
              autoFocus={!!target}
              disabled={busy}
              required
            />
          </Field>
        ) : (
          <p className="small muted">对方设备会弹出请求，由对方允许后开始观看。对方信任过这个浏览器时直接连接。</p>
        )}
        {busy && (
          <div className="row gap-sm muted">
            <Spinner size={16} /> {STEPS[step!]}
          </div>
        )}
        <Button type="submit" variant="primary" className="btn-block" loading={busy}>
          开始观看
        </Button>
        {busy && step === 'confirming' && (
          <Button variant="ghost" onClick={() => navigate('/visit', true)}>
            取消
          </Button>
        )}
      </form>
    </AuthFrame>
  );
}
