import { fireEvent, render, screen, waitFor } from '@testing-library/svelte';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { workspaceClient, type DirectoryListing, type FileSearch } from '../agent';
import WorkspaceFiles from './WorkspaceFiles.svelte';

vi.mock('../agent', () => ({
  workspaceClient: { browse: vi.fn(), search: vi.fn(), read: vi.fn() },
}));

const docs = { name: 'docs', path: './docs', isDirectory: true };
const readme = { name: 'README.md', path: './README.md', isDirectory: false };
const guide = { name: 'guide.md', path: './docs/guide.md', isDirectory: false };

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => (resolve = done));
  return { promise, resolve };
}

beforeEach(() => {
  vi.mocked(workspaceClient.browse)
    .mockReset()
    .mockImplementation(async (_, path) => ({
      entries: path === '.' ? [docs, readme] : [guide],
      truncated: false,
    }));
  vi.mocked(workspaceClient.search)
    .mockReset()
    .mockResolvedValue({
      matches: [{ ...guide, line: 2, preview: 'A project guide' }],
      truncated: false,
    });
  vi.mocked(workspaceClient.read).mockReset().mockResolvedValue('<script>plain text</script>');
});

describe('Agent mode project files', () => {
  it('expands folders, previews files as text, and collapses the tree', async () => {
    const onClose = vi.fn();
    render(WorkspaceFiles, { workspace: '/projects/Blackwall', onClose });
    const folder = await screen.findByRole('button', { name: 'docs' });
    expect(folder).toHaveAttribute('aria-expanded', 'false');
    await fireEvent.click(folder);
    const file = await screen.findByRole('button', { name: 'guide.md' });
    expect(folder).toHaveAttribute('aria-expanded', 'true');
    expect(workspaceClient.browse).toHaveBeenCalledWith('/projects/Blackwall', './docs');
    await fireEvent.click(file);
    expect(await screen.findByText('<script>plain text</script>')).toBeInTheDocument();
    expect(workspaceClient.read).toHaveBeenCalledWith('/projects/Blackwall', './docs/guide.md');
    await fireEvent.click(screen.getByRole('button', { name: 'Close file preview' }));
    expect(screen.queryByRole('region', { name: 'File preview' })).not.toBeInTheDocument();
    await fireEvent.click(screen.getByRole('button', { name: 'Collapse all folders' }));
    expect(screen.queryByRole('button', { name: 'guide.md' })).not.toBeInTheDocument();
    await fireEvent.click(screen.getByRole('button', { name: 'Hide project files' }));
    expect(onClose).toHaveBeenCalledOnce();
  });

  it('searches names and contents and returns to the tree when cleared', async () => {
    render(WorkspaceFiles, { workspace: '/project', onClose: vi.fn() });
    const input = screen.getByRole('textbox', { name: 'Find files' });
    await fireEvent.input(input, { target: { value: 'guide' } });
    await screen.findByRole('button', { name: /docs\/guide.md/ });
    expect(workspaceClient.search).toHaveBeenLastCalledWith('/project', 'guide', 'names');
    await fireEvent.click(screen.getByRole('button', { name: 'Contents' }));
    await waitFor(() => {
      expect(workspaceClient.search).toHaveBeenLastCalledWith('/project', 'guide', 'contents');
    });
    expect(await screen.findByText('2: A project guide')).toBeInTheDocument();
    await fireEvent.input(input, { target: { value: '' } });
    expect(screen.getByRole('button', { name: 'README.md' })).toBeInTheDocument();
  });

  it('ignores search results from an older query', async () => {
    const oldSearch = deferred<FileSearch>();
    vi.mocked(workspaceClient.search).mockImplementationOnce(() => oldSearch.promise);
    render(WorkspaceFiles, { workspace: '/project', onClose: vi.fn() });
    const input = screen.getByRole('textbox', { name: 'Find files' });
    await fireEvent.input(input, { target: { value: 'old' } });
    await waitFor(() => expect(workspaceClient.search).toHaveBeenCalledOnce());
    await fireEvent.input(input, { target: { value: 'new' } });
    await screen.findByRole('button', { name: /docs\/guide.md/ });
    oldSearch.resolve({ matches: [{ ...readme, line: null, preview: null }], truncated: false });
    await waitFor(() =>
      expect(screen.queryByRole('button', { name: 'README.md' })).not.toBeInTheDocument(),
    );
    expect(screen.getByRole('button', { name: /docs\/guide.md/ })).toBeInTheDocument();
  });

  it('reveals a nested folder found by search', async () => {
    const nested = { name: 'nested', path: './docs/nested', isDirectory: true };
    vi.mocked(workspaceClient.browse).mockImplementation(async (_, path) => ({
      entries: path === '.' ? [docs] : path === './docs' ? [nested] : [guide],
      truncated: false,
    }));
    vi.mocked(workspaceClient.search).mockResolvedValue({
      matches: [{ ...nested, line: null, preview: null }],
      truncated: false,
    });
    render(WorkspaceFiles, { workspace: '/project', onClose: vi.fn() });
    await fireEvent.input(screen.getByRole('textbox', { name: 'Find files' }), {
      target: { value: 'nested' },
    });
    await fireEvent.click(await screen.findByRole('button', { name: 'docs/nested' }));
    await screen.findByRole('button', { name: 'guide.md' });
    expect(screen.getByRole('button', { name: 'docs' })).toHaveAttribute('aria-expanded', 'true');
    expect(screen.getByRole('button', { name: 'nested' })).toHaveAttribute('aria-expanded', 'true');
  });

  it('discards old directory and preview responses after changing projects', async () => {
    const oldFolder = deferred<DirectoryListing>();
    const oldPreview = deferred<string>();
    vi.mocked(workspaceClient.browse).mockImplementation(async (project, path) => {
      if (path !== '.') return oldFolder.promise;
      return { entries: project === '/old' ? [docs, readme] : [guide], truncated: false };
    });
    vi.mocked(workspaceClient.read).mockReturnValue(oldPreview.promise);
    const { rerender } = render(WorkspaceFiles, { workspace: '/old', onClose: vi.fn() });
    await fireEvent.click(await screen.findByRole('button', { name: 'docs' }));
    await fireEvent.click(screen.getByRole('button', { name: 'README.md' }));
    await rerender({ workspace: '/new' });
    await screen.findByRole('button', { name: 'guide.md' });
    oldFolder.resolve({ entries: [readme], truncated: false });
    oldPreview.resolve('Old private content');
    await waitFor(() => expect(screen.queryByText('Old private content')).not.toBeInTheDocument());
    expect(screen.queryByRole('button', { name: 'README.md' })).not.toBeInTheDocument();
    expect(screen.queryByRole('region', { name: 'File preview' })).not.toBeInTheDocument();
  });

  it('shows read failures, refreshes the listing, and explains bounded results', async () => {
    vi.mocked(workspaceClient.browse).mockRejectedValueOnce('The folder is protected.');
    render(WorkspaceFiles, { workspace: '/project', onClose: vi.fn() });
    expect(await screen.findByRole('alert')).toHaveTextContent('The folder is protected.');
    vi.mocked(workspaceClient.browse).mockResolvedValueOnce({ entries: [readme], truncated: true });
    await fireEvent.click(screen.getByRole('button', { name: 'Refresh project files' }));
    expect(await screen.findByText(/Showing up to 500/)).toBeInTheDocument();
    vi.mocked(workspaceClient.read).mockRejectedValueOnce(
      'This tool supports text files up to 64 KiB.',
    );
    await fireEvent.click(screen.getByRole('button', { name: 'README.md' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('64 KiB');
  });
});
