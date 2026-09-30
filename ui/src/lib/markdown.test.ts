import { describe, expect, it } from 'vitest';
import { renderMarkdown } from './markdown';

function render(source: string) {
  const node = document.createElement('div');
  node.innerHTML = renderMarkdown(source);
  return node;
}

describe('Markdown math', () => {
  it.each(['$x_1^2 + x_2^2$', String.raw`\(x_1^2 + x_2^2\)`])(
    'renders inline TeX with accessible MathML: %s',
    (formula) => {
      const node = render(`The result is ${formula}, with **emphasis**.`);
      expect(node.querySelector('.katex')).not.toBeNull();
      expect(node.querySelector('.katex-display')).toBeNull();
      expect(node.querySelector('annotation')?.textContent).toBe('x_1^2 + x_2^2');
      expect(node.querySelector('strong')?.textContent).toBe('emphasis');
      expect(node.querySelector('.katex-html')?.getAttribute('aria-hidden')).toBe('true');
    },
  );

  it.each(['$$', String.raw`\[`])('renders multiline display TeX: %s', (open) => {
    const close = open === '$$' ? '$$' : String.raw`\]`;
    const tex = String.raw`\begin{aligned}
a_1 &= \frac{1}{2} \\
a_2 &= \sqrt{4}
\end{aligned}`;
    const node = render(`Before\n\n${open}\n${tex}\n${close}\n\nAfter`);
    expect(node.querySelectorAll('.katex-display')).toHaveLength(1);
    expect(node.querySelector('annotation')?.textContent?.trim()).toBe(tex);
    expect(node.textContent).toContain('Before');
    expect(node.textContent).toContain('After');
    // Fraction/alignment layout styles must survive without accepting source HTML styles.
    expect(node.querySelector('.katex [style]')).not.toBeNull();
  });

  it('handles equations in lists, quotes, and inline display notation', () => {
    const node = render(String.raw`- An inline equation: $x^2$

> \[x = \frac{1}{2}\]

Another: $$y = 3$$.`);
    expect(node.querySelectorAll('.katex')).toHaveLength(3);
    expect(node.querySelector('li .katex')).not.toBeNull();
    expect(node.querySelector('blockquote .katex-display')).not.toBeNull();
    expect(node.querySelectorAll('.katex-display')).toHaveLength(2);
  });

  it('leaves fenced, indented, and inline code verbatim', () => {
    const node = render(
      [
        'Inline: `$x$` and `\\(y\\)`.',
        '',
        '```latex',
        String.raw`$$\frac{1}{2}$$`,
        '```',
        '',
        String.raw`    \[z\]`,
      ].join('\n'),
    );
    expect(node.querySelector('.katex')).toBeNull();
    expect([...node.querySelectorAll('code')].map((code) => code.textContent)).toEqual([
      '$x$',
      String.raw`\(y\)`,
      String.raw`$$\frac{1}{2}$$` + '\n',
      String.raw`\[z\]` + '\n',
    ]);
  });

  it.each([
    'Costs $5 and $10, or $20.00.',
    String.raw`Escaped: \$x\$ and \\(y\\).`,
    '$ spaced $ and $unfinished',
    '$x\ny$',
  ])('preserves prices, escaped delimiters, and unfinished math: %s', (source) => {
    expect(render(source).querySelector('.katex')).toBeNull();
  });

  it.each([String.raw`\(x_1`, String.raw`\[\frac{1}{2}`])(
    'preserves backslash delimiters before a streamed formula closes: %s',
    (source) => {
      const node = render(source);
      expect(node.querySelector('.katex')).toBeNull();
      expect(node.textContent?.trimEnd()).toBe(source);
    },
  );

  it('handles escaped dollars and braces within TeX', () => {
    const tex = String.raw`\text{cost: \$5} + \{x\}`;
    const node = render(`$${tex}$`);
    expect(node.querySelector('annotation')?.textContent).toBe(tex);
  });

  it('falls back to readable source for invalid or unsupported TeX', () => {
    const source = String.raw`$\unknown{<img src=x onerror=alert(1)>}$`;
    const node = render(source + '\n\n**Still works**');
    expect(node.textContent).toContain(source);
    expect(node.querySelector('.katex')).toBeNull();
    expect(node.querySelector('img')).toBeNull();
    expect(node.querySelector('strong')?.textContent).toBe('Still works');
  });

  it('sanitizes source HTML and prevents TeX from generating executable or remote markup', () => {
    const node = render(String.raw`<span style="position:fixed" onclick="alert(1)">Safe</span>
<script>alert(1)</script><iframe src="https://evil.test"></iframe>
<a href="javascript:alert(1)">bad</a>
<span data-bw-math="fake">fake slot</span>

$\href{javascript:alert(1)}{click}$
$\includegraphics{https://evil.test/image.png}$
$\htmlStyle{background:url(https://evil.test)}{x}$
$x^2$`);
    expect(node.querySelector('script, iframe, img, [onclick]')).toBeNull();
    expect(node.querySelector('a')?.getAttribute('href')).toBeNull();
    expect(node.querySelector('span')?.getAttribute('style')).toBeNull();
    expect(node.innerHTML).not.toContain('style="background:');
    expect([...node.querySelectorAll('annotation')].at(-1)?.textContent).toBe('x^2');
  });

  it('bounds macro expansion and isolates macros between formulas and renders', () => {
    const loop = String.raw`$\def\loop{\loop}\loop$`;
    expect(render(loop).textContent).toContain(loop);
    const node = render(String.raw`$\gdef\foo{a}\foo$ and $\foo$`);
    expect(node.querySelectorAll('.katex')).toHaveLength(1);
    expect(node.textContent).toContain(String.raw`$\foo$`);
    expect(render(String.raw`$\foo$`).querySelector('.katex')).toBeNull();
  });
});
