export interface Rect {
  x: number;
  y: number;
  width: number;
  height: number;
}

export interface Element {
  platform_id: string;
  label: string | null;
  text: string | null;
  element_type: string;
  bounds: Rect;
  enabled: boolean;
  visible: boolean;
  children: Element[];
  /** Tree path the server assigned in this snapshot ("0/3/1"). */
  nodeKey?: string;
}

/** A selector in the DSL's flat form, as a suite writes it:
 * `{ text: "Pay" }`, `{ role: "button", name: "Pay", index: 1 }`,
 * `{ text: "Edit", below: { id: "title" } }`. */
export type Selector = {
  id?: string;
  text?: string;
  textContains?: string;
  textMatches?: string;
  idMatches?: string;
  accessibilityId?: string;
  className?: string;
  role?: string;
  name?: string;
  css?: string;
  xpath?: string;
  checked?: boolean;
  focused?: boolean;
  selected?: boolean;
  enabled?: boolean;
  index?: number;
  below?: Selector;
  above?: Selector;
  leftOf?: Selector;
  rightOf?: Selector;
  childOf?: Selector;
  containsChild?: Selector;
};

export type Direction = "up" | "down" | "left" | "right";

export type Platform = "ios" | "android" | "web";

export type DeviceState = "booted" | "shutdown" | "unknown";

export type DeviceType = "physical" | "simulator" | "emulator" | "unknown";

export interface DeviceInfo {
  id: string;
  name: string;
  platform: Platform;
  state: DeviceState;
  os_version: string | null;
  device_type: DeviceType;
}

export interface SelectorCandidate {
  selector: Selector;
  yaml: string;
  /** How many elements on the screen it matches; 1 means unique. */
  matchCount: number;
}

export interface GenerateResponse {
  selector: Selector;
  yaml_tap: string;
  yaml_assert: string;
  yaml_input?: string;
  candidates?: SelectorCandidate[];
}

export interface WsMessage {
  type: "screenshot" | "hierarchy" | "error" | "ok" | "performance";
  url?: string;
  root?: Element;
  /** Snapshot version of a hierarchy; selectors are generated against it. */
  version?: number;
  message?: string;
  // Performance fields (only when type === "performance")
  javaHeapKb?: number;
  nativeHeapKb?: number;
  totalPssKb?: number;
  cpuPercent?: number;
}

/** A single performance sample from the WebSocket stream. */
export interface PerfSample {
  timestamp: number;
  javaHeapKb: number;
  nativeHeapKb: number;
  totalPssKb: number;
  cpuPercent: number;
}

/** A recorded test step with its YAML representation. */
export interface RecordedStep {
  /** Action type for display (tap, inputText, swipe, assertVisible) */
  action: string;
  /** Human-readable label (e.g., element label or swipe direction) */
  label: string;
  /** YAML snippet for this step */
  yaml: string;
  /** Timestamp when recorded */
  timestamp: number;
}

/** Request body for saving a recorded flow. */
export interface SaveFlowRequest {
  name: string;
  app_id: string;
  steps: string[];
  path?: string;
}

export interface SaveFlowResponse {
  path: string;
  yaml: string;
}
