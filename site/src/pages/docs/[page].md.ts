// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

import type { APIRoute } from 'astro';
import { loadDocs } from '../../lib/docs';
import { pageMarkdown } from '../../lib/markdown';

export async function getStaticPaths() {
  const pages = await loadDocs();
  return pages.map((p) => ({ params: { page: p.slug || 'index' } }));
}

export const GET: APIRoute = async ({ params, site }) => {
  const pages = await loadDocs();
  const page = pages.find((p) => (p.slug || 'index') === (params.page as string));
  if (!page) return new Response('not found', { status: 404 });
  return new Response(pageMarkdown(page.source, site ?? new URL('https://atalanta.sh')), {
    headers: { 'Content-Type': 'text/markdown; charset=utf-8' },
  });
};
