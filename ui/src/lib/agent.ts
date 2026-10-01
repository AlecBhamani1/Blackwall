import { invoke } from '@tauri-apps/api/core';
import type { PendingApproval, SessionSummary } from './types';
export interface FileEntry {
  name: string;
  path: string;
  isDirectory: boolean;
}
export interface DirectoryListing {
  entries: FileEntry[];
  truncated: boolean;
}
export interface FileSearchMatch extends FileEntry {
  line: number | null;
  preview: string | null;
}
export interface FileSearch {
  matches: FileSearchMatch[];
  truncated: boolean;
}
export type FileSearchMode = 'names' | 'contents';

export const workspaceClient = {
  async browse(workspace: string, path: string): Promise<DirectoryListing> {
    return invoke('browse_workspace', { workspace, path });
  },
  async search(workspace: string, query: string, mode: FileSearchMode): Promise<FileSearch> {
    return invoke('search_workspace', { workspace, query, mode });
  },
  async read(workspace: string, path: string): Promise<string> {
    return invoke('read_workspace_file', { workspace, path });
  },
};
export const agentClient = {
  async chooseWorkspace(): Promise<string | null> {
    return invoke('choose_workspace');
  },
  async restoreWorkspace(sessionId: string): Promise<string | null> {
    return invoke('restore_workspace', { sessionId });
  },
  async resolve(
    approval: PendingApproval,
    decision: 'allow' | 'always_allow' | 'deny',
  ): Promise<void> {
    await invoke('resolve_approval', {
      request: { requestId: approval.requestId, approvalId: approval.approvalId, decision },
    });
  },
  async export(session: SessionSummary): Promise<boolean> {
    return invoke('export_conversation', { session });
  },
};
