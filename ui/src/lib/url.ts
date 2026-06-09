const LOOPBACK = new Set(['localhost', '127.0.0.1', '::1']);

/** True when `url` is plain http:// to a non-loopback host — the unsafe case the
 *  backend rejects on save (it would send the app password and footage in the
 *  clear). https, loopback http, and partial/empty input all return false. */
export function isInsecureHttpUrl(url: string): boolean {
  if (!url.startsWith('http://')) return false;
  const authority = url.slice('http://'.length).split('/')[0];
  // Strip any userinfo, then the port (handling a [::1]-style IPv6 host).
  let host = authority.split('@').pop() ?? '';
  if (host.startsWith('[')) {
    const end = host.indexOf(']');
    host = end >= 0 ? host.slice(1, end) : host.slice(1);
  } else {
    host = host.split(':')[0];
  }
  return host !== '' && !LOOPBACK.has(host.toLowerCase());
}
