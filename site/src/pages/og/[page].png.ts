// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

import type { APIRoute, GetStaticPaths } from 'astro';
import fs from 'node:fs';
import path from 'node:path';
import { loadDocs } from '../../lib/docs';
import satori from 'satori';
import { Resvg } from '@resvg/resvg-js';
import type { DocPage } from '../../lib/docs';

const WIDTH = 1200;
const HEIGHT = 630;

const FONT_DIR = path.join(process.cwd(), 'node_modules/@fontsource/inter/files');
const inter400 = fs.readFileSync(path.join(FONT_DIR, 'inter-latin-400-normal.woff'));
const inter700 = fs.readFileSync(path.join(FONT_DIR, 'inter-latin-700-normal.woff'));
const FONTS = [
  { data: inter400, name: 'Inter', weight: 400 as const },
  { data: inter700, name: 'Inter', weight: 700 as const },
];
const LOGO = `data:image/png;base64,${fs.readFileSync(
  path.join(process.cwd(), 'public/logo.png'),
  'base64',
)}`;

/** The first sentence if it fits, else whole words up to max with an ellipsis. */
function clip(text: string, max: number): string {
  const first = text.match(/^.*?[.!?](?=\s|$)/)?.[0];
  if (first && first.length <= max) return first;
  if (text.length <= max) return text;
  return text.slice(0, text.lastIndexOf(' ', max)).replace(/[\s,;:]+$/, '') + '…';
}

export const getStaticPaths: GetStaticPaths = async () => {
  const pages = await loadDocs();
  return pages.map((p: DocPage) => ({ params: { page: p.slug || 'index' } }));
};

export const GET: APIRoute = async ({ params }) => {
  const page = (params?.page as string | undefined) ?? 'index';
  const pages = await loadDocs();
  const doc = pages.find((p) => (p.slug || 'index') === page);

  const title = doc?.title || 'Atalanta';
  const subtitle = doc?.description ? clip(doc.description, 110) : undefined;

  const svg = await satori(
    {
      type: 'div',
      props: {
        style: {
          width: WIDTH,
          height: HEIGHT,
          backgroundColor: '#0b0d12',
          color: '#eceef2',
          fontFamily: 'Inter',
          display: 'flex',
          flexDirection: 'column',
          alignItems: 'center',
          justifyContent: 'center',
        },
        children: [
          {
            type: 'img',
            props: {
              src: LOGO,
              style: { width: 64, height: 64, borderRadius: 10, marginBottom: 16 },
            },
          },
          {
            type: 'h1',
            props: {
              style: {
                fontSize: 56,
                fontWeight: 700,
                margin: 0,
                textAlign: 'center',
                lineHeight: 1.15,
                maxWidth: '90%',
              },
              children: title,
            },
          },
          ...(subtitle
            ? [
                {
                  type: 'p',
                  props: {
                    style: {
                      fontSize: 24,
                      color: '#9aa1ad',
                      marginTop: 10,
                      textAlign: 'center',
                      maxWidth: '85%',
                    },
                    children: subtitle,
                  },
                },
              ]
            : []),
        ],
      },
    },
    { width: WIDTH, height: HEIGHT, fonts: FONTS },
  );

  const png = new Resvg(svg).render().asPng();

  return new Response(png as unknown as BodyInit, {
    headers: { 'Content-Type': 'image/png' },
  });
};
