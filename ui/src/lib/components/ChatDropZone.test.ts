import { fireEvent, render, screen } from '@testing-library/svelte';
import { describe, expect, it, vi } from 'vitest';
import ChatDropZone from './ChatDropZone.svelte';
import Composer from './Composer.svelte';

function transfer() {
  return {
    files: [new File(['pixels'], 'Screenshot.png', { type: 'image/png' })],
    types: ['Files'],
    dropEffect: 'none',
  };
}

describe('conversation file drops', () => {
  it('stages a screenshot for sending through the same pipeline as the picker', async () => {
    const onSend = vi.fn().mockResolvedValue(true);
    const { component } = render(Composer, { onSend, onStop: vi.fn() });
    const onFiles = vi.fn((files: File[]) => component.addFiles(files));
    render(ChatDropZone, { onFiles });
    const chat = screen.getByRole('region', { name: 'Blackwall chat' });
    const dataTransfer = transfer();

    await fireEvent.dragEnter(chat, { dataTransfer });
    expect(screen.getByRole('status')).toHaveTextContent('Drop screenshots or files to attach');
    await fireEvent.dragOver(chat, { dataTransfer });
    expect(dataTransfer.dropEffect).toBe('copy');
    await fireEvent.drop(chat, { dataTransfer });

    expect(onFiles).toHaveBeenCalledOnce();
    expect(screen.queryByRole('status')).not.toBeInTheDocument();
    expect(screen.getByAltText('Preview of Screenshot.png')).toBeInTheDocument();
    expect(onSend).not.toHaveBeenCalled();
    await fireEvent.click(screen.getByRole('button', { name: 'Send message' }));
    expect(onSend).toHaveBeenCalledWith('', [
      expect.objectContaining({ kind: 'image', name: 'Screenshot.png' }),
    ]);
  });

  it('keeps the prompt through nested drag entries and clears it on the final leave', async () => {
    render(ChatDropZone, { onFiles: vi.fn() });
    const chat = screen.getByRole('region', { name: 'Blackwall chat' });
    const dataTransfer = transfer();
    await fireEvent.dragEnter(chat, { dataTransfer });
    await fireEvent.dragEnter(chat, { dataTransfer });
    await fireEvent.dragLeave(chat, { dataTransfer });
    expect(screen.getByRole('status')).toBeInTheDocument();
    await fireEvent.dragLeave(chat, { dataTransfer });
    expect(screen.queryByRole('status')).not.toBeInTheDocument();
  });

  it('rejects drops while a response is in progress', async () => {
    const onFiles = vi.fn();
    render(ChatDropZone, { onFiles, busy: true });
    const chat = screen.getByRole('region', { name: 'Blackwall chat' });
    const dataTransfer = transfer();
    await fireEvent.dragOver(chat, { dataTransfer });
    expect(dataTransfer.dropEffect).toBe('none');
    await fireEvent.drop(chat, { dataTransfer });
    expect(onFiles).not.toHaveBeenCalled();
  });

  it('leaves ordinary text drags alone', async () => {
    const onFiles = vi.fn();
    render(ChatDropZone, { onFiles });
    const chat = screen.getByRole('region', { name: 'Blackwall chat' });
    const dataTransfer = { files: [], types: ['text/plain'], dropEffect: 'move' };
    expect(await fireEvent.dragOver(chat, { dataTransfer })).toBe(true);
    expect(await fireEvent.drop(chat, { dataTransfer })).toBe(true);
    expect(onFiles).not.toHaveBeenCalled();
    expect(screen.queryByRole('status')).not.toBeInTheDocument();
  });
});
