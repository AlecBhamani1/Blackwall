/**
 * An MCP server's command as one line of text. An argument that is empty or contains a space or
 * a quote is wrapped in double quotes, inside which only `"` and `\` are escaped with `\`; the
 * parser also reads single quotes, which hold their text as it is.
 */
export function formatCommand(command: string[]) {
  return command
    .map((word) =>
      word === '' || /[\s"']/.test(word) ? `"${word.replace(/["\\]/g, '\\$&')}"` : word,
    )
    .join(' ');
}

/** The program and arguments of a command line, or null when a quote is never closed. */
export function parseCommand(line: string) {
  const words: string[] = [];
  let word: string | null = null;
  let quote = '';
  for (let index = 0; index < line.length; index++) {
    const char = line[index];
    if (quote === '"' && char === '\\' && /["\\]/.test(line[index + 1] ?? '')) {
      word += line[++index];
    } else if (quote) {
      if (char === quote) quote = '';
      else word += char;
    } else if (char === '"' || char === "'") {
      quote = char;
      word ??= '';
    } else if (/\s/.test(char)) {
      if (word !== null) words.push(word);
      word = null;
    } else {
      word = (word ?? '') + char;
    }
  }
  if (quote) return null;
  if (word !== null) words.push(word);
  return words;
}
