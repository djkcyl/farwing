// The interface language (`docs/i18n.md`): the choice saved in this
// browser, else the browser's languages (any Chinese → 简体中文, otherwise
// English). Components read the messages with `useT()` (`./react`); code
// outside components reads them with `t()` when it produces the text.
import type { Lang } from './lang';
import { messages, type Messages } from './messages';

export type { Lang } from './lang';
export type { Messages } from './messages';

const KEY = 'farwing.lang';

function isLang(value: unknown): value is Lang {
  return value === 'zh-CN' || value === 'en';
}

function saved(): Lang | undefined {
  try {
    const value = globalThis.localStorage?.getItem(KEY);
    return isLang(value) ? value : undefined;
  } catch {
    return undefined;
  }
}

function detect(): Lang {
  const nav = globalThis.navigator;
  const list = nav?.languages?.length ? nav.languages : nav?.language ? [nav.language] : [];
  return list.some((l) => l.toLowerCase().startsWith('zh')) ? 'zh-CN' : 'en';
}

let current: Lang = saved() ?? detect();
const listeners = new Set<() => void>();

function changed() {
  if (typeof document !== 'undefined') document.documentElement.lang = current;
  listeners.forEach((l) => l());
}

if (typeof document !== 'undefined') document.documentElement.lang = current;

// Another tab chose a language.
if (typeof window !== 'undefined') {
  window.addEventListener('storage', (event) => {
    if (event.key === KEY && isLang(event.newValue) && event.newValue !== current) {
      current = event.newValue;
      changed();
    }
  });
}

export function currentLang(): Lang {
  return current;
}

/** Messages of the current language. */
export function t(): Messages {
  return messages[current];
}

/** Switch the language and remember the choice in this browser. */
export function setLang(lang: Lang) {
  try {
    localStorage.setItem(KEY, lang);
  } catch {
    // Not saved (storage unavailable); the choice lasts for this page.
  }
  if (lang === current) return;
  current = lang;
  changed();
}

export function subscribeLang(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

/** Locale for `Intl` formatting. */
export function locale(): string {
  return current;
}
