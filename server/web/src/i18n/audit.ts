import type { Lang } from './lang'

/** Text for an audit log entry's detail, from the server's structured detail. */
export function auditDetail(lang: Lang, action: string, detail: string): string {
  void lang
  void action
  return detail
}
