import fs from 'node:fs';
import path from 'node:path';
import type { APIRoute } from 'astro';
import { loadDocs } from '../lib/docs';
import { REPO_ROOT } from '../lib/site';

export const GET: APIRoute = async () => {
  const pages = await loadDocs();
  const slugs = new Set(pages.map((p) => p.slug || 'index'));
  const text = fs.readFileSync(path.join(REPO_ROOT, 'docs', 'llms.txt'), 'utf8');
  const bad: string[] = [];
  for (const m of text.matchAll(/https:\/\/atalanta\.sh\/docs\/([^)]+?)\.md/g)) {
    const x = m[1];
    if (!slugs.has(x)) bad.push(`https://atalanta.sh/docs/${x}.md`);
  }
  if (bad.length) throw new Error(`docs/llms.txt links to unknown pages: ${bad.join(', ')}`);
  return new Response(text, {
    headers: { 'Content-Type': 'text/plain; charset=utf-8' },
  });
};
