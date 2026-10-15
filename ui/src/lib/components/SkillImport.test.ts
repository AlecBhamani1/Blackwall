import { render, screen } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { persistence, type SkillImportCandidate } from '../persistence';
import SkillImport from './SkillImport.svelte';

const skill = (name: string) => ({
  name,
  description: `Use ${name}.`,
  body: 'Steps',
  enabled: true,
});
const found: SkillImportCandidate[] = [
  {
    origin: 'Claude Code',
    source: '/home/me/.claude/skills/pdf/SKILL.md',
    skill: skill('pdf'),
    error: null,
    notes: ['Bundled files are not imported: scripts/. Instructions that use them may not work.'],
    exists: false,
  },
  {
    origin: 'Codex',
    source: '/home/me/.codex/skills/release/SKILL.md',
    skill: skill('release'),
    error: null,
    notes: [],
    exists: true,
  },
  {
    origin: 'Claude Code',
    source: '/home/me/.claude/skills/big/SKILL.md',
    skill: null,
    error: 'Skills must be UTF-8 text and no larger than 32 KiB.',
    notes: [],
    exists: false,
  },
];

describe('skill import', () => {
  beforeEach(() => {
    vi.spyOn(persistence, 'findSkillImports').mockResolvedValue(found);
  });

  it('selects new skills, shows what is left out, and imports them', async () => {
    const user = userEvent.setup();
    const done = vi.fn();
    const save = vi.spyOn(persistence, 'importSkill').mockResolvedValue(skill('pdf'));
    render(SkillImport, { props: { onDone: done, onCancel: vi.fn() } });
    expect(await screen.findByLabelText('Import pdf')).toBeChecked();
    expect(screen.getByLabelText('Import release')).not.toBeChecked();
    expect(screen.getByText(/Bundled files are not imported/)).toBeInTheDocument();
    expect(screen.getByText(/no larger than 32 KiB/)).toBeInTheDocument();
    expect(screen.queryByLabelText('Import big')).not.toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Import 1 skill' }));
    expect(save).toHaveBeenCalledExactlyOnceWith(found[0].source, false);
    expect(done).toHaveBeenCalledWith(1);
  });

  it('replaces a saved skill only when asked', async () => {
    const user = userEvent.setup();
    const save = vi.spyOn(persistence, 'importSkill').mockResolvedValue(skill('release'));
    render(SkillImport, { props: { onDone: vi.fn(), onCancel: vi.fn() } });
    await user.click(await screen.findByLabelText('Import pdf'));
    await user.click(screen.getByLabelText('Import release'));
    expect(screen.getByRole('button', { name: 'Import 0 skills' })).toBeDisabled();
    await user.click(screen.getByLabelText(/A saved skill has this name/));
    await user.click(screen.getByRole('button', { name: 'Import 1 skill' }));
    expect(save).toHaveBeenCalledExactlyOnceWith(found[1].source, true);
  });

  it('adds skills from a chosen folder and reports an empty or failed choice', async () => {
    const user = userEvent.setup();
    const choose = vi
      .spyOn(persistence, 'chooseSkillImport')
      .mockResolvedValueOnce([{ ...found[0], origin: 'Chosen folder', source: '/tmp/x/SKILL.md' }])
      .mockResolvedValueOnce(null)
      .mockResolvedValueOnce([]);
    render(SkillImport, { props: { onDone: vi.fn(), onCancel: vi.fn() } });
    await screen.findByLabelText('Import release');
    await user.click(screen.getByRole('button', { name: 'Choose folder…' }));
    expect(choose).toHaveBeenLastCalledWith(true);
    expect(await screen.findByTitle('/tmp/x/SKILL.md')).toHaveTextContent('x/SKILL.md');
    expect(screen.getByRole('button', { name: 'Import 2 skills' })).toBeEnabled();
    await user.click(screen.getByRole('button', { name: 'Choose file…' }));
    expect(choose).toHaveBeenLastCalledWith(false);
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'Choose file…' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('No skills were found there');
  });

  it('keeps the list open when an import fails and marks the ones that worked as saved', async () => {
    const user = userEvent.setup();
    const done = vi.fn();
    vi.spyOn(persistence, 'findSkillImports').mockResolvedValue([
      found[0],
      { ...found[0], source: '/b/SKILL.md', skill: skill('second') },
    ]);
    vi.spyOn(persistence, 'importSkill')
      .mockResolvedValueOnce(skill('pdf'))
      .mockRejectedValueOnce('The skill folder could not be accessed.');
    render(SkillImport, { props: { onDone: done, onCancel: vi.fn() } });
    await user.click(await screen.findByRole('button', { name: 'Import 2 skills' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('second: The skill folder');
    expect(done).not.toHaveBeenCalled();
    expect(screen.getByLabelText('Import pdf')).not.toBeChecked();
    expect(screen.getAllByText(/A saved skill has this name/)).toHaveLength(1);
    expect(found[0].exists).toBe(false);
  });

  it('says where it looked when nothing is found', async () => {
    vi.spyOn(persistence, 'findSkillImports').mockResolvedValue([]);
    render(SkillImport, { props: { onDone: vi.fn(), onCancel: vi.fn() } });
    expect(await screen.findByText(/No skills found in ~\/.claude\/skills/)).toBeInTheDocument();
  });
});
