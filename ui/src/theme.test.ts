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
});
