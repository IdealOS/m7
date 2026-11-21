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

export type Sender = { service: string } | { app: { app: string; conn: number } };

export interface BusError {
  code: string;
  message: string;
}

export type Message =
  | { kind: "request"; id: number; to: string; method: string; params?: unknown; from?: Sender }
  | { kind: "response"; id: number; ok: boolean; result?: unknown; error?: BusError }
  | { kind: "subscribe"; id: number; topic: string }
  | { kind: "unsubscribe"; id: number; topic: string }
  | { kind: "publish"; topic: string; data?: unknown; from?: Sender };
