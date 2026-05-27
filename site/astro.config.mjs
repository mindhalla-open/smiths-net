// @ts-check
import { defineConfig } from 'astro/config';
import mdx from '@astrojs/mdx';
import sitemap from '@astrojs/sitemap';

// https://astro.build/config
export default defineConfig({
  site: 'https://cookbook.smiths-net.dev',
  output: 'static',

  integrations: [mdx(), sitemap()],

  i18n: {
    defaultLocale: 'en',
    locales: ['en', 'ru', 'es', 'zh', 'ar'],
    routing: {
      prefixDefaultLocale: false,
    },
  },

  markdown: {
    shikiConfig: {
      themes: {
        light: 'github-light',
        dark: 'one-dark-pro',
      },
    },
  },
});
