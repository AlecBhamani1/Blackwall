import type { TokenizerAndRendererExtension, Tokens } from 'marked';

export interface MathToken extends Tokens.Generic {
  text: string;
  displayMode: boolean;
  incomplete?: boolean;
}

const delimiters = [
  { open: '$$', close: '$$', displayMode: true },
  { open: '\\[', close: '\\]', displayMode: true },
  { open: '\\(', close: '\\)', displayMode: false },
  { open: '$', close: '$', displayMode: false },
];

function readMath(source: string): MathToken | undefined {
  const delimiter = delimiters.find(({ open }) => source.startsWith(open));
  if (!delimiter) return;
  const { open, close, displayMode } = delimiter;
  if (open === '$' && /\s/.test(source[1] ?? ' ')) return;

  let braces = 0;
  let end = source.length;
  for (let index = open.length; index < source.length; index++) {
    if (!displayMode && source[index] === '\n') {
      end = index;
      break;
    }
    if (braces === 0 && source.startsWith(close, index)) {
      const text = source.slice(open.length, index);
      if (!text.trim()) return;
      // Avoid interpreting prices such as "$5 and $10" as an equation.
      if (open === '$' && (/\s/.test(text.at(-1)!) || /\d/.test(source[index + 1] ?? ''))) {
        return;
      }
      return {
        type: 'math',
        raw: source.slice(0, index + close.length),
        text,
        displayMode,
      };
    }
    // Escaped delimiters/braces belong to the TeX source, not its Markdown boundary.
    if (source[index] === '\\') index++;
    else if (source[index] === '{') braces++;
    else if (source[index] === '}') braces = Math.max(0, braces - 1);
  }
  // Markdown would otherwise erase these backslashes while an equation streams in.
  if (open.startsWith('\\')) {
    return { type: 'math', raw: source.slice(0, end), text: '', displayMode, incomplete: true };
  }
}

/** Tokenize before Markdown consumes TeX backslashes, underscores, or asterisks. */
export function mathExtensions(
  renderer: (token: MathToken) => string,
): TokenizerAndRendererExtension[] {
  return [
    {
      name: 'math',
      level: 'block',
      start: (source) => source.match(/(?:^|\n) {0,3}(?:\$\$|\\\[)/)?.index,
      tokenizer(source) {
        const indent = source.match(/^ {0,3}/)![0];
        const token = readMath(source.slice(indent.length));
        if (!token?.displayMode) return;
        const end = indent.length + token.raw.length;
        const trailing = source.slice(end).match(/^[ \t]*(?:\n|$)/);
        if (!trailing) return;
        return { ...token, raw: source.slice(0, end + trailing[0].length) };
      },
      renderer: (token) => renderer(token as MathToken),
    },
    {
      name: 'math',
      level: 'inline',
      start: (source) => source.match(/\$|\\[([]/)?.index,
      tokenizer: readMath,
      renderer: (token) => renderer(token as MathToken),
    },
  ];
}
