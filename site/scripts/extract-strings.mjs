#!/usr/bin/env node
/**
 * extract-strings.mjs — F-1 deliverable 1.
 *
 * Extracts translatable content from:
 *   1. `src/i18n/en.json`   → UI strings (flat key-value pairs)
 *   2. `src/content/cookbook/ ** /en.mdx` → recipe prose (frontmatter + body)
 *
 * Output format: JSON files suitable for Crowdin / Weblate ingestion.
 *
 * Usage:
 *   node scripts/extract-strings.mjs [--out-dir ./l10n]
 *
 * Output structure:
 *   <out-dir>/
 *     ui-strings.json           — flattened UI key-value pairs
 *     recipes/
 *       wasm--rust--hello-world.json
 *       sidecar--python--hello-world.json
 *       ...
 */

import { readdirSync, readFileSync, writeFileSync, mkdirSync, statSync } from 'node:fs';
import { join, dirname, relative, basename } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const SITE_ROOT = join(__dirname, '..');
const I18N_DIR = join(SITE_ROOT, 'src', 'i18n');
const COOKBOOK_DIR = join(SITE_ROOT, 'src', 'content', 'cookbook');

// Parse CLI args
const args = process.argv.slice(2);
let outDir = join(SITE_ROOT, 'l10n');
const outIdx = args.indexOf('--out-dir');
if (outIdx >= 0 && args[outIdx + 1]) {
  outDir = join(process.cwd(), args[outIdx + 1]);
}

// ── Helpers ──────────────────────────────────────────────

/** Flatten a nested object into dotted key paths. */
function flatten(obj, prefix = '') {
  const result = {};
  for (const [key, value] of Object.entries(obj)) {
    const fullKey = prefix ? `${prefix}.${key}` : key;
    if (typeof value === 'object' && value !== null && !Array.isArray(value)) {
      Object.assign(result, flatten(value, fullKey));
    } else if (typeof value === 'string') {
      result[fullKey] = value;
    }
  }
  return result;
}

/** Extract frontmatter fields from an MDX file. */
function extractFrontmatter(content) {
  const match = content.match(/^---\n([\s\S]*?)\n---/);
  if (!match) return { frontmatter: {}, body: content };

  const fmBlock = match[1];
  const fm = {};
  for (const line of fmBlock.split('\n')) {
    const eq = line.indexOf(':');
    if (eq < 0) continue;
    const key = line.slice(0, eq).trim();
    let val = line.slice(eq + 1).trim();
    // Remove quotes
    if ((val.startsWith('"') && val.endsWith('"')) || (val.startsWith("'") && val.endsWith("'"))) {
      val = val.slice(1, -1);
    }
    fm[key] = val;
  }

  const body = content.slice(match[0].length).trim();
  return { frontmatter: fm, body };
}

/**
 * Extract translatable prose from MDX body.
 * Strips code blocks (``` ... ```) and import statements.
 * Returns an array of text segments.
 */
function extractProse(body) {
  // Remove code fences
  let stripped = body.replace(/```[\s\S]*?```/g, '{{CODE_BLOCK}}');
  // Remove inline code
  stripped = stripped.replace(/`[^`]+`/g, '{{INLINE_CODE}}');
  // Remove import statements
  stripped = stripped.replace(/^import\s+.*$/gm, '');
  // Remove JSX component tags
  stripped = stripped.replace(/<[A-Z][a-zA-Z]*\s*[^>]*\/>/g, '');

  // Split into paragraphs
  const paragraphs = stripped
    .split(/\n{2,}/)
    .map((p) => p.trim())
    .filter((p) => p.length > 0 && p !== '{{CODE_BLOCK}}');

  return paragraphs;
}

/** Recursively find all en.mdx files. */
function findEnMdx(dir) {
  const results = [];
  let entries;
  try {
    entries = readdirSync(dir);
  } catch {
    return results;
  }

  for (const entry of entries) {
    const full = join(dir, entry);
    try {
      const stat = statSync(full);
      if (stat.isDirectory()) {
        results.push(...findEnMdx(full));
      } else if (entry === 'en.mdx') {
        results.push(full);
      }
    } catch {
      // skip
    }
  }
  return results;
}

// ── Main ────────────────────────────────────────────────

console.log('📦 Extracting translatable strings...\n');

// 1. UI strings
const enJson = JSON.parse(readFileSync(join(I18N_DIR, 'en.json'), 'utf-8'));
const flatStrings = flatten(enJson);
const uiCount = Object.keys(flatStrings).length;

mkdirSync(outDir, { recursive: true });
writeFileSync(
  join(outDir, 'ui-strings.json'),
  JSON.stringify(flatStrings, null, 2) + '\n',
);
console.log(`  ✅ UI strings: ${uiCount} keys → l10n/ui-strings.json`);

// 2. Recipe prose
const recipesDir = join(outDir, 'recipes');
mkdirSync(recipesDir, { recursive: true });

const mdxFiles = findEnMdx(COOKBOOK_DIR);
let recipeCount = 0;

for (const mdxPath of mdxFiles) {
  const relPath = relative(COOKBOOK_DIR, dirname(mdxPath));
  const recipeId = relPath.replace(/\//g, '--');

  const content = readFileSync(mdxPath, 'utf-8');
  const { frontmatter, body } = extractFrontmatter(content);
  const prose = extractProse(body);

  const extracted = {
    _source: `src/content/cookbook/${relPath}/en.mdx`,
    _note: 'Only translate "title", "description", and "prose" values. Leave "_source" and "_note" unchanged.',
    title: frontmatter.title || '',
    description: frontmatter.description || '',
    prose,
  };

  writeFileSync(
    join(recipesDir, `${recipeId}.json`),
    JSON.stringify(extracted, null, 2) + '\n',
  );
  recipeCount++;
}

console.log(`  ✅ Recipes: ${recipeCount} files → l10n/recipes/`);
console.log(`\n📊 Total: ${uiCount} UI strings + ${recipeCount} recipes extracted.`);
console.log(`   Output: ${relative(process.cwd(), outDir)}/\n`);
