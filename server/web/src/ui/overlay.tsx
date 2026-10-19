// Dialogs, confirmations and toasts.
import { createContext, useCallback, useContext, useEffect, useRef, useState, type ReactNode } from 'react';
import { Icon } from './icons';
import { Button, IconButton, Input } from './kit';

export function Dialog({
  open,
  title,
  onClose,
  children,
  footer,
  wide,
}: {
  open: boolean;
  title: ReactNode;
  onClose(): void;
  children: ReactNode;
  footer?: ReactNode;
  wide?: boolean;
}) {
  const panel = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const key = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', key);
    const first = panel.current?.querySelector<HTMLElement>('input, select, textarea, button.btn-primary');
    first?.focus();
    return () => window.removeEventListener('keydown', key);
  }, [open, onClose]);
  if (!open) return null;
  return (
    <div className="overlay" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div className={`dialog ${wide ? 'wide' : ''}`} role="dialog" aria-modal="true" ref={panel}>
        <header className="dialog-head">
          <h2>{title}</h2>
          <IconButton icon="close" label="关闭" onClick={onClose} />
        </header>
        <div className="dialog-body">{children}</div>
        {footer && <footer className="dialog-foot">{footer}</footer>}
      </div>
    </div>
  );
}

interface ConfirmOptions {
  title: string;
  message?: ReactNode;
  confirm?: string;
  danger?: boolean;
  /// Ask the user to type this text first (irreversible actions).
  typeToConfirm?: string;
}

interface Toast {
  id: number;
  text: string;
  tone: 'green' | 'red' | 'blue';
}

interface OverlayContextValue {
  confirm(options: ConfirmOptions): Promise<boolean>;
  toast(text: string, tone?: Toast['tone']): void;
}

const OverlayContext = createContext<OverlayContextValue | null>(null);

export function OverlayProvider({ children }: { children: ReactNode }) {
  const [pending, setPending] = useState<(ConfirmOptions & { resolve(v: boolean): void }) | null>(null);
  const [typed, setTyped] = useState('');
  const [toasts, setToasts] = useState<Toast[]>([]);
  const nextToast = useRef(1);

  const confirm = useCallback(
    (options: ConfirmOptions) =>
      new Promise<boolean>((resolve) => {
        setTyped('');
        setPending({ ...options, resolve });
      }),
    [],
  );

  const toast = useCallback((text: string, tone: Toast['tone'] = 'green') => {
    const id = nextToast.current++;
    setToasts((t) => [...t.slice(-3), { id, text, tone }]);
    window.setTimeout(() => setToasts((t) => t.filter((x) => x.id !== id)), tone === 'red' ? 6000 : 3000);
  }, []);

  const close = (value: boolean) => {
    pending?.resolve(value);
    setPending(null);
  };

  const blocked = !!pending?.typeToConfirm && typed.trim() !== pending.typeToConfirm;

  return (
    <OverlayContext.Provider value={{ confirm, toast }}>
      {children}
      <Dialog
        open={!!pending}
        title={pending?.title ?? ''}
        onClose={() => close(false)}
        footer={
          <>
            <Button onClick={() => close(false)}>取消</Button>
            <Button variant={pending?.danger ? 'danger' : 'primary'} disabled={blocked} onClick={() => close(true)}>
              {pending?.confirm ?? '确定'}
            </Button>
          </>
        }
      >
        {pending?.message && <div className="confirm-message">{pending.message}</div>}
        {pending?.typeToConfirm && (
          <div className="stack-sm">
            <span className="muted small">
              输入 <b className="mono">{pending.typeToConfirm}</b> 确认
            </span>
            <Input
              value={typed}
              onChange={(e) => setTyped(e.target.value)}
              onKeyDown={(e) => e.key === 'Enter' && !blocked && close(true)}
            />
          </div>
        )}
      </Dialog>
      <div className="toasts" aria-live="polite">
        {toasts.map((t) => (
          <div key={t.id} className={`toast toast-${t.tone}`}>
            <Icon name={t.tone === 'red' ? 'alert' : t.tone === 'green' ? 'check' : 'info'} size={16} />
            <span>{t.text}</span>
          </div>
        ))}
      </div>
    </OverlayContext.Provider>
  );
}

export function useOverlay(): OverlayContextValue {
  const value = useContext(OverlayContext);
  if (!value) throw new Error('useOverlay outside OverlayProvider');
  return value;
}
