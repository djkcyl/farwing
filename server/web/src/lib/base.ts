// Where the console lives: `/`, or the path of the server's public URL
// behind a reverse proxy that strips it (the server writes it into
// `<base href>` of index.html).
export const BASE = new URL(document.baseURI).pathname.replace(/\/$/, '');

/// An absolute path of this site, e.g. `/api/v2/me` or `/farwing/api/v2/me`.
export function sitePath(path: string): string {
  return BASE + path;
}
