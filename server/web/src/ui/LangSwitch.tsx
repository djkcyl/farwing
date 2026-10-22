// The interface language: 简体中文 / English, each in its own language,
// chosen from the console's own menu.
import { useCallback, useState } from 'react';
import { LANGS } from '../i18n/lang';
import { useLang, useT } from '../i18n/react';
import { Icon } from './icons';
import { useDismiss } from './dismiss';

/// `compact`: the globe alone, as an icon button, its menu opening upwards
/// (the account bar at the bottom of the sidebar).
export function LangSwitch({ className = '', compact = false }: { className?: string; compact?: boolean }) {
  const t = useT();
  const [lang, setLang] = useLang();
  const [open, setOpen] = useState(false);
  const close = useCallback(() => setOpen(false), []);
  const ref = useDismiss(open, close);
  const current = LANGS.find((l) => l.id === lang);
  return (
    <div className={`lang-switch ${className}`} ref={ref}>
      <button
        type="button"
        className={compact ? `icon-btn btn-ghost ${open ? 'active' : ''}` : `lang-button ${open ? 'active' : ''}`}
        title={t.common.language}
        aria-label={t.common.language}
        aria-haspopup="menu"
        aria-expanded={open}
        onClick={() => setOpen(!open)}
      >
        <Icon name="globe" size={compact ? 16 : 14} />
        {!compact && <span>{current?.name}</span>}
      </button>
      {open && (
        <div className={`menu lang-menu ${compact ? 'menu-up' : ''}`} role="menu">
          {LANGS.map((l) => (
            <button
              key={l.id}
              type="button"
              role="menuitemradio"
              aria-checked={l.id === lang}
              lang={l.id}
              className={`menu-item ${l.id === lang ? 'selected' : ''}`}
              onClick={() => {
                setLang(l.id);
                setOpen(false);
              }}
            >
              <span className="menu-check">{l.id === lang && <Icon name="check" size={14} />}</span>
              <span className="grow">{l.name}</span>
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
