import { invoke } from '@tauri-apps/api/core';
import type { PendingApproval, SessionSummary } from './types';
export const agentClient = {
  async chooseWorkspace(): Promise<string | null> {
    return invoke('choose_workspace');
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
