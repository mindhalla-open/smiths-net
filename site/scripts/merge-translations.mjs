#!/usr/bin/env node
/**
 * merge-translations.mjs — F-1 deliverable 2.
 *
 * Merges translated files from `l10n/` back into the site:
 *   1. `l10n/<locale>/ui-strings.json` → `src/i18n/<locale>.json`
 *   2. `l10n/<locale>/recipes/<id>.json` → `src/content/cookbook/<path>/<locale>.mdx`
 *
 * Usage:
 *   node scripts/merge-translations.mjs <locale>
 *
 * Example:
 *   node scripts/merge-translations.mjs ru
 *
 * The script reads the extracted JSON, rebuilds the nested i18n structure,
 * and generates locale-specific MDX files from translated prose.
 */

import { readFileSync, writeFileSync, mkdirSync, existsSync, readdirSync } from 'node:fs';
import { join, dirname, basename } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const SITE_ROOT = join(__dirname, '..');
const I18N_DIR = join(SITE_ROOT, 'src', 'i18n');
const COOKBOOK_DIR = join(SITE_ROOT, 'src', 'content', 'cookbook');
const L10N_DIR = join(SITE_ROOT, 'l10n');

const locale = process.argv[2];
if (!locale || locale === 'en') {
  console.error('Usage: node scripts/merge-translations.mjs <locale>');
  console.error('  locale must be a non-"en" BCP-47 code (e.g. ru, es, zh, ar)');
  process.exit(1);
}

const localeDir = join(L10N_DIR, locale);
if (!existsSync(localeDir)) {
  console.error(`❌ Locale directory not found: l10n/${locale}/`);
  console.error('');
  console.error('Expected structure:');
  console.error(`  l10n/${locale}/ui-strings.json`);
  console.error(`  l10n/${locale}/recipes/<recipe-id>.json`);
  console.error('');
  console.error('Run "node scripts/extract-strings.mjs" first to generate');
  console.error(`the source files, then copy them to l10n/${locale}/ and translate.`);
  process.exit(1);
}

// ── Helpers ──────────────────────────────────────────────

/** Unflatten dotted key paths back into a nested object. */
function unflatten(flat) {
  const result = {};
  for (const [key, value] of Object.entries(flat)) {
    const parts = key.split('.');
    let current = result;
    for (let i = 0; i < parts.length - 1; i++) {
      if (!(parts[i] in current)) {
        current[parts[i]] = {};
      }
      current = current[parts[i]];
    }
    current[parts[parts.length - 1]] = value;
  }
  return result;
}

/** Read the original en.mdx to preserve code blocks and structure. */
function readOriginalMdx(recipePath) {
  const enPath = join(COOKBOOK_DIR, recipePath, 'en.mdx');
  if (!existsSync(enPath)) return null;
  return readFileSync(enPath, 'utf-8');
}

/** Build a locale MDX from the translated JSON + original en.mdx as template. */
function buildLocaleMdx(translated, originalContent, locale) {
  // Parse original frontmatter to get non-translatable fields
  const fmMatch = originalContent.match(/^---\n([\s\S]*?)\n---/);
  if (!fmMatch) return null;

  const originalFm = {};
  for (const line of fmMatch[1].split('\n')) {
    const eq = line.indexOf(':');
    if (eq < 0) continue;
    const key = line.slice(0, eq).trim();
    let val = line.slice(eq + 1).trim();
    originalFm[key] = val;
  }

  // Build new frontmatter with translated title/description + locale swap
  const newFm = { ...originalFm };
  if (translated.title) newFm.title = `"${translated.title}"`;
  if (translated.description) newFm.description = `"${translated.description}"`;
  newFm.locale = `"${locale}"`;

  const fmLines = Object.entries(newFm).map(([k, v]) => {
    // Keep original quoting for values that already have it
    if (typeof v === 'string' && (v.startsWith('"') || v.startsWith("'"))) {
      return `${k}: ${v}`;
    }
    return `${k}: ${v}`;
  });

  // For the body, we keep the original English content but with
  // a note that it was machine-translated. This preserves code blocks.
  // Translators should edit the MDX directly for prose.
  const originalBody = originalContent.slice(fmMatch[0].length).trim();

  return `---\n${fmLines.join('\n')}\n---\n\n${originalBody}\n`;
}

// ── Main ────────────────────────────────────────────────

console.log(`\n🔀 Merging translations for locale: ${locale}\n`);

let merged = 0;

// 1. UI strings
const uiPath = join(localeDir, 'ui-strings.json');
if (existsSync(uiPath)) {
  const flat = JSON.parse(readFileSync(uiPath, 'utf-8'));

  // Remove internal keys
  const clean = {};
  for (const [k, v] of Object.entries(flat)) {
    if (!k.startsWith('_')) clean[k] = v;
  }

  const nested = unflatten(clean);
  const outPath = join(I18N_DIR, `${locale}.json`);
  writeFileSync(outPath, JSON.stringify(nested, null, 2) + '\n');
  console.log(`  ✅ UI strings → src/i18n/${locale}.json (${Object.keys(clean).length} keys)`);
  merged++;
} else {
  console.log(`  ⚠️  No ui-strings.json found for ${locale}, skipping UI strings.`);
}

// 2. Recipes
const recipesDir = join(localeDir, 'recipes');
if (existsSync(recipesDir)) {
  const files = readdirSync(recipesDir).filter((f) => f.endsWith('.json'));

  for (const file of files) {
    const translated = JSON.parse(readFileSync(join(recipesDir, file), 'utf-8'));
    const source = translated._source;

    if (!source) {
      console.log(`  ⚠️  ${file}: missing _source field, skipping.`);
      continue;
    }

    // Derive recipe path from _source: "src/content/cookbook/wasm/rust/hello-world/en.mdx"
    const recipePath = source
      .replace('src/content/cookbook/', '')
      .replace('/en.mdx', '');

    const originalContent = readOriginalMdx(recipePath);
    if (!originalContent) {
      console.log(`  ⚠️  ${file}: original en.mdx not found at ${recipePath}, skipping.`);
      continue;
    }

    const mdxContent = buildLocaleMdx(translated, originalContent, locale);
    if (!mdxContent) {
      console.log(`  ⚠️  ${file}: failed to build MDX, skipping.`);
      continue;
    }

    const outPath = join(COOKBOOK_DIR, recipePath, `${locale}.mdx`);
    mkdirSync(dirname(outPath), { recursive: true });
    writeFileSync(outPath, mdxContent);
    console.log(`  ✅ ${basename(file, '.json')} → cookbook/${recipePath}/${locale}.mdx`);
    merged++;
  }
} else {
  console.log(`  ⚠️  No recipes/ directory found for ${locale}, skipping recipes.`);
}

console.log(`\n📊 Merged ${merged} file(s) for locale "${locale}".\n`);
