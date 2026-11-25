// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

import type { APIRoute } from 'astro';
import { loadDocs } from '../lib/docs';
import { pageMarkdown } from '../lib/markdown';
import { TAGLINE } from '../lib/site';
import { url } from '../lib/url';

const SKIP = new Set(['CHANGELOG.md', 'docs/CHANGELOG-0.2.0.md', 'docs/STATUS.md']);

export const GET: APIRoute = async ({ site }) => {
  const pages = await loadDocs();
  const siteUrl = site ?? new URL('https://atalanta.sh');
  const parts: string[] = [`# Atalanta documentation\n\n> ${TAGLINE}\n\n`];
  for (const p of pages) {
    if (SKIP.has(p.source)) continue;
    const mdUrl = new URL(url(`/docs/${p.slug || 'index'}.md`), siteUrl).href;
    parts.push(`Source: ${mdUrl}\n\n${pageMarkdown(p.source, siteUrl)}`);
  }
  return new Response(parts.join('\n\n---\n\n'), {
    headers: { 'Content-Type': 'text/plain; charset=utf-8' },
  });
};
