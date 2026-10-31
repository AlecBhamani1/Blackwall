import { describe, expect, it } from 'vitest';
import { formatCommand, parseCommand } from './mcpCommand';

describe('MCP command lines', () => {
  it('joins the program and arguments with spaces', () => {
    expect(formatCommand(['/home/riz/.local/bin/uvx', 'mcp-server-time'])).toBe(
      '/home/riz/.local/bin/uvx mcp-server-time',
    );
  });
  it('quotes an argument that is empty or contains spaces or quotes', () => {
    expect(formatCommand(['server', '--root', 'my folder', '', `it's`, 'say "hi"'])).toBe(
      `server --root "my folder" "" "it's" "say \\"hi\\""`,
    );
  });
  it('parses quoted arguments with spaces', () => {
    expect(parseCommand(`server --root "my folder" 'other folder' plain`)).toEqual([
      'server',
      '--root',
      'my folder',
      'other folder',
      'plain',
    ]);
  });
  it('gives back the exact program and arguments after a round trip', () => {
    for (const command of [
      ['/home/riz/.local/bin/uvx', 'mcp-server-time'],
      ['server', '--root', 'my folder', '--name=a b'],
      ['server', '', `it's`, 'say "hi"', 'back\\slash', 'C:\\Program Files\\tool.exe'],
      ['C:\\tools\\server.exe', '--flag'],
    ]) {
      expect(parseCommand(formatCommand(command))).toEqual(command);
    }
  });
  it('refuses a quote that is never closed', () => {
    expect(parseCommand('server "my folder')).toBeNull();
  });
  it('ignores extra spaces', () => {
    expect(parseCommand('  server   --flag ')).toEqual(['server', '--flag']);
    expect(parseCommand('   ')).toEqual([]);
  });
});
