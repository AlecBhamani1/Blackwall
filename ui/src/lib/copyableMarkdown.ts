import { renderMarkdown } from './markdown';

/** Render sanitized Markdown with copy controls outside each block's scrollable code. */
export function copyableMarkdown(node: HTMLElement, source: string) {
  let cleanup = () => {};

  function update(content: string) {
    cleanup();
    node.innerHTML = renderMarkdown(content);
    const disposers: Array<() => void> = [];

    for (const code of node.querySelectorAll('pre > code')) {
      const pre = code.parentElement!;
      const block = document.createElement('div');
      block.className = 'code-block';
      const toolbar = document.createElement('div');
      toolbar.className = 'code-block-toolbar';
      const button = document.createElement('button');
      button.type = 'button';
      button.className = 'code-block-copy';
      button.textContent = 'Copy code';
      button.setAttribute('aria-live', 'polite');
      toolbar.append(button);
      pre.replaceWith(block);
      block.append(toolbar, pre);

      let disposed = false;
      let timer: ReturnType<typeof setTimeout> | undefined;
      async function copy() {
        clearTimeout(timer);
        button.disabled = true;
        try {
          await navigator.clipboard.writeText(code.textContent ?? '');
          if (!disposed) button.textContent = 'Copied!';
        } catch {
          if (!disposed) button.textContent = 'Copy failed — retry';
        } finally {
          if (!disposed) {
            button.disabled = false;
            timer = setTimeout(() => (button.textContent = 'Copy code'), 2_000);
          }
        }
      }

      button.addEventListener('click', copy);
      disposers.push(() => {
        disposed = true;
        clearTimeout(timer);
        button.removeEventListener('click', copy);
      });
    }

    cleanup = () => disposers.forEach((dispose) => dispose());
  }

  update(source);
  return { update, destroy: () => cleanup() };
}
