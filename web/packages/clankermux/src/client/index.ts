export { Client, connect } from "./client.ts";
export type { ConnectOptions, EventGap, SendInput, WaitOptions } from "./client.ts";
export { ClientError } from "./connection.ts";
export type { Copy, CopyFailure, CopySource } from "./copy.ts";
export type { Colors, MountOptions, Terminal } from "./mount.ts";
export type { Link, LinkEnd } from "./worker.ts";
export type {
  Acked,
  Attention,
  AttachmentInfo,
  Catalog,
  CreateParams,
  Description,
  ErrorCode,
  Event,
  Exit,
  MethodInfo,
  Methods,
  OnExit,
  Page,
  Peer,
  Read,
  ReadFormat,
  ReadScope,
  Scheme,
  Sent,
  Session,
  SizePolicy,
  StatusRecord,
  StreamCursor,
  Tail,
  Until,
  Waited,
} from "../proto/index.ts";
