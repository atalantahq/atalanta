// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

// Pure helpers for the element tree: node keys, flattening and windowing.
// The server puts a `nodeKey` (tree path, "0/3/1") on every node of a
// hierarchy snapshot, and selection is held by key so it survives refreshes.
import type { Element } from "./api/types";

/** The element at `key` in `root`, or null when the screen no longer has it. */
export function resolveKey(root: Element | null, key: string | null): Element | null {
  if (!root || !key) return null;
  const parts = key.split("/");
  if (parts[0] !== "0") return null;
  let node: Element = root;
  for (const part of parts.slice(1)) {
    const child: Element | undefined = node.children[Number(part)];
    if (!child) return null;
    node = child;
  }
  return node;
}

/** The key of `el`: the server's, or its path when an older server sent none. */
export function keyOf(el: Element, path: string): string {
  return el.nodeKey ?? path;
}

export interface Row {
  key: string;
  element: Element;
  depth: number;
  hasChildren: boolean;
  expanded: boolean;
}

/** The rows a tree shows with `expanded` open, in order. Iterative, so a
 * deep tree can't overflow the stack. */
export function flattenTree(root: Element | null, expanded: ReadonlySet<string>): Row[] {
  const rows: Row[] = [];
  if (!root) return rows;
  const stack: { element: Element; depth: number; path: string }[] = [
    { element: root, depth: 0, path: "0" },
  ];
  while (stack.length > 0) {
    const { element, depth, path } = stack.pop()!;
    const key = keyOf(element, path);
    const hasChildren = element.children.length > 0;
    const open = hasChildren && expanded.has(key);
    rows.push({ key, element, depth, hasChildren, expanded: open });
    if (open) {
      for (let i = element.children.length - 1; i >= 0; i--) {
        stack.push({ element: element.children[i], depth: depth + 1, path: `${path}/${i}` });
      }
    }
  }
  return rows;
}

/** Keys of the nodes shallower than `depth`: the tree's initial open state. */
export function initiallyExpanded(root: Element | null, depth = 2): Set<string> {
  const out = new Set<string>();
  if (!root) return out;
  const stack: { element: Element; d: number; path: string }[] = [
    { element: root, d: 0, path: "0" },
  ];
  while (stack.length > 0) {
    const { element, d, path } = stack.pop()!;
    if (d >= depth) continue;
    out.add(keyOf(element, path));
    element.children.forEach((c, i) => stack.push({ element: c, d: d + 1, path: `${path}/${i}` }));
  }
  return out;
}

/** The [start, end) rows to render for a scrolled viewport, with overscan. */
export function visibleRange(
  total: number,
  scrollTop: number,
  viewportHeight: number,
  rowHeight: number,
  overscan = 10,
): [number, number] {
  const first = Math.floor(Math.max(0, scrollTop) / rowHeight);
  const count = Math.ceil(Math.max(0, viewportHeight) / rowHeight);
  return [Math.max(0, first - overscan), Math.min(total, first + count + overscan)];
}
