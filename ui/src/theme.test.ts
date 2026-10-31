import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { describe, expect, it } from 'vitest';

describe('Blackwall theme contract', () => {
  it('defines every color token required by the implementation plan', () => {
    const theme = readFileSync(resolve(process.cwd(), 'src/theme.css'), 'utf8');
    const requiredTokens = [
      '--bg-base',
      '--bg-panel',
      '--bg-elevated',
      '--bg-user-msg',
      '--border',
      '--text-primary',
      '--text-muted',
      '--text-faint',
      '--accent',
      '--ok',
      '--err',
      '--warn',
      '--focus-ring',
      '--subagent-bg',
    ];

    for (const token of requiredTokens) expect(theme).toContain(`${token}:`);
  });

  it('styles native selects for the active color theme', () => {
    const theme = readFileSync(resolve(process.cwd(), 'src/theme.css'), 'utf8');

    expect(theme).toMatch(/select\s*\{[^}]*appearance:\s*none/s);
    expect(theme).toMatch(/select\s*\{[^}]*background-image:\s*url\("data:image\/svg\+xml,/s);
    expect(theme).toMatch(/select\s*\{[^}]*color:\s*var\(--text-primary\)/s);
    expect(theme).toMatch(/select\s*\{[^}]*border:\s*1px solid var\(--border-strong\)/s);
  });

  it('keeps Memory radio controls out of full-width text input styling', () => {
    const memoryDialog = readFileSync(
      resolve(process.cwd(), 'src/lib/components/MemoryDialog.svelte'),
      'utf8',
    );

    expect(memoryDialog).toContain("input:not([type='checkbox']):not([type='radio'])");
  });
});
