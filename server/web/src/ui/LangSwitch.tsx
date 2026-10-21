// The interface language: 简体中文 / English, each in its own language.
import { LANGS, type Lang } from '../i18n/lang';
import { useLang, useT } from '../i18n/react';
import { Icon } from './icons';

/// `compact`: the globe alone, as an icon button; the list opens from it.
export function LangSwitch({ className = '', compact = false }: { className?: string; compact?: boolean }) {
  const t = useT();
  const [lang, setLang] = useLang();
  return (
    <label className={`${compact ? 'icon-btn lang-icon' : 'lang-switch'} ${className}`} title={t.common.language}>
      <Icon name="globe" size={compact ? 16 : 14} />
      <select name="lang" aria-label={t.common.language} value={lang} onChange={(e) => setLang(e.target.value as Lang)}>
        {LANGS.map((l) => (
          <option key={l.id} value={l.id} lang={l.id}>
            {l.name}
          </option>
        ))}
      </select>
    </label>
  );
}
