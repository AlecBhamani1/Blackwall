import type { ComponentProps } from 'svelte';
import type Icon from './components/Icon.svelte';

export type SettingsPage = 'connection' | 'security' | 'updates' | 'sharing';
export type SettingsPanel = 'memory' | 'skills' | 'mcp' | 'runs' | 'usage';
export const settingsGroups = ['General', 'AI capabilities', 'Workflows'] as const;

type SettingsEntry = {
  title: string;
  description: string;
  group: (typeof settingsGroups)[number];
  icon: ComponentProps<typeof Icon>['name'];
  keywords: string;
  desktopOnly?: boolean;
} & ({ kind: 'page'; id: SettingsPage } | { kind: 'dialog'; id: SettingsPanel });

// Register new app configuration and management destinations here, rather than in the chat sidebar.
export const settingsEntries: readonly SettingsEntry[] = [
  {
    id: 'connection',
    kind: 'page',
    group: 'General',
    icon: 'link',
    title: 'Model connection',
    description: 'Save and switch model services or paired computers',
    keywords: 'endpoint server ollama openai setup saved connections switch',
  },
  {
    id: 'security',
    kind: 'page',
    group: 'General',
    icon: 'shield',
    desktopOnly: true,
    title: 'Security',
    description: 'Access keys, app lock, telemetry, and trusted projects',
    keywords:
      'credentials token password privacy telemetry otlp opentelemetry export collector retention trust project policy',
  },
  {
    id: 'updates',
    kind: 'page',
    group: 'General',
    icon: 'refresh',
    title: 'App updates',
    description: 'Release channel and signed updates',
    keywords: 'version stable beta install',
  },
  {
    id: 'memory',
    kind: 'dialog',
    group: 'AI capabilities',
    icon: 'brain',
    title: 'Memory',
    description: 'Saved preferences & context',
    keywords: 'context window tokens limit instructions',
  },
  {
    id: 'skills',
    kind: 'dialog',
    group: 'AI capabilities',
    icon: 'sparkles',
    title: 'Skills',
    description: 'Reusable workflows',
    keywords: 'commands import learning proposals',
  },
  {
    id: 'mcp',
    kind: 'dialog',
    group: 'AI capabilities',
    icon: 'server',
    title: 'MCP servers',
    description: 'Tools from local servers',
    keywords: 'integrations permissions approvals tools',
  },
  {
    id: 'sharing',
    kind: 'page',
    group: 'Workflows',
    icon: 'external-link',
    title: 'Sharing & pairing',
    description: 'Guest links and paired devices',
    keywords: 'share model relay access computers phone',
  },
  {
    id: 'runs',
    kind: 'dialog',
    group: 'Workflows',
    icon: 'history',
    title: 'Runs',
    description: 'Traces of agent runs',
    keywords: 'history logs debug activity',
  },
  {
    id: 'usage',
    kind: 'dialog',
    group: 'Workflows',
    icon: 'chart',
    desktopOnly: true,
    title: 'Usage',
    description: 'Tokens, runs, and tools over time',
    keywords: 'tokens telemetry metrics statistics models projects cost export opentelemetry',
  },
];
