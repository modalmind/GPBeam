import { describe, it, expect } from 'vitest';
import { isInsecureHttpUrl } from './url';

describe('isInsecureHttpUrl', () => {
  it('flags http to a remote host', () => {
    expect(isInsecureHttpUrl('http://cloud.example.com')).toBe(true);
    expect(isInsecureHttpUrl('http://cloud.example.com:8080/dav')).toBe(true);
  });
  it('allows https', () => {
    expect(isInsecureHttpUrl('https://cloud.example.com')).toBe(false);
  });
  it('allows http to loopback', () => {
    expect(isInsecureHttpUrl('http://localhost')).toBe(false);
    expect(isInsecureHttpUrl('http://127.0.0.1:8080')).toBe(false);
    expect(isInsecureHttpUrl('http://[::1]:8080')).toBe(false);
    expect(isInsecureHttpUrl('http://LOCALHOST')).toBe(false);
  });
  it('does not flag empty or partial input', () => {
    expect(isInsecureHttpUrl('')).toBe(false);
    expect(isInsecureHttpUrl('http://')).toBe(false);
  });
});
