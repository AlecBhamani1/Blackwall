import { render, screen } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { persistence } from '../persistence';
import RunsDialog from './RunsDialog.svelte';

const run = {
  id: '1792040999237-cli_run_2',
  startedMs: 1792040999237,
  durationMs: 21900,
  error: null,
  modelCalls: 2,
  toolCalls: 1,
  inputTokens: 2400,
  outputTokens: 231,
  model: 'local-model',
};
const spans = [
  {
    id: 'root',
    parent: null,
    name: 'invoke_agent blackwall',
    startMs: 0,
    durationMs: 21900,
    error: null,
    attributes: {},
    events: [],
  },
  {
    id: 'tool',
    parent: 'root',
    name: 'execute_tool write_file',
    startMs: 2,
    durationMs: 10,
    error: 'The operation was denied.',
    attributes: { 'gen_ai.tool.call.arguments': '{"path":"count.txt"}' },
    events: ['approval_requested'],
  },
  {
    id: 'chat',
    parent: 'root',
    name: 'chat local-model',
    startMs: 1,
    durationMs: 3900,
    error: null,
    attributes: {
      'gen_ai.usage.input_tokens': 1128,
      'gen_ai.usage.output_tokens': 168,
      'gen_ai.output.messages': JSON.stringify([
        { role: 'assistant', parts: [{ type: 'text', content: 'Writing the count.' }] },
      ]),
    },
    events: [],
  },
];

describe('runs dialog', () => {
  beforeEach(() => {
    Object.defineProperty(HTMLDialogElement.prototype, 'showModal', {
      configurable: true,
      value: function (this: HTMLDialogElement) {
        this.open = true;
      },
    });
    vi.spyOn(persistence, 'available').mockReturnValue(true);
    vi.spyOn(persistence, 'preferences').mockResolvedValue({});
    vi.spyOn(persistence, 'traces').mockResolvedValue([
      run,
      { ...run, id: 'b', modelCalls: 1, toolCalls: 0, error: 'The run was stopped.' },
    ]);
  });

  it('lists runs and opens one as an ordered tree with its content', async () => {
    const user = userEvent.setup();
    const read = vi.spyOn(persistence, 'trace').mockResolvedValue(spans);
    render(RunsDialog, { props: { onClose: vi.fn() } });
    expect(await screen.findByText('2 model calls · 1 tool call')).toBeInTheDocument();
    expect(screen.getByText('Failed: The run was stopped.')).toBeInTheDocument();
    await user.click(screen.getAllByRole('button', { name: /model call/ })[0]);
    expect(read).toHaveBeenCalledWith(run.id);
    const steps = screen
      .getAllByRole('listitem')
      .map((item) => item.querySelector('strong')?.textContent);
    expect(steps).toEqual([
      'invoke_agent blackwall',
      'chat local-model',
      'execute_tool write_file',
    ]);
    expect(screen.getByText(/1128 in \/ 168 out tokens/)).toBeInTheDocument();
    expect(screen.getByText(/approval requested/)).toBeInTheDocument();
    expect(screen.getByText('The operation was denied.')).toBeInTheDocument();
    expect(screen.getByText('{"path":"count.txt"}')).toBeInTheDocument();
    expect(screen.getByText('Writing the count.', { selector: 'pre' })).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: 'All runs' }));
    expect(screen.getByText('2 model calls · 1 tool call')).toBeInTheDocument();
  });

  it('defaults to recording without content and saves both settings', async () => {
    const user = userEvent.setup();
    const save = vi.spyOn(persistence, 'savePreferences').mockResolvedValue();
    render(RunsDialog, { props: { onClose: vi.fn() } });
    const record = await screen.findByLabelText('Record agent runs');
    await vi.waitFor(() => expect(record).toBeEnabled());
    const content = screen.getByLabelText('Include content');
    expect(record).toBeChecked();
    expect(content).not.toBeChecked();
    await user.click(content);
    expect(save).toHaveBeenLastCalledWith({ tracesEnabled: true, traceContent: true });
    await user.click(record);
    expect(save).toHaveBeenLastCalledWith({ tracesEnabled: false, traceContent: true });
    expect(content).toBeDisabled();
  });

  it('reports a run that cannot be read', async () => {
    const user = userEvent.setup();
    vi.spyOn(persistence, 'trace').mockRejectedValue('No trace has that ID.');
    render(RunsDialog, { props: { onClose: vi.fn() } });
    await user.click((await screen.findAllByRole('button', { name: /model call/ }))[0]);
    expect(await screen.findByRole('alert')).toHaveTextContent('No trace has that ID.');
  });

  it('says when there are no runs yet', async () => {
    vi.spyOn(persistence, 'traces').mockResolvedValue([]);
    render(RunsDialog, { props: { onClose: vi.fn() } });
    expect(await screen.findByText(/No runs yet/)).toBeInTheDocument();
  });

  it('keeps orphaned and repeated spans without looping', async () => {
    const user = userEvent.setup();
    vi.spyOn(persistence, 'trace').mockResolvedValue([
      ...spans,
      { ...spans[1], id: 'orphan', parent: 'missing', name: 'execute_tool orphan' },
      { ...spans[2], id: 'root', parent: 'root', name: 'repeated root' },
    ]);
    render(RunsDialog, { props: { onClose: vi.fn() } });
    await user.click((await screen.findAllByRole('button', { name: /model call/ }))[0]);
    const items = await screen.findAllByRole('listitem');
    expect(items.map((item) => item.querySelector('strong')?.textContent)).toEqual([
      'invoke_agent blackwall',
      'chat local-model',
      'repeated root',
      'execute_tool write_file',
      'execute_tool orphan',
    ]);
    expect(items[1]).toHaveAttribute('aria-level', '2');
  });

  it('shows the run opened last and restores a setting that was not saved', async () => {
    const user = userEvent.setup();
    let finishFirst: (value: typeof spans) => void = () => {};
    vi.spyOn(persistence, 'trace')
      .mockImplementationOnce(() => new Promise((resolve) => (finishFirst = resolve)))
      .mockResolvedValueOnce([{ ...spans[0], name: 'second run' }]);
    vi.spyOn(persistence, 'savePreferences').mockRejectedValue(new Error('busy'));
    render(RunsDialog, { props: { onClose: vi.fn() } });
    const content = await screen.findByLabelText('Include content');
    await vi.waitFor(() => expect(content).toBeEnabled());
    await user.click(content);
    expect(await screen.findByRole('alert')).toHaveTextContent('could not be saved');
    expect(content).not.toBeChecked();
    const [first, second] = screen.getAllByRole('button', { name: /model call/ });
    await user.click(first);
    await user.click(second);
    expect(await screen.findByText('second run')).toBeInTheDocument();
    finishFirst(spans);
    await new Promise((resolve) => setTimeout(resolve));
    expect(screen.getByText('second run')).toBeInTheDocument();
    expect(screen.queryByText('chat local-model')).not.toBeInTheDocument();
  });
});
