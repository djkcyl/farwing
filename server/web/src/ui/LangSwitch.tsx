// The interface language: 简体中文 / English, each in its own language.
import { LANGS, type Lang } from '../i18n/lang';
import { useLang, useT } from '../i18n/react';
import { Icon } from './icons';

export function LangSwitch({ className = '' }: { className?: string }) {
  const t = useT();
  const [lang, setLang] = useLang();
  return (
    <label className={`lang-switch ${className}`} title={t.common.language}>
      <Icon name="globe" size={14} />
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
