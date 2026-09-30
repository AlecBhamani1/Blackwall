import DOMPurify from 'dompurify';
import katex from 'katex';
import 'katex/dist/katex.min.css';
import { Marked } from 'marked';
import { mathExtensions, type MathToken } from './mathMarkdown';

export function renderMarkdown(source: string): string {
  const math = new Map<string, MathToken>();
  const nonce = Array.from(crypto.getRandomValues(new Uint8Array(16)), (byte) =>
    byte.toString(16).padStart(2, '0'),
  ).join('');
  const marked = new Marked({
    breaks: true,
    gfm: true,
    extensions: mathExtensions((token) => {
      const key = `${nonce}-${math.size}`;
      math.set(key, token);
      return `<span data-bw-math="${key}"></span>`;
    }),
  });
  const html = marked.parse(source, { async: false }) as string;
  const template = document.createElement('template');
  template.content.append(
    DOMPurify.sanitize(html, {
      FORBID_TAGS: ['style', 'script', 'iframe', 'object', 'embed', 'form'],
      FORBID_ATTR: ['style'],
      RETURN_DOM_FRAGMENT: true,
    }),
  );

  // Only library-generated math may retain layout styles. Unpredictable slots prevent
  // source HTML from impersonating math; all source markup is sanitized first.
  for (const slot of template.content.querySelectorAll<HTMLElement>('[data-bw-math]')) {
    const token = math.get(slot.dataset.bwMath ?? '');
    if (!token) continue;
    slot.removeAttribute('data-bw-math');
    if (token.incomplete) {
      slot.textContent = token.raw;
      continue;
    }
    try {
      slot.innerHTML = katex.renderToString(token.text, {
        displayMode: token.displayMode,
        output: 'htmlAndMathml',
        throwOnError: true,
        trust: false,
        strict: 'error',
        maxExpand: 1_000,
        maxSize: 20,
      });
    } catch {
      // Invalid/unsupported TeX remains readable and cannot interrupt a streamed response.
      slot.textContent = token.raw;
    }
  }
  return template.innerHTML;
}
