/**
 * Keep the app window on Blackwall. Links (for example in model-written Markdown) never navigate
 * the window; public http(s) links open in the system browser and all others are ignored.
 */
export function guardLinks(root: Document, open: (url: string) => unknown): () => void {
  function onClick(event: MouseEvent) {
    const target = event.target instanceof Element ? event.target : null;
    const link = target?.closest('a[href]');
    if (!link) return;
    const href = link.getAttribute('href') ?? '';
    // In-page anchors such as footnotes stay inside the document.
    if (href.startsWith('#')) return;
    event.preventDefault();
    if (event.type === 'click' || event.button === 1) {
      const url = externalUrl(href);
      if (url) void Promise.resolve(open(url)).catch(() => {});
    }
  }
  root.addEventListener('click', onClick, true);
  root.addEventListener('auxclick', onClick, true);
  return () => {
    root.removeEventListener('click', onClick, true);
    root.removeEventListener('auxclick', onClick, true);
  };
}

function externalUrl(href: string): string | null {
  try {
    const url = new URL(href);
    return ['http:', 'https:'].includes(url.protocol) && !url.username && !url.password
      ? url.href
      : null;
  } catch {
    return null;
  }
}
