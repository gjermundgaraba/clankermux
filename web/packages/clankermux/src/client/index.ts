export { Attachment } from "./attachment.ts";
export type { Action } from "./attachment.ts";
export { Client, connect } from "./client.ts";
export type { ConnectOptions, EventGap, SendInput, WaitOptions } from "./client.ts";
export { ClientError, Connection, hello } from "./connection.ts";
export type { ErrorKind, Transport } from "./connection.ts";
export { Osc52Scanner } from "./osc52.ts";
export type { ClipboardWrite } from "./osc52.ts";
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
