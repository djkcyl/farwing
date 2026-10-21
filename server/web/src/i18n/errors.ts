import type { Lang } from './lang'

/** Text for a server error, by its code and, where the server gives one, its reason. */
export function errorMessage(lang: Lang, code: string, reason?: string): string {
  void reason
  return lang === 'en' ? `Error (${code})` : `错误（${code}）`
}
