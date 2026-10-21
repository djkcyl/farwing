// The language in components: `useT()` gives the current messages and
// re-renders the component when the language changes.
import { createContext, createElement, useContext, useSyncExternalStore, type ReactNode } from 'react';
import { currentLang, setLang, subscribeLang, type Lang } from './index';
import { messages, type Messages } from './messages';

const LangContext = createContext<Lang>(currentLang());

export function LangProvider({ children }: { children: ReactNode }) {
  const lang = useSyncExternalStore(subscribeLang, currentLang);
  return createElement(LangContext.Provider, { value: lang }, children);
}

/** The current language and its setter (saved in this browser). */
export function useLang(): [Lang, (lang: Lang) => void] {
  return [useContext(LangContext), setLang];
}

/** Messages of the current language. */
export function useT(): Messages {
  return messages[useContext(LangContext)];
}
