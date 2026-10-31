import { describe, expect, it } from 'vitest';
import { MCP_SUGGESTIONS } from './mcpSuggestions';

describe('suggested MCP servers', () => {
  it('use valid names and pin an exact package version', () => {
    const names = MCP_SUGGESTIONS.map((item) => item.name);
    expect(new Set(names).size).toBe(names.length);
    for (const item of MCP_SUGGESTIONS) {
      expect(item.name).toMatch(/^[a-z0-9-]{1,32}$/);
      if (item.runtime === 'npx') {
        expect(item.command.slice(0, 2)).toEqual(['npx', '-y']);
        expect(item.command[2]).toMatch(/^(@[a-z0-9-]+\/)?[a-z0-9-]+@\d+\.\d+\.\d+$/);
      } else {
        expect(item.command).toEqual([
          'uvx',
          expect.stringMatching(/^mcp-server-[a-z]+@\d+\.\d+\.\d+$/),
        ]);
      }
    }
    expect(MCP_SUGGESTIONS.filter((item) => item.needs).map((item) => [item.name, item.needs])).toEqual([
      ['filesystem', 'folder'],
      ['remote', 'url'],
    ]);
  });
});
