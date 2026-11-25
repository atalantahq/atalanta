// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

import fs from 'node:fs';
import path from 'node:path';
import type { APIRoute } from 'astro';
import { REPO_ROOT } from '../lib/site';

export const GET: APIRoute = () =>
  new Response(fs.readFileSync(path.join(REPO_ROOT, 'install.sh'), 'utf8'), {
    headers: { 'Content-Type': 'text/x-shellscript; charset=utf-8' },
  });
