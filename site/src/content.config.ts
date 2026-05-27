/**
 * Astro Content Collections — schema definitions for all docs sections.
 *
 * Collections:
 * - cookbook: Plugin recipes (WASM, sidecar, script)
 * - guide:   Getting started, configuration, CLI
 * - reference: MCP tools, resources, SIP, WebRTC
 * - operations: Observability, HA, hot-reload, deployment
 * - architecture: Design deep-dives
 */
import { defineCollection, z } from 'astro:content';
import { glob } from 'astro/loaders';

const cookbook = defineCollection({
  loader: glob({
    pattern: '**/*.mdx',
    base: './src/content/cookbook',
  }),
  schema: z.object({
    /** Recipe title, rendered as <h1>. */
    title: z.string(),

    /** One-liner shown in cards and meta description. */
    description: z.string(),

    /** Plugin track: wasm, sidecar, or script. */
    track: z.enum(['wasm', 'sidecar', 'script']),

    /** Programming language within the track. */
    lang: z.string(),

    /** Human-language locale this file is written in. */
    locale: z.string().default('en'),

    /** Optional ordering weight (lower = earlier in listings). */
    order: z.number().default(100),

    /** ISO date of last content update. */
    updatedAt: z.string().optional(),
  }),
});

/** Shared schema for prose-heavy documentation sections. */
const docsSchema = z.object({
  /** Page title, rendered as <h1>. */
  title: z.string(),

  /** One-liner shown in cards and meta description. */
  description: z.string(),

  /** Section grouping key (e.g. "getting-started", "mcp"). */
  section: z.string().optional(),

  /** Human-language locale. */
  locale: z.string().default('en'),

  /** Ordering weight within the section. */
  order: z.number().default(100),

  /** ISO date of last content update. */
  updatedAt: z.string().optional(),
});

const guide = defineCollection({
  loader: glob({
    pattern: '**/*.mdx',
    base: './src/content/guide',
  }),
  schema: docsSchema,
});

const reference = defineCollection({
  loader: glob({
    pattern: '**/*.mdx',
    base: './src/content/reference',
  }),
  schema: docsSchema,
});

const operations = defineCollection({
  loader: glob({
    pattern: '**/*.mdx',
    base: './src/content/operations',
  }),
  schema: docsSchema,
});

const architecture = defineCollection({
  loader: glob({
    pattern: '**/*.mdx',
    base: './src/content/architecture',
  }),
  schema: docsSchema,
});

export const collections = { cookbook, guide, reference, operations, architecture };
