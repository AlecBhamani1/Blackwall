/**
 * General-purpose MCP servers offered in the Add form. The list ships with the app and is never
 * downloaded, and each command pins an exact package version; raise versions in reviewed changes.
 */
export interface McpSuggestion {
  name: string;
  description: string;
  command: string[];
  runtime: 'npx' | 'uvx';
  /** Added to the end of the command by the owner before saving. */
  needs?: 'folder' | 'url';
}

export const MCP_SUGGESTIONS: McpSuggestion[] = [
  {
    name: 'chrome-devtools',
    description: 'Inspect and drive Chrome: pages, console, network, and performance.',
    command: ['npx', '-y', 'chrome-devtools-mcp@1.10.1'],
    runtime: 'npx',
  },
  {
    name: 'playwright',
    description: 'Automate a browser with Playwright.',
    command: ['npx', '-y', '@playwright/mcp@0.0.83'],
    runtime: 'npx',
  },
  {
    name: 'context7',
    description: 'Current documentation for libraries and APIs.',
    command: ['npx', '-y', '@upstash/context7-mcp@4.4.1'],
    runtime: 'npx',
  },
  {
    name: 'filesystem',
    description: 'Read and edit files in folders you choose.',
    command: ['npx', '-y', '@modelcontextprotocol/server-filesystem@1.0.0'],
    runtime: 'npx',
    needs: 'folder',
  },
  {
    name: 'sequential-thinking',
    description: 'Plan multi-step work one step at a time.',
    command: ['npx', '-y', '@modelcontextprotocol/server-sequential-thinking@1.0.0'],
    runtime: 'npx',
  },
  {
    name: 'git',
    description: 'Git history, diffs, and status.',
    command: ['npx', '-y', '@cyanheads/git-mcp-server@2.15.3'],
    runtime: 'npx',
  },
  {
    name: 'remote',
    description: 'Connect a server that is offered only as a web address.',
    command: ['npx', '-y', 'mcp-remote@0.14.3'],
    runtime: 'npx',
    needs: 'url',
  },
  {
    name: 'git-python',
    description: 'Git history, diffs, and status through the official Python server.',
    command: ['uvx', 'mcp-server-git@2026.10.10'],
    runtime: 'uvx',
  },
  {
    name: 'time',
    description: 'Current time and time-zone conversion.',
    command: ['uvx', 'mcp-server-time@2026.10.10'],
    runtime: 'uvx',
  },
  {
    name: 'fetch',
    description: 'Fetch web pages through the official Python server.',
    command: ['uvx', 'mcp-server-fetch@2026.10.10'],
    runtime: 'uvx',
  },
];
