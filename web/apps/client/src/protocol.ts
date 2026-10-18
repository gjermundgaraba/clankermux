import type { Frame, KeyInput } from "@clankermux/engine";
import type { Profile, Session, Size } from "@clankermux/proto";

/** Messages from the page to the worker. */
export type ToWorker =
  | { type: "init"; wasm: string; url: string; viewport: Size; profile: Profile; session: string | undefined }
  | { type: "attach"; session: string }
  | { type: "create" }
  | { type: "key"; input: KeyInput }
  | { type: "paste"; text: string }
  | { type: "viewport"; size: Size; profile: Profile }
  /** The page lost its drawing: send the whole screen again. */
  | { type: "repaint" };

/** Messages from the worker to the page. */
export type ToPage =
  | { type: "status"; text: string }
  | { type: "frame"; frame: Frame }
  | { type: "sessions"; sessions: Session[]; current: string | undefined }
  | { type: "clipboard"; text: string };
