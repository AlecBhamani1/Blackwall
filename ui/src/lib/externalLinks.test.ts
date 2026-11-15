import { afterEach, describe, expect, it, vi } from 'vitest';
import { guardLinks } from './externalLinks';

describe('guardLinks', () => {
  let release = () => {};
  afterEach(() => {
    release();
    document.body.innerHTML = '';
  });

  function click(href: string, init: MouseEventInit = {}, type = 'click') {
    const open = vi.fn();
    release = guardLinks(document, open);
    document.body.innerHTML = `<div class="markdown"><a href="${href}"><strong>here</strong></a></div>`;
    const event = new MouseEvent(type, { bubbles: true, cancelable: true, ...init });
    document.querySelector('strong')!.dispatchEvent(event);
    return { open, prevented: event.defaultPrevented };
  }

  it('opens public links in the system browser instead of navigating the window', () => {
    const { open, prevented } = click('https://evil.example/unlock');
    expect(prevented).toBe(true);
    expect(open).toHaveBeenCalledWith('https://evil.example/unlock');
  });

  it('opens middle-clicked links in the browser too', () => {
    const { open, prevented } = click('http://example.com/', { button: 1 }, 'auxclick');
    expect(prevented).toBe(true);
    expect(open).toHaveBeenCalledWith('http://example.com/');
  });

  it.each(['javascript:alert(1)', 'file:///etc/passwd', 'https://user:pw@example.com/', '/lock'])(
    'blocks without opening: %s',
    (href) => {
      const { open, prevented } = click(href);
      expect(prevented).toBe(true);
      expect(open).not.toHaveBeenCalled();
    },
  );

  it('keeps in-page anchors working', () => {
    const { open, prevented } = click('#fn-1');
    expect(prevented).toBe(false);
    expect(open).not.toHaveBeenCalled();
  });
});
