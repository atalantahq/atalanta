import fs from 'node:fs';
import path from 'node:path';
import type { APIRoute } from 'astro';
import { REPO_ROOT } from '../lib/site';

export const GET: APIRoute = () =>
  new Response(fs.readFileSync(path.join(REPO_ROOT, 'docs', 'llms.txt'), 'utf8'), {
    headers: { 'Content-Type': 'text/plain; charset=utf-8' },
  });
