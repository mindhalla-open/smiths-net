#!/usr/bin/env node
/**
 * check-translations.mjs — CI script for C-3.
 *
 * Scans `src/content/cookbook/` for recipe directories and checks
 * translation coverage per locale.
 *
 * Rules:
 *   - Every recipe MUST have an `en.mdx` (error if missing)
 *   - Non-`en` locale files are checked for non-empty title/description
 *   - Reports a table with per-locale coverage percentages
 *
 * Exit code:
 *   0 — all `en.mdx` present (warnings are OK)
 *   1 — at least one `en.mdx` is missing
 */

import { readdirSync, existsSync, readFileSync, statSync } from 'node:fs';
import { join, basename, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const COOKBOOK_DIR = join(__dirname, '..', 'src', 'content', 'cookbook');
const SUPPORTED_LOCALES = ['en', 'ru', 'es', 'zh', 'ar'];

/** Recursively find recipe directories (those that contain any .mdx file). */
function findRecipeDirs(dir, prefix = '') {
  const results = [];

  let entries;
  try {
    entries = readdirSync(dir);
  } catch {
    return results;
  }

  const mdxFiles = entries.filter((e) => e.endsWith('.mdx'));

  if (mdxFiles.length > 0) {
    const locales = new Set(mdxFiles.map((f) => basename(f, '.mdx')));
    results.push({ path: prefix || dir, locales });
  }

  // Recurse into subdirectories
  for (const entry of entries) {
    const full = join(dir, entry);
    try {
      if (statSync(full).isDirectory()) {
        results.push(
          ...findRecipeDirs(full, prefix ? `${prefix}/${entry}` : entry),
        );
      }
    } catch {
      // skip
    }
  }

  return results;
}

/** Check that a non-en MDX file has non-empty frontmatter title/description. */
function checkFrontmatter(filePath) {
  const content = readFileSync(filePath, 'utf-8');
  const issues = [];

  const match = content.match(/^---\n([\s\S]*?)\n---/);
  if (!match) {
    issues.push('no frontmatter found');
    return { valid: false, issues };
  }

  const fm = match[1];
  const titleMatch = fm.match(/^title:\s*["']?(.+?)["']?\s*$/m);
  const descMatch = fm.match(/^description:\s*["']?(.+?)["']?\s*$/m);

  if (!titleMatch || !titleMatch[1].trim()) {
    issues.push('empty or missing title');
  }
  if (!descMatch || !descMatch[1].trim()) {
    issues.push('empty or missing description');
  }

  return { valid: issues.length === 0, issues };
}

// ── Main ──────────────────────────────────────────────

const recipes = findRecipeDirs(COOKBOOK_DIR);

if (recipes.length === 0) {
  console.log('⚠️  No recipe directories found in', COOKBOOK_DIR);
  process.exit(0);
}

// Track coverage per locale
const coverage = {};
for (const locale of SUPPORTED_LOCALES) {
  coverage[locale] = { total: recipes.length, translated: 0 };
}

let hasErrors = false;
const warnings = [];

console.log(`\n📚 Found ${recipes.length} recipe(s)\n`);

for (const recipe of recipes) {
  if (!recipe.locales.has('en')) {
    console.error(`❌ MISSING en.mdx: ${recipe.path}`);
    hasErrors = true;
  } else {
    coverage.en.translated++;
  }

  for (const locale of SUPPORTED_LOCALES) {
    if (locale === 'en') continue;

    if (recipe.locales.has(locale)) {
      const filePath = join(COOKBOOK_DIR, recipe.path, `${locale}.mdx`);
      if (existsSync(filePath)) {
        const { valid, issues } = checkFrontmatter(filePath);
        if (valid) {
          coverage[locale].translated++;
        } else {
          warnings.push(`⚠️  ${recipe.path}/${locale}.mdx: ${issues.join(', ')}`);
        }
      }
    }
  }
}

if (warnings.length > 0) {
  console.log('Warnings:');
  for (const w of warnings) console.log(`  ${w}`);
}

console.log('\n┌─────────┬───────┬────────────┬─────┐');
console.log('│ Locale  │ Total │ Translated │  %  │');
console.log('├─────────┼───────┼────────────┼─────┤');

for (const locale of SUPPORTED_LOCALES) {
  const { total, translated } = coverage[locale];
  const pct = total > 0 ? Math.round((translated / total) * 100) : 0;
  const bar = locale === 'en' && pct === 100 ? '✅' : `${pct}%`;
  console.log(
    `│ ${locale.padEnd(7)} │ ${String(total).padStart(5)} │ ${String(translated).padStart(10)} │ ${bar.padStart(3)} │`,
  );
}

console.log('└─────────┴───────┴────────────┴─────┘\n');

if (hasErrors) {
  console.error('💥 Missing en.mdx files detected — failing CI.');
  process.exit(1);
} else {
  console.log('✅ All recipes have en.mdx. Translation coverage reported above.');
  process.exit(0);
}
