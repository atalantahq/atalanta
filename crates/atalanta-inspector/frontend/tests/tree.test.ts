// Run: node --test tests/*.test.ts  (Node >= 22.18 strips the TypeScript types)
import { test } from "node:test";
import assert from "node:assert/strict";

const { resolveKey, flattenTree, initiallyExpanded, visibleRange } = await import(
  "../src/tree.ts"
);

interface N {
  platform_id: string;
  label: string | null;
  text: string;
  element_type: string;
  bounds: { x: number; y: number; width: number; height: number };
  enabled: boolean;
  visible: boolean;
  children: N[];
  nodeKey?: string;
}

function node(text: string, children: N[] = [], nodeKey?: string): N {
  return {
    platform_id: "",
    label: null,
    text,
    element_type: "View",
    bounds: { x: 0, y: 0, width: 1, height: 1 },
    enabled: true,
    visible: true,
    children,
    nodeKey,
  };
}

test("selection by key re-resolves on a new hierarchy (VEL-S27)", () => {
  const v1 = node("root", [node("a"), node("b", [node("c")])]);
  assert.equal(resolveKey(v1, "0/1/0").text, "c");
  // The refreshed screen changed "c": the selection follows the key.
  const v2 = node("root", [node("a"), node("b", [node("c2")])]);
  assert.equal(resolveKey(v2, "0/1/0").text, "c2");
  // Gone from the screen: no stale element.
  assert.equal(resolveKey(node("root"), "0/1/0"), null);
  assert.equal(resolveKey(v2, "1"), null);
});

test("a 10k-node tree renders only the rows in view (VEL-259)", () => {
  const children = Array.from({ length: 10_000 }, (_, i) => node(`row ${i}`));
  const root = node("root", children);
  const rows = flattenTree(root, initiallyExpanded(root));
  assert.equal(rows.length, 10_001);
  assert.equal(rows[5].key, "0/4");
  const [start, end] = visibleRange(rows.length, 22 * 5000, 600, 22);
  assert.ok(end - start < 60, `${start}..${end}`);
  assert.ok(start <= 5000 && end > 5000 + 600 / 22);
});

test("collapsed subtrees are not flattened, and deep trees don't recurse", () => {
  let deep = node("leaf");
  for (let i = 0; i < 20_000; i++) deep = node("", [deep]);
  const rows = flattenTree(deep, new Set());
  assert.equal(rows.length, 1);
  assert.equal(rows[0].hasChildren, true);
  assert.equal(rows[0].expanded, false);
  const all = new Set<string>();
  let key = "0";
  for (let i = 0; i <= 20_000; i++) {
    all.add(key);
    key += "/0";
  }
  assert.equal(flattenTree(deep, all).length, 20_001);
});
