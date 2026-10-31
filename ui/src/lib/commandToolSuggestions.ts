/**
 * Coding assistants offered in the Add form of Settings → Command tools. Each runs a program
 * without a shell, so a read-only flag is part of the command. The tool runs in the project
 * folder, so claude is told to load only the owner's settings and MCP servers, never the
 * project's. Only the first takes its prompt on standard input, which is what a subagent needs;
 * the others take it as an argument.
 */
export interface CommandToolSuggestion {
  name: string;
  description: string;
  command: string[];
  /** The prompt arrives as JSON on standard input, so a subagent can run on the tool. */
  promptOnStdin: boolean;
}

export const COMMAND_TOOL_SUGGESTIONS: CommandToolSuggestion[] = [
  {
    name: 'claude',
    description: 'Ask Claude Code for a read-only answer. Subagents can run on it.',
    command: [
      'claude',
      '-p',
      '--permission-mode',
      'plan',
      '--setting-sources',
      'user',
      '--strict-mcp-config',
      '--append-system-prompt',
      'Standard input is JSON; do the task in arguments.prompt.',
    ],
    promptOnStdin: true,
  },
  {
    name: 'codex',
    description: 'Ask Codex for a read-only answer.',
    command: ['codex', 'exec', '--sandbox', 'read-only', '{prompt}'],
    promptOnStdin: false,
  },
  {
    name: 'copilot',
    description: 'Ask GitHub Copilot CLI. It can use no tools that need permission.',
    command: ['copilot', '-p', '{prompt}'],
    promptOnStdin: false,
  },
];
