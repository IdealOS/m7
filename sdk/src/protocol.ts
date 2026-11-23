// Wire types mirrored from crates/protocol. Hand-written for now; M2 replaces
// this file with types generated from the Rust definitions' JSON Schema.

export type NodeId = number;

export type NodeKind =
  | "group" | "rect" | "text" | "image"
  | "vstack" | "hstack" | "label" | "button";

export type Props = Record<string, unknown>;

export type SceneOp =
  | { op: "create"; id: NodeId; type: NodeKind; props?: Props }
  | { op: "set"; id: NodeId; props: Props }
  | { op: "insert"; parent: NodeId; child: NodeId; index: number }
  | { op: "remove"; parent: NodeId; child: NodeId }
  | { op: "destroy"; id: NodeId }
  | { op: "listen"; id: NodeId; events: string[] };

export interface Commit {
  window: number;
  ops: SceneOp[];
}

/** Stamped by the broker. `conn` is a trusted local connection (a tool, or a
 * service before it claims a name). */
export type Sender = { service: string } | { app: { app: string; conn: number } } | { conn: number };

/** The broker's own name: `hello`, `claim`, `release` and `list` go to it. */
export const BROKER = "bus";
/** The broker's trace of every message; only trusted connections may subscribe. */
export const TAP_TOPIC = "bus.tap";
export const DEFAULT_TIMEOUT_MS = 30_000;

/** Error codes the broker itself sends. Services may send their own. */
export type BrokerErrorCode =
  | "bad_request" | "no_such_service" | "name_taken"
  | "permission_denied" | "timeout" | "service_gone";

export interface BusError {
  code: string;
  message: string;
}

export type Message =
  | { kind: "request"; id: number; to: string; method: string; params?: unknown; timeout_ms?: number; from?: Sender }
  | { kind: "response"; id: number; ok: boolean; result?: unknown; error?: BusError }
  | { kind: "cancel"; id: number }
  | { kind: "subscribe"; id: number; topic: string }
  | { kind: "unsubscribe"; id: number; topic: string }
  | { kind: "publish"; topic: string; data?: unknown; from?: Sender };
