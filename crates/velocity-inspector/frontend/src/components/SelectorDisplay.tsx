import { useEffect } from "react";
import { generateSelector } from "../api/client";
import { useInspectorStore } from "../store/inspectorStore";

function selectorToString(sel: Record<string, unknown>): string {
  // The flat DSL form: every key counts (`text: "Edit", index: 2`).
  const entries = Object.entries(sel);
  if (entries.length === 0) return "?";
  return entries
    .map(([key, value]) =>
      typeof value === "string" ? `${key}: "${value}"` : `${key}: ${JSON.stringify(value)}`,
    )
    .join(", ");
}

export function SelectorDisplay() {
  // Narrow subscriptions: a new hierarchy with the same selection and version
  // doesn't re-render this or refetch the selector (VEL-259).
  const selectedKey = useInspectorStore((s) => s.selectedKey);
  const selectedPresent = useInspectorStore((s) => s.selectedElement !== null);
  const version = useInspectorStore((s) => s.hierarchyVersion);
  const generated = useInspectorStore((s) => s.generated);
  const setGenerated = useInspectorStore((s) => s.setGenerated);
  const setError = useInspectorStore((s) => s.setError);

  useEffect(() => {
    // Never leave the previous element's selector up while this one loads
    // (or after a 409): Copy would copy the wrong one (review m13).
    setGenerated(null);
    if (!selectedKey || !selectedPresent || version == null) {
      return;
    }
    let cancelled = false;
    generateSelector(selectedKey, version)
      .then((g) => {
        if (!cancelled) setGenerated(g);
      })
      .catch((e) => {
        // 409: a newer hierarchy is on its way, and this reruns with it.
        if (!cancelled && !String(e.message).startsWith("409")) setError(e.message);
      });
    return () => {
      cancelled = true;
    };
  }, [selectedKey, selectedPresent, version, setGenerated, setError]);

  if (!generated) {
    return (
      <div className="selector-display empty">
        Select an element to generate a selector
      </div>
    );
  }

  const copyToClipboard = (text: string) => {
    void navigator.clipboard?.writeText(text);
  };

  return (
    <div className="selector-display">
      <h3>Selector</h3>
      <div className="selector-value">
        <code>{selectorToString(generated.selector as unknown as Record<string, unknown>)}</code>
      </div>
      <h3>YAML Snippets</h3>
      <div className="yaml-block">
        <div className="yaml-header">
          <span>Tap</span>
          <button className="copy-btn" onClick={() => copyToClipboard(generated.yaml_tap)}>
            Copy
          </button>
        </div>
        <pre>{generated.yaml_tap}</pre>
      </div>
      <div className="yaml-block">
        <div className="yaml-header">
          <span>Assert Visible</span>
          <button className="copy-btn" onClick={() => copyToClipboard(generated.yaml_assert)}>
            Copy
          </button>
        </div>
        <pre>{generated.yaml_assert}</pre>
      </div>
      {generated.yaml_input && (
        <div className="yaml-block">
          <div className="yaml-header">
            <span>Input Text</span>
            <button className="copy-btn" onClick={() => copyToClipboard(generated.yaml_input!)}>
              Copy
            </button>
          </div>
          <pre>{generated.yaml_input}</pre>
        </div>
      )}
      {generated.candidates && generated.candidates.length > 0 && (
        <>
          <h3>Candidates</h3>
          <ul className="selector-candidates">
            {generated.candidates.map((c) => (
              <li key={c.yaml} className={c.matchCount === 1 ? "unique" : "ambiguous"}>
                <code>{selectorToString(c.selector as unknown as Record<string, unknown>)}</code>
                <span className="match-count" title="elements on screen it matches">
                  {c.matchCount === 1 ? "unique" : `${c.matchCount} matches`}
                </span>
                <button className="copy-btn" onClick={() => copyToClipboard(c.yaml)}>
                  Copy
                </button>
              </li>
            ))}
          </ul>
        </>
      )}
    </div>
  );
}
