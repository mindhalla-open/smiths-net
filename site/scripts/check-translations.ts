#!/usr/bin/env npx tsx
/**
 * check-translations.ts — CI script for C-3.
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

import { readdirSync, existsSync, readFileSync } from 'node:fs';
import { join, basename } from 'node:path';

const COOKBOOK_DIR = join(import.meta.dirname!, '..', 'src', 'content', 'cookbook');
const SUPPORTED_LOCALES = ['en', 'ru', 'es', 'zh', 'ar'] as const;
type Locale = (typeof SUPPORTED_LOCALES)[number];

interface RecipeDir {
  /** e.g. "sidecar/python/hello-world" */
  path: string;
  /** Which locale files exist */
  locales: Set<string>;
}

/** Recursively find recipe directories (those that contain any .mdx file). */
function findRecipeDirs(dir: string, prefix = ''): RecipeDir[] {
  const results: RecipeDir[] = [];

  let entries: string[];
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
      const stat = readdirSync(full); // throws if not a dir
      if (stat) {
        results.push(
          ...findRecipeDirs(full, prefix ? `${prefix}/${entry}` : entry),
        );
      }
    } catch {
      // not a directory, skip
    }
  }

  return results;
}

/** Check that a non-en MDX file has non-empty frontmatter title/description. */
function checkFrontmatter(
  filePath: string,
): { valid: boolean; issues: string[] } {
  const content = readFileSync(filePath, 'utf-8');
  const issues: string[] = [];

  // Extract YAML frontmatter between --- markers
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
const coverage: Record<Locale, { total: number; translated: number }> = {} as any;
for (const locale of SUPPORTED_LOCALES) {
  coverage[locale] = { total: recipes.length, translated: 0 };
}

let hasErrors = false;
const warnings: string[] = [];

console.log(`\n📚 Found ${recipes.length} recipe(s)\n`);

for (const recipe of recipes) {
  // Check en.mdx is present
  if (!recipe.locales.has('en')) {
    console.error(`❌ MISSING en.mdx: ${recipe.path}`);
    hasErrors = true;
  } else {
    coverage.en.translated++;
  }

  // Check non-en locales
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

// Print warnings
if (warnings.length > 0) {
  console.log('\nWarnings:');
  for (const w of warnings) console.log(`  ${w}`);
}

// Print coverage table
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
