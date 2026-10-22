// The console's own components, styled to match the Farwing client
// (`styles.css`).
import {
  useCallback,
  useEffect,
  useId,
  useRef,
  useState,
  type ButtonHTMLAttributes,
  type CSSProperties,
  type InputHTMLAttributes,
  type ReactNode,
} from 'react';
import { useT } from '../i18n/react';
import { useDismiss } from './dismiss';
import { Icon, type IconName } from './icons';

type Variant = 'primary' | 'default' | 'ghost' | 'danger';

export function Button({
  variant = 'default',
  size = 'md',
  icon,
  loading,
  children,
  className = '',
  ...rest
}: ButtonHTMLAttributes<HTMLButtonElement> & {
  variant?: Variant;
  size?: 'sm' | 'md';
  icon?: IconName;
  loading?: boolean;
}) {
  return (
    <button
      type="button"
      {...rest}
      disabled={rest.disabled || loading}
      className={`btn btn-${variant} btn-${size} ${className}`}
    >
      {loading ? <Spinner size={14} /> : icon ? <Icon name={icon} size={size === 'sm' ? 14 : 16} /> : null}
      {children !== undefined && <span>{children}</span>}
    </button>
  );
}

export function IconButton({
  icon,
  label,
  variant = 'ghost',
  className = '',
  ...rest
}: ButtonHTMLAttributes<HTMLButtonElement> & { icon: IconName; label: string; variant?: Variant }) {
  return (
    <button type="button" title={label} aria-label={label} {...rest} className={`icon-btn btn-${variant} ${className}`}>
      <Icon name={icon} size={16} />
    </button>
  );
}

// Every field gets an id (browsers use it for autofill and accessibility).
export function Input({ className = '', ...rest }: InputHTMLAttributes<HTMLInputElement>) {
  const id = useId();
  return <input {...rest} id={rest.id ?? id} className={`input ${className}`} />;
}

export interface Option<T> {
  value: T;
  label: ReactNode;
}

/// One of several values, from the console's own menu.
export function Select<T extends string | number>({
  value,
  options,
  onChange,
  width,
  disabled,
}: {
  value: T;
  options: Option<NoInfer<T>>[];
  onChange(value: NoInfer<T>): void;
  width?: number;
  disabled?: boolean;
}) {
  const id = useId();
  const [open, setOpen] = useState(false);
  // Fixed to the window: a dialog's scrolling body would clip it.
  const [place, setPlace] = useState<CSSProperties>();
  const close = useCallback(() => setOpen(false), []);
  const ref = useDismiss(open, close);
  const button = useRef<HTMLButtonElement>(null);
  const current = options.find((o) => o.value === value);
  useEffect(() => {
    if (!open) return;
    const away = (e: Event) => {
      if (!ref.current?.contains(e.target as Node)) setOpen(false);
    };
    window.addEventListener('scroll', away, true);
    window.addEventListener('resize', close);
    return () => {
      window.removeEventListener('scroll', away, true);
      window.removeEventListener('resize', close);
    };
  }, [open, close, ref]);
  function toggle() {
    const r = button.current?.getBoundingClientRect();
    if (open || !r) {
      setOpen(false);
      return;
    }
    const below = window.innerHeight - r.bottom;
    const up = below < 240 && r.top > below;
    setPlace({
      position: 'fixed',
      left: r.left,
      right: 'auto',
      minWidth: r.width,
      top: up ? 'auto' : r.bottom + 6,
      bottom: up ? window.innerHeight - r.top + 6 : 'auto',
      maxHeight: (up ? r.top : below) - 16,
    });
    setOpen(true);
  }
  return (
    <div className="select-wrap" ref={ref} style={width ? { width } : undefined}>
      <button
        type="button"
        id={id}
        ref={button}
        className="input select select-button"
        aria-haspopup="listbox"
        aria-expanded={open}
        disabled={disabled}
        onClick={toggle}
      >
        <span className="ellipsis">{current?.label}</span>
      </button>
      {open && (
        <div className="menu select-menu" role="listbox" style={place}>
          {options.map((o) => (
            <button
              key={String(o.value)}
              type="button"
              role="option"
              aria-selected={o.value === value}
              className={`menu-item ${o.value === value ? 'selected' : ''}`}
              onClick={() => {
                onChange(o.value);
                setOpen(false);
              }}
            >
              <span className="menu-check">{o.value === value && <Icon name="check" size={14} />}</span>
              <span className="grow">{o.label}</span>
            </button>
          ))}
        </div>
      )}
    </div>
  );
}

/// A few values side by side, one of them chosen.
export function Segmented<T extends string>({
  value,
  options,
  onChange,
  disabled,
}: {
  value: T;
  options: Option<NoInfer<T>>[];
  onChange(value: NoInfer<T>): void;
  disabled?: boolean;
}) {
  return (
    <div className="segmented" role="radiogroup">
      {options.map((o) => (
        <button
          key={o.value}
          type="button"
          role="radio"
          aria-checked={o.value === value}
          disabled={disabled}
          className={o.value === value ? 'active' : ''}
          onClick={() => onChange(o.value)}
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}

/// A labelled control. `group`: several controls (or buttons) under one
/// label, which then labels none of them.
export function Field({
  label,
  hint,
  error,
  group,
  children,
}: {
  label: ReactNode;
  hint?: ReactNode;
  error?: ReactNode;
  group?: boolean;
  children: ReactNode;
}) {
  const id = useId();
  const content = (
    <>
      <span className="field-label" id={group ? id : undefined}>
        {label}
      </span>
      {children}
      {error ? <span className="field-error">{error}</span> : hint ? <span className="field-hint">{hint}</span> : null}
    </>
  );
  return group ? (
    <div className="field" role="group" aria-labelledby={id}>
      {content}
    </div>
  ) : (
    <label className="field">{content}</label>
  );
}

/// A list edited as text, one item per line.
export function LinesInput({
  value,
  onChange,
  rows = 3,
  placeholder,
}: {
  value: string[];
  onChange(value: string[]): void;
  rows?: number;
  placeholder?: string;
}) {
  const id = useId();
  const [text, setText] = useState(value.join('\n'));
  return (
    <textarea
      id={id}
      className="input mono-input"
      rows={Math.max(rows, text.split('\n').length)}
      value={text}
      placeholder={placeholder}
      spellCheck={false}
      onChange={(e) => {
        setText(e.target.value);
        onChange(
          e.target.value
            .split('\n')
            .map((line) => line.trim())
            .filter(Boolean),
        );
      }}
    />
  );
}

/// A button choosing a text file; its name is shown once chosen.
export function FilePick({
  label,
  accept,
  onText,
}: {
  label: string;
  accept?: string;
  onText(text: string, name: string): void;
}) {
  const [name, setName] = useState('');
  return (
    <span className="row gap-sm">
      <label className="btn btn-default btn-md file-pick">
        <input
          type="file"
          accept={accept}
          onChange={async (e) => {
            const file = e.target.files?.[0];
            e.target.value = '';
            if (!file) return;
            setName(file.name);
            onText(await file.text(), file.name);
          }}
        />
        <Icon name="upload" size={16} />
        <span>{label}</span>
      </label>
      {name && <span className="small muted ellipsis">{name}</span>}
    </span>
  );
}

export function Switch({
  checked,
  onChange,
  label,
  disabled,
}: {
  checked: boolean;
  onChange(value: boolean): void;
  label?: ReactNode;
  disabled?: boolean;
}) {
  return (
    <label className={`switch-row ${disabled ? 'disabled' : ''}`}>
      <span className="switch-text">{label && <span>{label}</span>}</span>
      <button
        type="button"
        role="switch"
        aria-checked={checked}
        disabled={disabled}
        className={`switch ${checked ? 'on' : ''}`}
        onClick={() => onChange(!checked)}
      >
        <span />
      </button>
    </label>
  );
}

export function Checkbox({
  checked,
  onChange,
  label,
  disabled,
}: {
  checked: boolean;
  onChange(v: boolean): void;
  label: ReactNode;
  disabled?: boolean;
}) {
  const id = useId();
  return (
    <label className={`checkbox ${disabled ? 'disabled' : ''}`}>
      <input
        id={id}
        type="checkbox"
        checked={checked}
        disabled={disabled}
        onChange={(e) => onChange(e.target.checked)}
      />
      <span className="box">{checked && <Icon name="check" size={12} />}</span>
      <span>{label}</span>
    </label>
  );
}

export function Card({
  title,
  count,
  actions,
  children,
  className = '',
  pad = true,
}: {
  title?: ReactNode;
  /// How many items the card lists, after the title.
  count?: number;
  actions?: ReactNode;
  children: ReactNode;
  className?: string;
  pad?: boolean;
}) {
  return (
    <section className={`card ${className}`}>
      {(title || actions) && (
        <header className="card-head">
          <h2>
            {title}
            {count !== undefined && <span className="card-count">{count}</span>}
          </h2>
          {actions && <div className="row gap-sm">{actions}</div>}
        </header>
      )}
      <div className={pad ? 'card-body' : ''}>{children}</div>
    </section>
  );
}

export type Tone = 'green' | 'amber' | 'red' | 'blue' | 'muted';

export function Badge({ tone = 'muted', children }: { tone?: Tone; children: ReactNode }) {
  return <span className={`badge badge-${tone}`}>{children}</span>;
}

export function Online({ online, text }: { online: boolean; text?: string }) {
  const t = useT();
  return (
    <span className={`status ${online ? 'online' : 'offline'}`}>
      <span className="dot" />
      {text ?? (online ? t.common.online : t.common.offline)}
    </span>
  );
}

export function Spinner({ size = 18 }: { size?: number }) {
  return <span className="spinner" style={{ width: size, height: size }} />;
}

export function Loading({ text }: { text?: string }) {
  const t = useT();
  return (
    <div className="loading">
      <Spinner /> <span>{text ?? t.common.loading}</span>
    </div>
  );
}

export function Empty({ icon = 'info', title, action }: { icon?: IconName; title: ReactNode; action?: ReactNode }) {
  return (
    <div className="empty">
      <Icon name={icon} size={28} />
      <div className="empty-title">{title}</div>
      {action}
    </div>
  );
}

export function Alert({ tone = 'red', children, icon }: { tone?: Tone; children: ReactNode; icon?: IconName }) {
  return (
    <div className={`alert alert-${tone}`}>
      <Icon name={icon ?? (tone === 'red' || tone === 'amber' ? 'alert' : 'info')} size={16} />
      <div>{children}</div>
    </div>
  );
}

export function PageHeader({
  title,
  subtitle,
  actions,
  back,
}: {
  title: ReactNode;
  subtitle?: ReactNode;
  actions?: ReactNode;
  back?: ReactNode;
}) {
  return (
    <header className="page-head">
      <div>
        {back}
        <h1>{title}</h1>
        {subtitle && <p className="muted">{subtitle}</p>}
      </div>
      {actions && <div className="row gap-sm wrap">{actions}</div>}
    </header>
  );
}

/// A page whose data is not there yet: its title, then loading or why
/// it could not be read, where the content would be.
export function PageState({ title, back, error }: { title: ReactNode; back?: ReactNode; error?: string }) {
  return (
    <div className="stack">
      <PageHeader title={title} back={back} />
      {error ? (
        <Card>
          <Empty icon="alert" title={error} />
        </Card>
      ) : (
        <Loading />
      )}
    </div>
  );
}

export function Tabs<T extends string>({
  items,
  value,
  onChange,
}: {
  items: { id: T; label: ReactNode }[];
  value: T;
  onChange(id: T): void;
}) {
  return (
    <div className="tabs" role="tablist">
      {items.map((item) => (
        <button
          key={item.id}
          type="button"
          role="tab"
          aria-selected={item.id === value}
          className={item.id === value ? 'active' : ''}
          onClick={() => onChange(item.id)}
        >
          {item.label}
        </button>
      ))}
    </div>
  );
}

export function Stat({
  label,
  value,
  hint,
  tone,
}: {
  label: ReactNode;
  value: ReactNode;
  hint?: ReactNode;
  tone?: Tone;
}) {
  return (
    <div className={`stat ${tone ? `stat-${tone}` : ''}`}>
      <div className="stat-label">{label}</div>
      <div className="stat-value">{value}</div>
      {hint && <div className="stat-hint">{hint}</div>}
    </div>
  );
}

/// Text with a copy button.
export function Copy({ text, display, mono = true }: { text: string; display?: ReactNode; mono?: boolean }) {
  const t = useT();
  const [done, setDone] = useState(false);
  return (
    <span className={`copy ${mono ? 'mono' : ''}`}>
      <span className="copy-text">{display ?? text}</span>
      <button
        type="button"
        className="copy-btn"
        title={t.common.copy}
        aria-label={t.common.copy}
        onClick={async () => {
          await navigator.clipboard?.writeText(text).catch(() => undefined);
          setDone(true);
          window.setTimeout(() => setDone(false), 1200);
        }}
      >
        <Icon name={done ? 'check' : 'copy'} size={13} />
      </button>
    </span>
  );
}

/// A definition list of `Row`s.
export function Rows({ children }: { children: ReactNode }) {
  return <dl className="rows">{children}</dl>;
}

export function Row({ label, children }: { label: ReactNode; children: ReactNode }) {
  return (
    <div className="rows-item">
      <dt>{label}</dt>
      <dd>{children}</dd>
    </div>
  );
}

export function SearchBox({
  value,
  onChange,
  placeholder,
}: {
  value: string;
  onChange(v: string): void;
  placeholder?: string;
}) {
  const t = useT();
  return (
    <span className="search">
      <Icon name="search" size={15} />
      <input
        type="search"
        name="search"
        value={value}
        onChange={(e) => onChange(e.target.value)}
        placeholder={placeholder ?? t.common.search}
      />
    </span>
  );
}
