// The console's own components, styled to match the Farwing client
// (`styles.css`).
import {
  useId,
  useState,
  type ButtonHTMLAttributes,
  type InputHTMLAttributes,
  type ReactNode,
  type SelectHTMLAttributes,
} from 'react';
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

export function Select({ className = '', children, ...rest }: SelectHTMLAttributes<HTMLSelectElement>) {
  const id = useId();
  return (
    <select {...rest} id={rest.id ?? id} className={`input select ${className}`}>
      {children}
    </select>
  );
}

export function Field({
  label,
  hint,
  error,
  children,
}: {
  label: ReactNode;
  hint?: ReactNode;
  error?: ReactNode;
  children: ReactNode;
}) {
  return (
    <label className="field">
      <span className="field-label">{label}</span>
      {children}
      {error ? <span className="field-error">{error}</span> : hint ? <span className="field-hint">{hint}</span> : null}
    </label>
  );
}

export function Switch({
  checked,
  onChange,
  label,
  hint,
  disabled,
}: {
  checked: boolean;
  onChange(value: boolean): void;
  label?: ReactNode;
  hint?: ReactNode;
  disabled?: boolean;
}) {
  return (
    <label className={`switch-row ${disabled ? 'disabled' : ''}`}>
      <span className="switch-text">
        {label && <span>{label}</span>}
        {hint && <span className="muted small">{hint}</span>}
      </span>
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
  actions,
  children,
  className = '',
  pad = true,
}: {
  title?: ReactNode;
  actions?: ReactNode;
  children: ReactNode;
  className?: string;
  pad?: boolean;
}) {
  return (
    <section className={`card ${className}`}>
      {(title || actions) && (
        <header className="card-head">
          <h2>{title}</h2>
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
  return (
    <span className={`status ${online ? 'online' : 'offline'}`}>
      <span className="dot" />
      {text ?? (online ? '在线' : '离线')}
    </span>
  );
}

export function Spinner({ size = 18 }: { size?: number }) {
  return <span className="spinner" style={{ width: size, height: size }} />;
}

export function Loading({ text = '正在加载' }: { text?: string }) {
  return (
    <div className="loading">
      <Spinner /> <span>{text}</span>
    </div>
  );
}

export function Empty({
  icon = 'info',
  title,
  hint,
  action,
}: {
  icon?: IconName;
  title: ReactNode;
  hint?: ReactNode;
  action?: ReactNode;
}) {
  return (
    <div className="empty">
      <Icon name={icon} size={28} />
      <div className="empty-title">{title}</div>
      {hint && <div className="muted">{hint}</div>}
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
  const [done, setDone] = useState(false);
  return (
    <span className={`copy ${mono ? 'mono' : ''}`}>
      <span className="copy-text">{display ?? text}</span>
      <button
        type="button"
        className="copy-btn"
        title="复制"
        aria-label="复制"
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
  placeholder = '搜索',
}: {
  value: string;
  onChange(v: string): void;
  placeholder?: string;
}) {
  return (
    <span className="search">
      <Icon name="search" size={15} />
      <input
        type="search"
        name="search"
        value={value}
        onChange={(e) => onChange(e.target.value)}
        placeholder={placeholder}
      />
    </span>
  );
}
