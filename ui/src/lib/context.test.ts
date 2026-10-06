import { webcrypto } from 'node:crypto';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import {
  checkBudget,
  compactContext,
  contextReport,
  effectiveContext,
  estimateTokens,
  fingerprint,
  restoreContext,
} from './context';
import type { ContextBudget, ContextSummary } from './types';

const budget: ContextBudget = { contextWindow: 8192, outputTokens: 1024, autoCompact: false };
const summary: ContextSummary = {
  taskRequirements: ['Retain requirements'],
  corrections: ['Use Rust'],
  decisions: ['Use shared core'],
  changedFiles: ['context.rs'],
  checks: ['Offline checks passed'],
  unfinishedWork: ['Finish UI'],
  relevantContext: ['Historical approvals are not authorization'],
};
const source = () => [
  { role: 'user', content: 'Initial instructions' },
  { role: 'assistant', content: 'old 🦀 output'.repeat(4000) },
  { role: 'user', content: 'Correction: use Rust' },
  { role: 'assistant', content: 'Recent answer' },
  { role: 'user', content: 'Finish' },
];
beforeEach(() => vi.stubGlobal('crypto', webcrypto));

describe('browser context budgeting and compaction', () => {
  it('retains original history, all user instructions and recent turns with bounded summary requests', async () => {
    const state = await restoreContext(source());
    const original = structuredClone(state);
    const generate = vi.fn(async (messages) => {
      checkBudget(messages, budget);
      return JSON.stringify(summary);
    });
    const compacted = await compactContext(state, budget, generate);
    expect(state).toEqual(original);
    expect(compacted.transcript).toEqual(state.transcript);
    expect(compacted.checkpoints).toHaveLength(1);
    expect(generate.mock.calls.length).toBeGreaterThan(1);
    const effective = effectiveContext(compacted);
    expect(effective[0]).toEqual(state.transcript[0]);
    expect(effective.slice(2)).toEqual(state.transcript.slice(2));
    expect(effective[1].content).toContain('not authorization');
    checkBudget(effective, budget);
    expect(await restoreContext(source(), compacted)).toEqual(compacted);
  });
  it.each([
    '{}',
    '[]',
    'malformed',
    JSON.stringify({ ...summary, decisions: 'invalid' }),
    JSON.stringify({ ...summary, extra: 'unknown' }),
    JSON.stringify({ ...summary, changedFiles: ['x'.repeat(4000)] }),
  ])('rejects invalid summary %s without changing context', async (response) => {
    const state = await restoreContext(source());
    const original = structuredClone(state);
    await expect(compactContext(state, budget, async () => response)).rejects.toThrow();
    expect(state).toEqual(original);
  });
  it('retains previous context when summary generation is aborted or unavailable', async () => {
    const state = await restoreContext(source());
    const original = structuredClone(state);
    for (const error of [new DOMException('Stopped', 'AbortError'), new Error('Unavailable')]) {
      await expect(
        compactContext(state, budget, async () => {
          throw error;
        }),
      ).rejects.toThrow();
      expect(state).toEqual(original);
    }
  });
  it('rejects stale checkpoint sources and invalid historical tool pairs', async () => {
    const state = await restoreContext(source());
    const changed = source();
    changed[0].content = 'Changed task';
    await expect(restoreContext(changed, state)).rejects.toThrow('invalid');
    await expect(
      restoreContext([{ role: 'tool', content: 'orphan', tool_call_id: 'old' }]),
    ).rejects.toThrow('historical tool result');
    const compacted = await compactContext(state, budget, async () => JSON.stringify(summary));
    compacted.checkpoints[0].sourceHash = 'tampered';
    await expect(restoreContext(source(), compacted)).rejects.toThrow('checkpoint');
    expect(await fingerprint([{ role: 'user', content: 'a' }])).toEqual(
      await fingerprint([{ content: 'a', role: 'user' }]),
    );
  });
  it('estimates image input without treating encoded image bytes as text tokens', () => {
    const message = {
      role: 'user',
      content: [
        { type: 'text', text: 'Describe the image' },
        { type: 'image_url', image_url: { url: `data:image/png;base64,${'A'.repeat(500000)}` } },
      ],
    };
    expect(estimateTokens([message])).toBeLessThan(5000);
    checkBudget([message], { contextWindow: 32000, outputTokens: 4096, autoCompact: false });
    expect(JSON.stringify(message).length).toBeGreaterThan(500000);
  });
  it('reserves output and safety and separates estimates from endpoint counts', () => {
    expect(() => checkBudget(source(), budget)).toThrow('context budget');
    const usage = { promptTokens: 17, completionTokens: 4, totalTokens: 21 };
    const report = contextReport([{ role: 'user', content: 'Request' }], budget, usage);
    expect(report.serverUsage).toEqual(usage);
    expect(report.estimatedPromptTokens).not.toBe(usage.promptTokens);
    expect(report.outputTokens).toBe(1024);
    expect(report.safetyTokens).toBe(409);
  });
});
