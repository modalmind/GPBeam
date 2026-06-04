import { describe, it, expect } from 'vitest';
import fs from 'node:fs';
import path from 'node:path';

describe('settings entry wiring', () => {
  it('settings.html references the settings entry script', () => {
    const html = fs.readFileSync(path.resolve(__dirname, '../../settings.html'), 'utf8');
    expect(html).toContain('/src/settings/main.ts');
    expect(html).toContain('id="app"');
  });

  it('main.ts mounts the Settings component into #app', () => {
    const main = fs.readFileSync(path.resolve(__dirname, 'main.ts'), 'utf8');
    expect(main).toContain('Settings.svelte');
    expect(main).toContain("target: document.getElementById('app')");
  });
});
