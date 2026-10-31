import { render, screen, waitFor, within } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { persistence, type OwnerRule, type PolicyAuditEntry } from '../persistence';
import OwnerRules from './OwnerRules.svelte';

const rules: OwnerRule[] = [
  { decision: 'deny', kind: 'file', path: '.git' },
  { decision: 'allow', kind: 'exec', tool: 'docs', project: '/work/app' },
  { decision: 'ask', kind: 'exec', command: 'git push' },
];
const entry = (time: number, subject: string, rule: PolicyAuditEntry['rule']): PolicyAuditEntry => ({
  time,
  kind: 'exec',
  subject,
  decision: rule ? 'allow' : 'ask',
  allowed: true,
  rule,
});

describe('Owner rules in Settings', () => {
  beforeEach(() => {
    vi.spyOn(persistence, 'ownerRules').mockResolvedValue({ rules });
    vi.spyOn(persistence, 'policyAudit').mockResolvedValue([]);
  });

  it('lists the rules in order with the mode and where command tools run', async () => {
    vi.spyOn(persistence, 'ownerRules').mockResolvedValue({ mode: 'ordered', rules });
    render(OwnerRules, { isolation: 'sandboxed' });
    const items = await screen.findAllByRole('listitem');
    expect(items.map((item) => item.textContent?.replace(/\s+/g, ' ').trim())).toEqual([
      expect.stringContaining('deny file .git'),
      expect.stringContaining('allow exec docs (project /work/app)'),
      expect.stringContaining('ask exec git push'),
    ]);
    expect(screen.getByText(/first matching rule decides/)).toBeInTheDocument();
    expect(screen.getByText(/inside the Commands profile/)).toBeInTheDocument();
  });

  it('says outside the sandbox and strictest when nothing else is set', async () => {
    render(OwnerRules, { isolation: 'outside' });
    expect(await screen.findByText(/deny outranks ask/)).toBeInTheDocument();
    expect(screen.getByText(/outside the sandbox/)).toBeInTheDocument();
  });

  it('adds a rule through the backend and shows the saved list', async () => {
    const user = userEvent.setup();
    const added: OwnerRule = { decision: 'allow', kind: 'file', path: 'src' };
    const save = vi.spyOn(persistence, 'saveOwnerRules').mockImplementation(async () => {
      vi.spyOn(persistence, 'ownerRules').mockResolvedValue({ rules: [...rules, added] });
    });
    render(OwnerRules);
    await screen.findAllByRole('listitem');
    await user.click(screen.getByRole('button', { name: 'Add rule' }));
    await user.selectOptions(screen.getByLabelText('Decision'), 'allow');
    await user.selectOptions(screen.getByLabelText('Kind'), 'file');
    await user.type(screen.getByLabelText('Path'), 'src');
    await user.click(screen.getByRole('button', { name: 'Save rule' }));
    expect(save).toHaveBeenCalledWith({ rules }, [...rules, added]);
    expect(await screen.findByText('allow file src')).toBeInTheDocument();
    expect(screen.queryByLabelText('Path')).not.toBeInTheDocument();
  });

  it('edits one rule and keeps its other fields', async () => {
    const user = userEvent.setup();
    const save = vi.spyOn(persistence, 'saveOwnerRules').mockResolvedValue();
    render(OwnerRules);
    await user.click(await screen.findByRole('button', { name: 'Edit rule 2: docs' }));
    expect(screen.getByLabelText('Kind')).toBeDisabled();
    await user.selectOptions(screen.getByLabelText('Decision'), 'deny');
    await user.clear(screen.getByLabelText('Tool'));
    await user.type(screen.getByLabelText('Tool'), 'docs__fetch');
    await user.click(screen.getByRole('button', { name: 'Save rule' }));
    expect(save).toHaveBeenCalledWith({ rules }, [
      rules[0],
      { decision: 'deny', kind: 'exec', tool: 'docs__fetch', project: '/work/app' },
      rules[2],
    ]);
  });

  it('deletes one rule', async () => {
    const user = userEvent.setup();
    const save = vi.spyOn(persistence, 'saveOwnerRules').mockResolvedValue();
    render(OwnerRules);
    await user.click(await screen.findByRole('button', { name: 'Delete rule 1: .git' }));
    expect(save).toHaveBeenCalledWith({ rules }, [rules[1], rules[2]]);
  });

  it("shows the backend's refusal and keeps the form open", async () => {
    const user = userEvent.setup();
    const refusal = 'Rule rules[3] path "src/**" uses a wildcard. Paths are plain prefixes.';
    vi.spyOn(persistence, 'saveOwnerRules').mockRejectedValue(refusal);
    render(OwnerRules);
    await screen.findAllByRole('listitem');
    await user.click(screen.getByRole('button', { name: 'Add rule' }));
    await user.selectOptions(screen.getByLabelText('Kind'), 'file');
    await user.type(screen.getByLabelText('Path'), 'src/**');
    await user.click(screen.getByRole('button', { name: 'Save rule' }));
    expect(await screen.findByRole('alert')).toHaveTextContent(refusal);
    expect(screen.getByLabelText('Path')).toHaveValue('src/**');
  });

  it('shows a broken file and offers no way to overwrite it', async () => {
    const error = 'The policy file could not be read or is not valid, so it was left unchanged.';
    vi.spyOn(persistence, 'ownerRules').mockRejectedValue(error);
    render(OwnerRules);
    expect(await screen.findByRole('alert')).toHaveTextContent(error);
    expect(screen.queryByRole('button', { name: 'Add rule' })).not.toBeInTheDocument();
  });

  it('says a test ran outside a project when only a project rule would match', async () => {
    const user = userEvent.setup();
    vi.spyOn(persistence, 'testOwnerRule').mockResolvedValue({
      decision: 'ask',
      matched: null,
      projectOnly: true,
    });
    render(OwnerRules);
    await screen.findAllByRole('listitem');
    const tester = within(screen.getByRole('group', { name: 'Test a rule' }));
    await user.type(tester.getByLabelText('Command'), 'docs');
    await user.click(tester.getByRole('button', { name: 'Test' }));
    expect(await tester.findByRole('status')).toHaveTextContent('outside a project');
  });

  it('closes the editor when a failed save finds the rules changed underneath it', async () => {
    const user = userEvent.setup();
    vi.spyOn(persistence, 'saveOwnerRules').mockRejectedValue('The rules changed since they were shown.');
    vi.spyOn(persistence, 'ownerRules')
      .mockResolvedValueOnce({ rules })
      .mockResolvedValue({ rules: [rules[2]] });
    render(OwnerRules);
    await user.click(await screen.findByRole('button', { name: 'Edit rule 1: .git' }));
    await user.type(screen.getByLabelText('Path'), 'x');
    await user.click(screen.getByRole('button', { name: 'Save rule' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('changed');
    expect(screen.queryByLabelText('Path')).not.toBeInTheDocument();
    expect(screen.getByText('ask exec git push')).toBeInTheDocument();
  });

  it('tests a command, a path and a host without saving anything', async () => {
    const user = userEvent.setup();
    const test = vi
      .spyOn(persistence, 'testOwnerRule')
      .mockResolvedValueOnce({
        decision: 'ask',
        matched: { source: 'user', index: 2, mode: 'strictest' },
        projectOnly: false,
      })
      .mockResolvedValueOnce({ decision: 'ask', matched: null, projectOnly: false });
    const save = vi.spyOn(persistence, 'saveOwnerRules');
    render(OwnerRules);
    await screen.findAllByRole('listitem');
    const tester = within(screen.getByRole('group', { name: 'Test a rule' }));
    await user.type(tester.getByLabelText('Command'), 'git push origin');
    await user.click(tester.getByRole('button', { name: 'Test' }));
    expect(test).toHaveBeenCalledWith('exec', 'git push origin');
    expect(await tester.findByRole('status')).toHaveTextContent('ask by rule 3: ask exec git push');
    await user.selectOptions(tester.getByLabelText('Test kind'), 'network');
    await user.type(tester.getByLabelText('Host'), 'example.com');
    await user.click(tester.getByRole('button', { name: 'Test' }));
    expect(test).toHaveBeenLastCalledWith('network', 'example.com');
    await waitFor(() => expect(tester.getByRole('status')).toHaveTextContent('ask, no rule matched'));
    expect(save).not.toHaveBeenCalled();
  });

  it('lists the audit log as the backend returned it, newest first', async () => {
    vi.spyOn(persistence, 'policyAudit').mockResolvedValue([
      entry(1_700_000_100, 'git status', { source: 'user', index: 1, mode: 'strictest' }),
      entry(1_700_000_000, 'make', null),
    ]);
    render(OwnerRules);
    const log = within(await screen.findByRole('region', { name: 'Audit log' }));
    const rows = log.getAllByRole('listitem').map((row) => row.textContent?.replace(/\s+/g, ' '));
    expect(rows).toEqual([
      expect.stringContaining('exec git status allow by rule 2'),
      expect.stringContaining('exec make ask no rule'),
    ]);
  });

  it('says when the audit log is empty', async () => {
    render(OwnerRules);
    expect(await screen.findByText('Nothing has been recorded yet.')).toBeInTheDocument();
  });
});
