import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { describe, expect, it } from 'vitest';

describe('desktop configuration', () => {
  it('describes Blackwall in Linux application menus and switchers', () => {
    const config = JSON.parse(
      readFileSync(resolve(process.cwd(), '../src/app/tauri.conf.json'), 'utf8'),
    );

    // Tauri writes shortDescription as the desktop entry's Comment.
    expect(config.bundle.shortDescription).toBe('Local AI assistant');
  });
});
