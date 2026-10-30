import { memo, useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { Element } from "../api/types";
import { useInspectorStore } from "../store/inspectorStore";
import { flattenTree, initiallyExpanded, visibleRange, type Row } from "../tree";

// Fixed row height, so only the rows in view are rendered (VEL-259).
const ROW_HEIGHT = 22;

const TreeRow = memo(function TreeRow({
  row,
  selected,
  onSelect,
  onToggle,
}: {
  row: Row;
  selected: boolean;
  onSelect: (element: Element) => void;
  onToggle: (key: string) => void;
}) {
  const { element, depth, hasChildren, expanded, key } = row;
  const label =
    element.label || element.text || element.platform_id || element.element_type;
  const truncated = label.length > 40 ? label.slice(0, 37) + "..." : label;

  return (
    <div
      className={`tree-row ${selected ? "selected" : ""}`}
      style={{ paddingLeft: depth * 16, height: ROW_HEIGHT, boxSizing: "border-box" }}
      onClick={() => onSelect(element)}
    >
      {hasChildren && (
        <span
          className="toggle"
          onClick={(e) => {
            e.stopPropagation();
            onToggle(key);
          }}
        >
          {expanded ? "▼" : "▶"}
        </span>
      )}
      {!hasChildren && <span className="toggle-spacer" />}
      <span className="type-badge">{element.element_type.split(".").pop()}</span>
      <span className="node-label">{truncated}</span>
    </div>
  );
});

export function ElementTree() {
  const hierarchy = useInspectorStore((s) => s.hierarchy);
  const selectedKey = useInspectorStore((s) => s.selectedKey);
  const setSelectedElement = useInspectorStore((s) => s.setSelectedElement);
  const deviceId = useInspectorStore((s) => s.currentDeviceId);

  // Open/closed state by node key; it starts two levels deep per device.
  const [expandedFor, setExpandedFor] = useState<{
    device: string | null;
    keys: Set<string>;
  } | null>(null);
  const expanded = expandedFor && expandedFor.device === deviceId ? expandedFor.keys : null;
  const open = useMemo(
    () => expanded ?? initiallyExpanded(hierarchy),
    [expanded, hierarchy],
  );
  const rows = useMemo(() => flattenTree(hierarchy, open), [hierarchy, open]);

  const onToggle = useCallback(
    (key: string) =>
      setExpandedFor(() => {
        const next = new Set(open);
        if (next.has(key)) next.delete(key);
        else next.add(key);
        return { device: deviceId, keys: next };
      }),
    [open, deviceId],
  );

  const scrollRef = useRef<HTMLDivElement>(null);
  const [scrollTop, setScrollTop] = useState(0);
  const [viewport, setViewport] = useState(600);
  useEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    const measure = () => setViewport(el.clientHeight || 600);
    measure();
    const ro = typeof ResizeObserver !== "undefined" ? new ResizeObserver(measure) : null;
    ro?.observe(el);
    return () => ro?.disconnect();
  }, [hierarchy]);

  if (!hierarchy) {
    return <div className="element-tree empty">No hierarchy loaded</div>;
  }

  const [start, end] = visibleRange(rows.length, scrollTop, viewport, ROW_HEIGHT);

  return (
    <div className="element-tree">
      <h3>Element Tree</h3>
      <div
        className="tree-scroll"
        ref={scrollRef}
        onScroll={(e) => setScrollTop(e.currentTarget.scrollTop)}
        style={{ overflowY: "auto", maxHeight: "60vh" }}
      >
        <div style={{ height: rows.length * ROW_HEIGHT, position: "relative" }}>
          <div style={{ position: "absolute", top: start * ROW_HEIGHT, left: 0, right: 0 }}>
            {rows.slice(start, end).map((row) => (
              <TreeRow
                key={row.key}
                row={row}
                selected={row.key === selectedKey}
                onSelect={setSelectedElement}
                onToggle={onToggle}
              />
            ))}
          </div>
        </div>
      </div>
    </div>
  );
}
