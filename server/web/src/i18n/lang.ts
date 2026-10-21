/** Interface languages of the web console (`docs/i18n.md`). */
export type Lang = 'zh-CN' | 'en';

/** The languages offered in the switcher, each named in its own language. */
export const LANGS: { id: Lang; name: string }[] = [
  { id: 'zh-CN', name: '简体中文' },
  { id: 'en', name: 'English' },
];
