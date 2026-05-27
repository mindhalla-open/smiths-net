/**
 * Code block copy-to-clipboard.
 *
 * Injects a copy button into every <pre> element. For bash/shell
 * blocks with multiple commands, the text is preserved exactly
 * so a paste into the terminal runs all lines.
 */

const COPY_ICON = `<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="9" y="9" width="13" height="13" rx="2"/><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/></svg>`;
const CHECK_ICON = `<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="20 6 9 17 4 12"/></svg>`;

function extractCopyText(pre: HTMLPreElement): string {
  const code = pre.querySelector('code');
  if (!code) return pre.textContent?.trim() ?? '';

  // Get the raw text, strip trailing whitespace per line
  const raw = code.textContent ?? '';
  const lines = raw.split('\n');

  // Remove leading/trailing empty lines
  while (lines.length && lines[0].trim() === '') lines.shift();
  while (lines.length && lines[lines.length - 1].trim() === '') lines.pop();

  return lines.join('\n');
}

function initCopyButtons() {
  document.querySelectorAll<HTMLPreElement>('pre').forEach((pre) => {
    // Don't add twice
    if (pre.querySelector('.code-copy-btn')) return;

    const btn = document.createElement('button');
    btn.className = 'code-copy-btn';
    btn.setAttribute('aria-label', 'Copy code');
    btn.setAttribute('title', 'Copy to clipboard');
    btn.innerHTML = COPY_ICON;

    btn.addEventListener('click', async () => {
      const text = extractCopyText(pre);

      try {
        await navigator.clipboard.writeText(text);

        btn.innerHTML = CHECK_ICON;
        btn.setAttribute('data-copied', 'true');

        setTimeout(() => {
          btn.innerHTML = COPY_ICON;
          btn.removeAttribute('data-copied');
        }, 2000);
      } catch {
        // Fallback for non-HTTPS / older browsers
        const textarea = document.createElement('textarea');
        textarea.value = text;
        textarea.style.position = 'fixed';
        textarea.style.opacity = '0';
        document.body.appendChild(textarea);
        textarea.select();
        document.execCommand('copy');
        document.body.removeChild(textarea);

        btn.innerHTML = CHECK_ICON;
        btn.setAttribute('data-copied', 'true');

        setTimeout(() => {
          btn.innerHTML = COPY_ICON;
          btn.removeAttribute('data-copied');
        }, 2000);
      }
    });

    pre.appendChild(btn);
  });
}

// Run on initial load and on Astro client-side navigations
initCopyButtons();
document.addEventListener('astro:after-swap', initCopyButtons);
