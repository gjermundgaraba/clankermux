import type { KeyInput } from "@clankermux/engine";
import type { Frame } from "@clankermux/engine";
import type { Profile, Session, Size } from "@clankermux/proto";

/** Messages from the page to the worker. */
export type ToWorker =
  | { type: "init"; wasm: string; url: string; viewport: Size; profile: Profile; session: string | undefined }
  | { type: "attach"; session: string }
  | { type: "create" }
  | { type: "key"; input: KeyInput }
  | { type: "paste"; text: string }
  | { type: "viewport"; size: Size; profile: Profile };

/** Messages from the worker to the page. */
export type ToPage =
  | { type: "status"; text: string }
  | { type: "frame"; frame: Frame }
  | { type: "sessions"; sessions: Session[]; current: string | undefined }
  | { type: "clipboard"; text: string };
