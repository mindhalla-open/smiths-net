/**
 * Internationalization helpers for the cookbook site.
 *
 * Locale files live in `src/i18n/<locale>.json`.  At build time Astro
 * statically imports them so there's zero runtime fetch.  The `t()`
 * helper resolves a dotted key path against the active locale's
 * string map, falling back to `en` when a key is missing.
 */

import en from './en.json';
import ru from './ru.json';
import ar from './ar.json';
import zh from './zh.json';
import es from './es.json';

/** All loaded locale maps, keyed by BCP-47 tag. */
const locales: Record<string, Record<string, unknown>> = { en, ru, ar, zh, es };

/** Supported locale codes. */
export const SUPPORTED_LOCALES = ['en', 'ru', 'es', 'zh', 'ar'] as const;
export type Locale = (typeof SUPPORTED_LOCALES)[number];

/** Default locale used when no prefix is present in the URL. */
export const DEFAULT_LOCALE: Locale = 'en';

/** Locales that use RTL layout. */
export const RTL_LOCALES: ReadonlySet<Locale> = new Set(['ar']);

/** Returns true if the given locale uses right-to-left layout. */
export function isRtl(locale: Locale): boolean {
  return RTL_LOCALES.has(locale);
}

/** Returns `'rtl'` or `'ltr'` for the given locale. */
export function getDir(locale: Locale): 'rtl' | 'ltr' {
  return isRtl(locale) ? 'rtl' : 'ltr';
}

/**
 * Resolve a dotted key path against a locale's string map.
 *
 * ```ts
 * t('en', 'hero.title')   // → "Plugin Cookbook"
 * t('ru', 'hero.title')   // → falls back to en if ru is missing
 * ```
 */
export function t(locale: Locale, key: string): string {
  const map = locales[locale] ?? locales[DEFAULT_LOCALE];
  const fallback = locales[DEFAULT_LOCALE];

  const resolve = (obj: Record<string, unknown>, path: string): string | undefined => {
    const parts = path.split('.');
    let current: unknown = obj;

    for (const part of parts) {
      if (current === null || typeof current !== 'object') return undefined;
      current = (current as Record<string, unknown>)[part];
    }

    return typeof current === 'string' ? current : undefined;
  };

  return resolve(map, key) ?? resolve(fallback!, key) ?? `[missing: ${key}]`;
}

/**
 * Parse a locale from a URL pathname segment.
 * Returns `DEFAULT_LOCALE` when the segment isn't a known locale.
 */
export function parseLocale(segment: string | undefined): Locale {
  if (!segment) return DEFAULT_LOCALE;
  const lower = segment.toLowerCase() as Locale;
  return SUPPORTED_LOCALES.includes(lower) ? lower : DEFAULT_LOCALE;
}

/**
 * Build a path for a different locale of the same page.
 * Used to generate `hreflang` alternate links.
 */
export function localePath(
  currentPath: string,
  targetLocale: Locale,
): string {
  // Strip leading locale prefix if present
  const segments = currentPath.split('/').filter(Boolean);

  if (SUPPORTED_LOCALES.includes(segments[0] as Locale)) {
    segments.shift();
  }

  if (targetLocale === DEFAULT_LOCALE) {
    return '/' + segments.join('/');
  }

  return '/' + targetLocale + '/' + segments.join('/');
}
