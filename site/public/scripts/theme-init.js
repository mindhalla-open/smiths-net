/**
 * Theme init — MUST run synchronously in <head> before first paint.
 * Sets data-theme on <html> from localStorage or system preference.
 */
(function () {
  var STORAGE_KEY = 'smiths-net-theme';
  var stored;
  try { stored = localStorage.getItem(STORAGE_KEY); } catch (e) {}
  if (stored !== 'light' && stored !== 'dark' && stored !== 'system') stored = 'system';

  var effective;
  if (stored === 'system') {
    effective = window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
  } else {
    effective = stored;
  }

  document.documentElement.setAttribute('data-theme', effective);
  document.documentElement.setAttribute('data-theme-preference', stored);
})();
