// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

import fs from 'node:fs';
import path from 'node:path';
import { REPO_ROOT } from './site';
import { resolveHref } from './docs';
import { url } from './url';

const fence = /^(\s*)(`{3,}|~{3,})(\S*)(\s*.*)$/;

export function pageMarkdown(source: string, site: URL): string {
  const file = path.join(REPO_ROOT, source);
  const dir = path.dirname(source);
  const lines = fs.readFileSync(file, 'utf8').split('\n');
  const out: string[] = [];
  let inFence = false;
  let fenceChar = '';

  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    if (fence.test(line) && (!inFence || line.trimStart().startsWith(fenceChar))) {
      inFence = !inFence;
      fenceChar = line.match(fence)![2];
      out.push(line);
      continue;
    }
    if (inFence) { out.push(line); continue; }
    out.push(rewriteSegment(line, dir, source, site));
  }
  return out.join('\n');
}

function rewriteSegment(seg: string, dir: string, source: string, site: URL): string {
  // Protect inline code spans so link/image rewriting isn't broken by backticks
  // inside link text (e.g. [`http` step](DSL.md#http-and-api-tests)).
  const codeSpans: string[] = [];
  seg = seg.replace(/`[^`]*`/g, (m) => {
    codeSpans.push(m);
    return `\0CODE${codeSpans.length - 1}\0`;
  });

  seg = seg.replace(/!\[([^\]]*?)\]\(((?:[^()\\]|\\(?:.))*?)(?:\s+"[^"]*")?\)/g, (m, _alt, src) => {
    const r = resolveHref(src, source);
    if (r.kind === 'external' || r.kind === 'root') return m;
    if (r.kind === 'anchor') return m;
    if (isLogo(src, dir)) return '';
    return `![${_alt}](${new URL(url('/' + path.basename(src)), site).href})`;
  });
  // Only the target matters, so match `](href)` alone: link text may hold an image.
  seg = seg.replace(/\]\(((?:[^()\s\\]|\\.)+)(\s+"[^"]*")?\)/g, (m, href, title = '') => {
    const u = hrefOf(resolveHref(href, source), source, site);
    return u === null ? m : `](${u}${title})`;
  });
  seg = seg.replace(/^\[([^\]]+)\]:\s*(\S+)(?:\s+"[^"]*")?\s*$/m, (m, id, href) => {
    const r = resolveHref(href, source);
    const u = hrefOf(r, source, site);
    if (u === null) return m;
    return `[${id}]: ${u}`;
  });
  seg = seg.replace(/<(?:a|img)\b[^>]*?(?:href|src)=(["'])(.*?)\1[^>]*>/gi, (m, quote, v) => {
    const attr = /(href|src)=/i.exec(m)![1];
    const isImg = /^<img\b/i.test(m);
    const r = resolveHref(v, source);
    if (isImg) {
      if (r.kind === 'external' || r.kind === 'root') return m;
      if (r.kind === 'anchor') return m;
      if (isLogo(v, dir)) return '';
      return m.replace(new RegExp(`(${attr}=)${quote}[^${quote}]*${quote}`, 'i'), `$1${quote}${new URL(url('/' + path.basename(v)), site).href}${quote}`);
    }
    const u = hrefOf(r, source, site);
    if (u === null) return m;
    return m.replace(new RegExp(`(${attr}=)${quote}[^${quote}]*${quote}`, 'i'), `$1${quote}${u}${quote}`);
  });

  seg = seg.replace(/\0CODE(\d+)\0/g, (_, n) => codeSpans[+n]);
  return seg;
}

function hrefOf(r: ReturnType<typeof resolveHref>, source: string, site: URL): string | null {
  switch (r.kind) {
    case 'external': case 'root': return null;
    case 'anchor': return '#' + r.hash!;
    case 'page': return new URL(url('/docs/' + (r.slug || 'index') + '.md'), site).href + (r.hash || '');
    case 'file': return r.url!;
    case 'error': return null;
  }
}

function isLogo(href: string, dir: string): boolean {
  const m = href.match(/^([^#?]*)/);
  const rel = decodeURIComponent(m![1]);
  const abs = path.resolve(REPO_ROOT, dir, rel);
  const inRepo = posix(path.relative(REPO_ROOT, abs));
  return inRepo.startsWith('logo/');
}

const posix = (p: string) => p.split(path.sep).join('/');
