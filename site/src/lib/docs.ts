import fs from 'node:fs';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import { createMarkdownProcessor } from '@astrojs/markdown-remark';
import { parse, renderSync, walkSync, ELEMENT_NODE, TEXT_NODE } from 'ultrahtml';
import { GROUPS, IGNORED } from './docs-config';
import { REPO_ROOT, REPO_URL } from './site';
import { url } from './url';

export interface Heading { depth: number; id: string; text: string }
export interface DocPage {
  slug: string; route: string; label: string; group: string; source: string;
  title: string; description: string; html: string; headings: Heading[];
  ids: Set<string>;
}

import { shikiThemes } from './shiki';

const routeOf = (slug: string) => (slug ? `/docs/${slug}/` : '/docs/');
const posix = (p: string) => p.split(path.sep).join('/');

function textOf(node: any): string {
  if (node.type === TEXT_NODE) return node.value;
  return (node.children || []).map(textOf).join('');
}

function checkRegistry() {
  const listed = new Set(GROUPS.flatMap((g) => g.pages.map((p) => p.source)));
  const ignored = new Set(IGNORED);
  const missing = fs.readdirSync(path.join(REPO_ROOT, 'docs'))
    .filter((f) => f.endsWith('.md'))
    .map((f) => 'docs/' + f)
    .filter((f) => !listed.has(f) && !ignored.has(f));
  if (missing.length) {
    throw new Error(
      `docs page(s) not registered in site/src/lib/docs-config.ts (add to GROUPS or IGNORED): ${missing.join(', ')}`,
    );
  }
  for (const s of listed) {
    if (!fs.existsSync(path.join(REPO_ROOT, s))) throw new Error(`docs-config lists missing file ${s}`);
  }
}

type Pending = { from: string; target: string; anchor: string };

async function build(): Promise<DocPage[]> {
  checkRegistry();
  const processor = await createMarkdownProcessor({
    syntaxHighlight: { type: 'shiki', excludeLangs: [] },
    shikiConfig: { themes: shikiThemes, defaultColor: false, wrap: false },
    smartypants: false,
    gfm: true,
  });

  const published = new Map<string, { slug: string }>();
  for (const g of GROUPS) for (const p of g.pages) published.set(p.source, { slug: p.slug });

  const errors: string[] = [];
  const pending: Pending[] = [];

  /** Returns the rewritten href (null = leave as is). */
  function rewrite(href: string, source: string): string | null {
    if (/^([a-z][a-z0-9+.-]*:|\/\/)/i.test(href)) return null;
    if (href.startsWith('#')) {
      pending.push({ from: source, target: source, anchor: href.slice(1) });
      return null;
    }
    if (href.startsWith('/')) return null;
    const m = href.match(/^([^#?]*)(\?[^#]*)?(#.*)?$/)!;
    const rel = decodeURIComponent(m[1]);
    const hash = m[3] || '';
    const abs = path.resolve(REPO_ROOT, path.dirname(source), rel);
    const inRepo = posix(path.relative(REPO_ROOT, abs));
    if (inRepo.startsWith('..')) { errors.push(`${source}: link leaves the repo: ${href}`); return null; }
    const page = published.get(inRepo);
    if (page) {
      if (hash) pending.push({ from: source, target: inRepo, anchor: hash.slice(1) });
      return url(routeOf(page.slug)) + hash;
    }
    if (!fs.existsSync(abs)) { errors.push(`${source}: broken relative link: ${href}`); return null; }
    const isDir = fs.statSync(abs).isDirectory();
    const clean = inRepo.replace(/\/+$/, '');
    return `${REPO_URL}/${isDir ? 'tree' : 'blob'}/main/${clean}${isDir ? '' : hash}`;
  }

  const pages: DocPage[] = [];
  for (const g of GROUPS) {
    for (const def of g.pages) {
      const file = path.join(REPO_ROOT, def.source);
      const md = fs.readFileSync(file, 'utf8');
      const out = await processor.render(md, { fileURL: pathToFileURL(file) });
      const root = parse(out.code);
      const ids = new Set<string>();
      let title = '';
      const headings: Heading[] = [];
      let description = '';
      const toRemove: any[] = [];

      walkSync(root, (node: any) => {
        if (node.type !== ELEMENT_NODE) return;
        const a = node.attributes;
        if (a.id) ids.add(a.id);
        if (node.name === 'a' && a.name) ids.add(a.name);
        if (node.name === 'a' && a.href) {
          const r = rewrite(a.href, def.source);
          if (r) a.href = r;
          if (/^https?:/.test(a.href)) a.rel = 'noopener';
        }
        if (node.name === 'img' && a.src && !/^([a-z][a-z0-9+.-]*:|\/\/|\/)/i.test(a.src)) {
          // A relative image in the Markdown (it shows on GitHub). The repo
          // logo at the top of the README is left out: the site's nav has it.
          // Any other image must also be in site/public under the same name.
          const inRepo = posix(path.relative(REPO_ROOT, path.resolve(REPO_ROOT, path.dirname(def.source), a.src)));
          const name = path.basename(inRepo);
          if (!fs.existsSync(path.join(REPO_ROOT, inRepo))) errors.push(`${def.source}: broken image: ${a.src}`);
          else if (inRepo.startsWith('logo/')) toRemove.push(node.parent?.name === 'p' && node.parent.children.filter((c: any) => c.type === ELEMENT_NODE || (c.type === TEXT_NODE && c.value.trim())).length === 1 ? node.parent : node);
          else if (fs.existsSync(path.join(REPO_ROOT, 'site/public', name))) a.src = url('/' + name);
          else errors.push(`${def.source}: image ${a.src} is not in site/public`);
        }
        if (/^h[1-6]$/.test(node.name)) {
          const depth = Number(node.name[1]);
          const text = textOf(node).trim();
          if (depth === 1 && !title) { title = text; toRemove.push(node); }
          else if ((depth === 2 || depth === 3) && a.id) {
            headings.push({ depth, id: a.id, text });
            node.children.push(
              ...parse(`<a class="permalink" href="#${a.id}" aria-label="Link to this section" data-pagefind-ignore>#</a>`).children,
            );
          }
        }
        if (node.name === 'p' && !description && node.parent?.type === 0) {
          description = textOf(node).replace(/\s+/g, ' ').trim();
        }
        if (node.name === 'blockquote') {
          const p = node.children.find((c: any) => c.type === ELEMENT_NODE);
          const first = p?.name === 'p' ? p.children.find((c: any) => c.type === ELEMENT_NODE || (c.type === TEXT_NODE && c.value.trim())) : null;
          if (first?.type === ELEMENT_NODE && first.name === 'strong' && /[.:]$/.test(textOf(first).trim())) {
            const kind = textOf(first).trim().replace(/[.:]$/, '').toLowerCase().replace(/[^a-z0-9]+/g, '-');
            a.class = `callout callout-${kind}`;
            a['data-kind'] = kind;
          }
        }
      });

      // Wrap tables (mutating the tree, after the walk).
      const tables: any[] = [];
      walkSync(root, (node: any) => { if (node.type === ELEMENT_NODE && node.name === 'table') tables.push(node); });
      for (const t of tables) {
        const parent = t.parent;
        const wrap = parse('<div class="table-wrap" tabindex="0" role="region" aria-label="Scrollable table"></div>').children[0];
        const i = parent.children.indexOf(t);
        wrap.parent = parent;
        parent.children[i] = wrap;
        t.parent = wrap;
        wrap.children = [t];
      }
      for (const n of toRemove) n.parent.children.splice(n.parent.children.indexOf(n), 1);

      pages.push({
        slug: def.slug, route: routeOf(def.slug), label: def.label, group: g.title, source: def.source,
        title: title || def.label, description: description.slice(0, 200), html: renderSync(root), headings, ids,
      });
    }
  }

  // Anchor validation (warnings only).
  const byPath = new Map(pages.map((p) => [p.source, p]));
  const warnings: string[] = [];
  for (const { from, target, anchor } of pending) {
    if (!anchor) continue;
    const page = byPath.get(target);
    if (!page) continue;
    let a = anchor;
    try { a = decodeURIComponent(anchor); } catch {}
    if (!page.ids.has(a) && !page.ids.has(a.toLowerCase())) {
      warnings.push(`${from}: no heading or id "#${anchor}" on ${target === from ? 'the same page' : target}`);
    }
  }
  const uniq = [...new Set(warnings)];
  fs.mkdirSync(path.join(process.cwd(), '.astro'), { recursive: true });
  fs.writeFileSync(path.join(process.cwd(), '.astro', 'doc-warnings.json'), JSON.stringify(uniq, null, 2));

  if (errors.length) {
    const msg = [...new Set(errors)].join('\n  ');
    if (process.env.LINK_CHECK === 'warn') console.warn('[docs] broken links (LINK_CHECK=warn):\n  ' + msg);
    else throw new Error('Broken relative links in the docs Markdown:\n  ' + msg);
  }
  return pages;
}

let cache: Promise<DocPage[]> | undefined;
export const loadDocs = () => (cache ??= build());

export function sidebar(pages: DocPage[]) {
  return GROUPS.map((g) => ({
    title: g.title,
    pages: g.pages.map((d) => pages.find((p) => p.source === d.source)!),
  }));
}
export const orderedSources = () => GROUPS.flatMap((g) => g.pages.map((p) => p.source));
