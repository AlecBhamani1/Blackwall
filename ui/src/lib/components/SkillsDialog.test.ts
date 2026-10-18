import { render, screen } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { persistence } from '../persistence';
import SkillsDialog from './SkillsDialog.svelte';

const release = {
  name: 'release',
  description: 'Ship a release.',
  body: 'Run the checklist.',
  enabled: true,
};
const source = { sessionId: 'session_1', messageId: 'message_1' };

describe('skill proposal review', () => {
  beforeEach(() => {
    Object.defineProperty(HTMLDialogElement.prototype, 'showModal', {
      configurable: true,
      value: function (this: HTMLDialogElement) {
        this.open = true;
      },
    });
    vi.spyOn(persistence, 'available').mockReturnValue(true);
    vi.spyOn(persistence, 'skills').mockResolvedValue({ skills: [release], warnings: [] });
    vi.spyOn(persistence, 'skillProposals').mockResolvedValue({
      proposals: [
        {
          id: 'skill_00000000000000aa',
          skill: { ...release, body: 'Run the checklist twice.' },
          before: release,
          source,
        },
        {
          id: 'skill_00000000000000bb',
          skill: { name: 'todo-audit', description: 'List TODOs.', body: 'Search.', enabled: true },
          before: null,
          source,
        },
      ],
      warnings: [],
    });
  });

  it('shows the saved skill and approves an edited proposal', async () => {
    const user = userEvent.setup();
    const approve = vi.spyOn(persistence, 'approveSkillProposal').mockResolvedValue();
    render(SkillsDialog, { props: { onClose: vi.fn() } });
    expect(await screen.findByText('Pending review · 2')).toBeInTheDocument();
    expect(screen.getByText('Updates this skill')).toBeInTheDocument();
    expect(screen.getByText('New skill', { selector: 'small' })).toBeInTheDocument();
    expect(screen.getByText(/Ship a release\.\s+Run the checklist\./)).toBeInTheDocument();
    const body = screen.getByLabelText('Proposed instructions for release');
    expect(body).toHaveValue('Run the checklist twice.');
    await user.clear(body);
    await user.type(body, 'Run the checklist once.');
    await user.click(screen.getByRole('button', { name: 'Approve release' }));
    expect(approve).toHaveBeenCalledWith(
      'skill_00000000000000aa',
      'Ship a release.',
      'Run the checklist once.',
    );
  });

  it('rejects a proposal and blocks approving empty instructions', async () => {
    const user = userEvent.setup();
    const reject = vi.spyOn(persistence, 'rejectSkillProposal').mockResolvedValue();
    render(SkillsDialog, { props: { onClose: vi.fn() } });
    await screen.findByText('Pending review · 2');
    await user.clear(screen.getByLabelText('Proposed instructions for todo-audit'));
    expect(screen.getByRole('button', { name: 'Approve todo-audit' })).toBeDisabled();
    await user.click(screen.getByRole('button', { name: 'Reject todo-audit' }));
    expect(reject).toHaveBeenCalledWith('skill_00000000000000bb');
  });

  it('shows the source message and an approval error', async () => {
    const user = userEvent.setup();
    vi.spyOn(persistence, 'load').mockResolvedValue({
      id: 'session_1',
      title: 'Release',
      updatedAt: 0,
      messages: [{ id: 'message_1', role: 'user', content: '/learn' }],
    } as never);
    vi.spyOn(persistence, 'approveSkillProposal').mockRejectedValue(
      'This skill changed after it was proposed. Review the new proposal instead.',
    );
    render(SkillsDialog, { props: { onClose: vi.fn() } });
    await screen.findByText('Pending review · 2');
    await user.click(screen.getAllByRole('button', { name: 'View source message' })[0]);
    expect(await screen.findByText('/learn', { selector: 'pre' })).toBeInTheDocument();
    const body = screen.getByLabelText('Proposed instructions for release');
    await user.type(body, ' Then tag it.');
    await user.click(screen.getByRole('button', { name: 'Approve release' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('changed after it was proposed');
    expect(body).toHaveValue('Run the checklist twice. Then tag it.');
  });

  it('still lists skills when proposals cannot be read', async () => {
    vi.spyOn(persistence, 'skillProposals').mockRejectedValue(new Error('unreadable'));
    render(SkillsDialog, { props: { onClose: vi.fn() } });
    expect(await screen.findByText('release')).toBeInTheDocument();
    expect(screen.getByText(/Proposed skills could not be read/)).toBeInTheDocument();
    expect(screen.queryByText(/Pending review/)).not.toBeInTheDocument();
  });
  it('keeps a successful review when the refresh afterwards fails', async () => {
    const user = userEvent.setup();
    vi.spyOn(persistence, 'rejectSkillProposal').mockResolvedValue();
    render(SkillsDialog, { props: { onClose: vi.fn() } });
    await screen.findByText('Pending review · 2');
    vi.spyOn(persistence, 'skills').mockRejectedValue(new Error('busy'));
    await user.click(screen.getByRole('button', { name: 'Reject todo-audit' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('reviewed, but your skills');
    expect(screen.getByText('Pending review · 1')).toBeInTheDocument();
  });
  it('keeps edits across a refresh and drops a reviewed proposal', async () => {
    const user = userEvent.setup();
    vi.spyOn(persistence, 'saveSkill').mockResolvedValue();
    vi.spyOn(persistence, 'approveSkillProposal').mockResolvedValue();
    render(SkillsDialog, { props: { onClose: vi.fn() } });
    await screen.findByText('Pending review · 2');
    const body = screen.getByLabelText('Proposed instructions for todo-audit');
    await user.type(body, ' Then report.');
    await user.click(screen.getByRole('button', { name: 'Disable release' }));
    expect(screen.getByLabelText('Proposed instructions for todo-audit')).toHaveValue(
      'Search. Then report.',
    );
    vi.spyOn(persistence, 'skillProposals').mockResolvedValue({ proposals: [], warnings: [] });
    await user.click(screen.getByRole('button', { name: 'Approve todo-audit' }));
    await vi.waitFor(() => expect(screen.queryByText(/Pending review/)).not.toBeInTheDocument());
  });
  it('opens the importer and reports what was imported', async () => {
    const user = userEvent.setup();
    vi.spyOn(persistence, 'findSkillImports').mockResolvedValue([
      {
        origin: 'Codex',
        source: '/s/SKILL.md',
        skill: { name: 'imported', description: 'd', body: 'b', enabled: true },
        error: null,
        notes: [],
        exists: false,
        files: [],
        license: null,
        findings: [],
      },
    ]);
    vi.spyOn(persistence, 'suggestedSkills').mockResolvedValue([]);
    vi.spyOn(persistence, 'importSkill').mockResolvedValue({
      name: 'imported',
      description: 'd',
      body: 'b',
      enabled: true,
    });
    render(SkillsDialog, { props: { onClose: vi.fn() } });
    await user.click(await screen.findByRole('button', { name: 'Import skills' }));
    await user.click(await screen.findByRole('button', { name: 'Import 1 skill' }));
    expect(await screen.findByRole('status')).toHaveTextContent('Imported 1 skill.');
    expect(screen.getByText('Pending review · 2')).toBeInTheDocument();
  });
});
