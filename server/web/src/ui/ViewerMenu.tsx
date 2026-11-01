// The viewer's tool menus: a button that opens a list of items.
import { useEffect, useRef, useState, type ReactNode } from 'react';
import { Icon, type IconName } from './icons';

export function Menu({
  label,
  icon,
  text,
  active,
  children,
}: {
  label: string;
  icon: IconName;
  text?: string;
  active?: boolean;
  children: ReactNode;
}) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const close = (e: MouseEvent) => {
      if (!ref.current?.contains(e.target as Node)) setOpen(false);
    };
    window.addEventListener('mousedown', close);
    return () => window.removeEventListener('mousedown', close);
  }, [open]);
  return (
    <div className="tool-menu" ref={ref}>
      <button
        type="button"
        className={`tool ${open || active ? 'active' : ''}`}
        title={label}
        aria-label={label}
        onClick={() => setOpen(!open)}
      >
        <Icon name={icon} size={18} />
        {text && <span className="tool-text">{text}</span>}
      </button>
      {open && (
        <div className="menu" onClick={() => setOpen(false)}>
          {children}
        </div>
      )}
    </div>
  );
}

export function MenuItem({
  selected,
  onClick,
  children,
  hint,
  disabled,
  title,
}: {
  selected?: boolean;
  onClick(): void;
  children: ReactNode;
  hint?: ReactNode;
  disabled?: boolean;
  title?: string;
}) {
  return (
    <button
      type="button"
      className={`menu-item ${selected ? 'selected' : ''}`}
      onClick={onClick}
      disabled={disabled}
      title={title}
    >
      <span className="menu-check">{selected && <Icon name="check" size={14} />}</span>
      <span className="grow">{children}</span>
      {hint && <span className="small faint">{hint}</span>}
    </button>
  );
}
