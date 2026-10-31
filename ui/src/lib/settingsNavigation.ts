import type { ComponentProps } from 'svelte';
import type Icon from './components/Icon.svelte';

export type SettingsPage =
  | 'connection'
  | 'updates'
  | 'limits'
  | 'subagents'
  | 'instructions'
  | 'mcp'
  | 'skills'
  | 'rules'
  | 'telemetry'
  | 'lock'
  | 'memory'
  | 'sharing'
  | 'runs'
  | 'usage';
export const settingsGroups = [
  'General',
  'Agent',
  'Privacy & security',
  'Memory',
  'Workflows',
] as const;
export type SettingsEntry = {
  id: SettingsPage;
  title: string;
  description: string;
  group: (typeof settingsGroups)[number];
  icon: ComponentProps<typeof Icon>['name'];
  keywords: string;
  desktopOnly?: boolean;
};

export const settingsEntries: readonly SettingsEntry[] = [
  {
    id: 'connection',
    group: 'General',
    icon: 'link',
    title: 'Model connection',
    description: 'Save and switch model services or paired computers',
    keywords: 'endpoint server ollama openai setup saved connections switch',
  },
  {
    id: 'updates',
    group: 'General',
    icon: 'refresh',
    title: 'App updates',
    description: 'Release channel and signed updates',
    keywords: 'version stable beta install',
  },
  {
    id: 'limits',
    group: 'Agent',
    icon: 'chart',
    title: 'Limits',
    description: 'Context, iteration, timeout, and output limits',
    keywords: 'context window tokens iterations timeout output cap compact',
  },
  {
    id: 'subagents',
    group: 'Agent',
    icon: 'sparkles',
    desktopOnly: true,
    title: 'Subagents',
    description: 'Subagent models, tools, and limits',
    keywords: 'delegation command tools isolation',
  },
  {
    id: 'instructions',
    group: 'Agent',
    icon: 'history',
    desktopOnly: true,
    title: 'Instructions',
    description: 'Instruction files and size limits',
    keywords: 'agents claude files skill size',
  },
  {
    id: 'mcp',
    group: 'Agent',
    icon: 'server',
    title: 'MCP servers',
    description: 'Tools from local servers',
    keywords: 'integrations permissions approvals tools',
  },
  {
    id: 'skills',
    group: 'Agent',
    icon: 'sparkles',
    title: 'Skills',
    description: 'Reusable workflows',
    keywords: 'commands import learning proposals',
  },
  {
    id: 'rules',
    group: 'Privacy & security',
    icon: 'shield',
    desktopOnly: true,
    title: 'Rules & sandbox',
    description: 'Access keys and trusted projects',
    keywords: 'credentials token password trust project policy sandbox rules',
  },
  {
    id: 'telemetry',
    group: 'Privacy & security',
    icon: 'chart',
    desktopOnly: true,
    title: 'Telemetry',
    description: 'Diagnostics and metrics export',
    keywords: 'privacy otlp opentelemetry collector retention',
  },
  {
    id: 'lock',
    group: 'Privacy & security',
    icon: 'shield',
    desktopOnly: true,
    title: 'Lock',
    description: 'Protect access to Blackwall',
    keywords: 'app lock password security',
  },
  {
    id: 'memory',
    group: 'Memory',
    icon: 'brain',
    title: 'Saved memories',
    description: 'Saved preferences and context',
    keywords: 'memory learning context saved chat',
  },
  {
    id: 'sharing',
    group: 'Workflows',
    icon: 'external-link',
    title: 'Sharing & pairing',
    description: 'Guest links and paired devices',
    keywords: 'share model relay access computers phone',
  },
  {
    id: 'runs',
    group: 'Workflows',
    icon: 'history',
    title: 'Runs',
    description: 'Traces of agent runs',
    keywords: 'history logs debug activity',
  },
  {
    id: 'usage',
    group: 'Workflows',
    icon: 'chart',
    desktopOnly: true,
    title: 'Usage',
    description: 'Tokens, runs, and tools over time',
    keywords: 'tokens telemetry metrics statistics models projects cost',
  },
];
