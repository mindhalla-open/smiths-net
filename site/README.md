# Smiths-Net Plugin Cookbook

The official cookbook of plugin recipes for [Smiths-Net](https://github.com/friday-mindhalla/smiths-net) — the open-source telephony engine.

## Development

```bash
npm install
npm run dev       # Start dev server at localhost:4321
npm run build     # Build static site to ./dist
npm run preview   # Preview production build locally
```

## Structure

```
src/
├── components/     # Reusable Astro components
├── content/
│   └── cookbook/    # MDX recipe content
│       ├── wasm/       # WASM plugin track
│       ├── sidecar/    # Sidecar plugin track
│       └── script/     # Script plugin track
├── i18n/           # Internationalization
├── layouts/        # Page layouts
├── pages/          # Route pages
└── styles/         # CSS design system
```

## Adding a recipe

1. Create a directory under `src/content/cookbook/<track>/<lang>/<recipe-name>/`
2. Add an `en.mdx` file with frontmatter:
   ```yaml
   ---
   title: "Recipe Title"
   description: "One-liner for cards and SEO."
   track: wasm        # or sidecar, script
   lang: rust          # programming language
   locale: en
   order: 10           # sort weight (lower = first)
   ---
   ```
3. Write the recipe content in MDX
4. The page will appear automatically at `/<track>/<lang>/<recipe-name>/`

## Translation Workflow

### Overview

The site supports 5 locales: `en` (default), `ru`, `es`, `zh`, `ar`.
Translation uses an **extract → translate → merge** pipeline.

```
en.json + en.mdx ──→ extract ──→ l10n/ (source JSON) ──→ Crowdin/Weblate
                                                              │
l10n/<locale>/ ←────── download ←──── translated JSON ←───────┘
       │
       └──→ merge ──→ src/i18n/<locale>.json + <locale>.mdx ──→ build
```

### Quick start (manual translation)

```bash
# 1. Extract source strings
node scripts/extract-strings.mjs

# 2. Copy source files for your locale
cp -r l10n/ l10n/ru/    # or es, zh, ar
#    └── l10n/ru/ui-strings.json
#    └── l10n/ru/recipes/*.json

# 3. Translate the JSON values in l10n/ru/
#    - ui-strings.json: translate all string values
#    - recipes/*.json:  translate "title", "description", "prose" arrays
#    - Do NOT change keys, _source, or _note fields

# 4. Merge back into the site
node scripts/merge-translations.mjs ru

# 5. Verify build
npm run build

# 6. Check coverage
node scripts/check-translations.mjs
```

### Crowdin integration

The project includes a `crowdin.yml` at the repo root.

```bash
# Upload source strings
crowdin upload sources

# Download translations
crowdin download translations

# Merge each locale
node scripts/merge-translations.mjs ru
node scripts/merge-translations.mjs es
```

### File structure

```
l10n/                          # Extracted source (gitignored)
├── ui-strings.json            # Flattened UI keys from en.json
└── recipes/                   # Per-recipe JSON files
    ├── wasm--rust--hello-world.json
    └── ...

l10n/<locale>/                 # Translated files
├── ui-strings.json
└── recipes/
    └── ...
```

### Scripts

| Script | Purpose |
|--------|---------|
| `scripts/extract-strings.mjs` | Extract source strings to `l10n/` |
| `scripts/merge-translations.mjs <locale>` | Merge translations back into site |
| `scripts/check-translations.mjs` | CI gate — check coverage per locale |

### Rules

- Every recipe MUST have `en.mdx` (CI enforced)
- Code blocks inside MDX are preserved during merge (never translated)
- UI strings use dotted keys (e.g., `hero.title`)
- Untranslated recipes show a "not translated yet" banner

## License

Apache-2.0
