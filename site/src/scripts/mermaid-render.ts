/**
 * Client-side Mermaid rendering.
 *
 * Finds every <pre> whose inner <code> has class "language-mermaid",
 * extracts the raw definition text, and replaces the <pre> with the
 * rendered SVG diagram.
 */

import mermaid from 'mermaid';

function initMermaid() {
  // Detect theme from data-theme attribute (set by theme-init.js).
  const isDark = document.documentElement.getAttribute('data-theme') === 'dark';

  mermaid.initialize({
    startOnLoad: false,
    theme: isDark ? 'dark' : 'default',
    fontFamily: 'Inter, system-ui, sans-serif',
    sequence: {
      actorFontSize: 14,
      messageFontSize: 13,
      noteFontSize: 12,
      wrap: true,
    },
  });

  const codeBlocks = document.querySelectorAll<HTMLElement>(
    'pre > code.language-mermaid, code[data-language="mermaid"]'
  );

  // Also match Shiki-style: <pre> with data-language="mermaid"
  const preBlocks = document.querySelectorAll<HTMLPreElement>(
    'pre[data-language="mermaid"]'
  );

  const targets = new Set<HTMLElement>();

  codeBlocks.forEach((code) => {
    const pre = code.closest('pre');
    if (pre) targets.add(pre);
  });

  preBlocks.forEach((pre) => targets.add(pre));

  // Also match by Astro's Shiki class — look for <pre> containing "sequenceDiagram" etc.
  document.querySelectorAll<HTMLPreElement>('pre.astro-code').forEach((pre) => {
    const text = pre.textContent?.trim() ?? '';
    if (
      text.startsWith('sequenceDiagram') ||
      text.startsWith('graph ') ||
      text.startsWith('flowchart ') ||
      text.startsWith('classDiagram') ||
      text.startsWith('stateDiagram') ||
      text.startsWith('erDiagram') ||
      text.startsWith('gantt') ||
      text.startsWith('pie') ||
      text.startsWith('gitgraph')
    ) {
      targets.add(pre);
    }
  });

  let idx = 0;
  targets.forEach(async (pre) => {
    const definition = pre.textContent?.trim();
    if (!definition) return;

    const id = `mermaid-diagram-${idx++}`;
    try {
      const { svg } = await mermaid.render(id, definition);
      const wrapper = document.createElement('div');
      wrapper.className = 'mermaid-diagram';
      wrapper.innerHTML = svg;
      pre.replaceWith(wrapper);
    } catch (err) {
      console.warn('[mermaid] render failed:', err);
      // Leave the code block as-is on error.
    }
  });
}

// Run on initial load and on Astro client-side navigations.
initMermaid();
document.addEventListener('astro:after-swap', initMermaid);
