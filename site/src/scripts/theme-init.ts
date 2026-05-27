/**
 * Theme initialisation — runs inline in <head> to prevent FOUC.
 *
 * Reads the user's preference from localStorage, falls back to
 * system preference, and sets `data-theme` on <html> before
 * the first paint.
 */

const STORAGE_KEY = 'smiths-net-theme';

type Theme = 'light' | 'dark' | 'system';

function getStoredTheme(): Theme {
  try {
    const stored = localStorage.getItem(STORAGE_KEY);
    if (stored === 'light' || stored === 'dark' || stored === 'system') {
      return stored;
    }
  } catch {
    // localStorage unavailable (private browsing, etc.)
  }
  return 'system';
}

function getEffectiveTheme(preference: Theme): 'light' | 'dark' {
  if (preference === 'system') {
    return window.matchMedia('(prefers-color-scheme: dark)').matches
      ? 'dark'
      : 'light';
  }
  return preference;
}

function applyTheme(preference: Theme) {
  const effective = getEffectiveTheme(preference);
  document.documentElement.setAttribute('data-theme', effective);
  document.documentElement.setAttribute('data-theme-preference', preference);
}

// Apply immediately.
applyTheme(getStoredTheme());

// Re-apply if OS preference changes while "system" is selected.
window
  .matchMedia('(prefers-color-scheme: dark)')
  .addEventListener('change', () => {
    const pref = getStoredTheme();
    if (pref === 'system') {
      applyTheme('system');
    }
  });
