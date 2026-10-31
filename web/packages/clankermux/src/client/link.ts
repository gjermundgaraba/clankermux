import { type FSM, type FSMSnapshot, setup, types } from "xstate/fsm";

import type { ClosedReason, ErrorCode, ErrorMessage, Exit, Session } from "../proto/index.ts";

import type { Attachment } from "./attachment.ts";
import { ClientError, type Connection } from "./connection.ts";

/** Why the link to the session ended. `closed`: the connection closed
 * (not a `CLOSED` frame, which the worker does not wait for when it
 * detaches); `removed`, `stopping` and `detached` (another client's
 * `detach`): the server's `CLOSED`; `exited`:
 * the session's program exited; `no_such_session` and `engine_mismatch`:
 * an `OPEN` refused; `error`: any other error, the server's or the
 * client's own. */
export type LinkEnd =
  | "closed"
  | "removed"
  | "stopping"
  | "detached"
  | "exited"
  | "no_such_session"
  | "engine_mismatch"
  | "error";

/** A terminal's link to its session, through the client's connection and
 * the terminal's attachment. */
export type Link =
  /** Opening a connection; `attempt` counts from 1 and starts again once
   * a server answers. */
  | { state: "connecting"; attempt: number }
  /** Connected, and showing `session` as `ATTACHED` described it (its
   * `status` may be `restored` or `exited`), or attached to none. */
  | { state: "connected"; session: Session | undefined }
  /** The link ended. `code` and `message` are the error's, when there is
   * one, and `message` the reason a `detach` gave; `exit` is the
   * program's, for `exited`. `retry`: the worker
   * reconnects or attaches again by itself, so `connecting` or
   * `connected` follows; otherwise nothing does until the page attaches. */
  | {
      state: "ended";
      reason: LinkEnd;
      code?: ErrorCode;
      message?: string;
      exit?: Exit;
      retry: boolean;
    };

const UNRETRYABLE_OPEN_ERRORS = new Set<ErrorMessage["code"]>(["no_such_session", "engine_mismatch", "invalid_request"]);

function linkEnd(code: ErrorCode): LinkEnd {
  return code === "no_such_session" || code === "engine_mismatch" ? code : "error";
}

export function endedWith(err: unknown, retry: boolean): Link {
  const server = err instanceof ClientError ? err.server : undefined;
  return server
    ? { state: "ended", reason: linkEnd(server.code), code: server.code, message: server.message, retry }
    : { state: "ended", reason: "error", message: err instanceof Error ? err.message : String(err), retry };
}

function closedWith(reason: ClosedReason, message: string | undefined): Link {
  return { state: "ended", reason, ...(message !== undefined && { message }), retry: reason === "stopping" };
}

export interface Open {
  conn: Connection;
  channel: number;
  fresh: Attachment;
}

export interface Line {
  session: string;
  attachment: Attachment;
  channel: number;
  conn: Connection;
}

const CLEARED = {
  gaveUp: undefined,
  live: undefined,
  session: undefined,
  attachment: undefined,
  channel: undefined,
  conn: undefined,
  described: undefined,
  exit: undefined,
  reason: undefined,
  message: undefined,
  ended: undefined,
};

type Only<T> = T & { [K in Exclude<keyof typeof CLEARED, keyof T>]?: undefined };

interface Contexts {
  none: Only<{ gaveUp: Link | undefined; live: boolean }>;
  waiting: Only<{ session: string; attachment: Attachment | undefined }>;
  opening: Only<Line>;
  attached: Only<Line & { described: Session | undefined }>;
  exited: Only<Line & { exit: Exit | undefined }>;
  closed: Only<Line & { reason: "stopping" | "closed"; message: string | undefined }>;
  backoff: Only<{ session: string; ended: Link }>;
  disposed: Only<{}>;
}

interface Events {
  "hub.live": { open: Open };
  "hub.down": {};
  "page.attach": { session: string; open: Open | undefined };
  "page.detach": { live: boolean };
  "page.unmount": {};
  "frame.screen": { session: Session | undefined };
  "frame.resumed": { session: Session | undefined };
  "frame.exited": { exit: Exit | undefined };
  "frame.closed": { reason: ClosedReason; message: string | undefined };
  "frame.error": { error: ErrorMessage; endsAttachment: boolean };
  "attachment.threw": { error: unknown };
  "reopen.due": { open: Open | undefined };
}

export type MountSnapshot = { [K in keyof Contexts]: FSMSnapshot<Contexts[K], K> }[keyof Contexts];
export type MountEvent = { [K in keyof Events]: { type: K } & Events[K] }[keyof Events];

const none = (gaveUp: Link | undefined, live: boolean) => ({ ...CLEARED, gaveUp, live });
const waiting = (session: string, attachment: Attachment | undefined) => ({ ...CLEARED, session, attachment });
const line = ({ session, attachment, channel, conn }: Line) => ({ ...CLEARED, session, attachment, channel, conn });
const opening = (session: string, attachment: Attachment, { conn, channel }: Open) =>
  line({ session, attachment, channel, conn });
const backoff = (session: string, ended: Link) => ({ ...CLEARED, session, ended });

const attach = ({ event: { session, open } }: { event: Events["page.attach"] }) =>
  open
    ? { target: "opening" as const, context: opening(session, open.fresh, open) }
    : { target: "waiting" as const, context: waiting(session, undefined) };
const detach = ({ event }: { event: Events["page.detach"] }) => ({ target: "none" as const, context: none(undefined, event.live) });
const unmount = { target: "disposed" as const, context: CLEARED };
const lost = ({ context }: { context: Line }) => ({ target: "waiting" as const, context: waiting(context.session, context.attachment) });
const screen = ({ context, event }: { context: Line; event: Events["frame.screen"] }) => ({
  target: "attached" as const,
  context: { ...line(context), described: event.session },
});
const closed = ({ context, event: { reason, message } }: { context: Line; event: Events["frame.closed"] }) =>
  reason === "removed" || reason === "detached"
    ? { target: "none" as const, context: none(closedWith(reason, message), true) }
    : { target: "closed" as const, context: { ...line(context), reason, message } };
const refused = ({ context, event }: { context: Line; event: Events["frame.error"] }) => {
  if (!event.endsAttachment) return undefined;
  const { code, message } = event.error;
  const retry = !UNRETRYABLE_OPEN_ERRORS.has(code);
  const ended: Link = { state: "ended", reason: linkEnd(code), code, message, retry };
  return retry
    ? { target: "backoff" as const, context: backoff(context.session, ended) }
    : { target: "none" as const, context: none(ended, true) };
};
const threw = ({ context, event }: { context: Line; event: Events["attachment.threw"] }) => {
  const desync = event.error instanceof ClientError && event.error.kind === "desync";
  const ended = endedWith(event.error, desync);
  return desync
    ? { target: "backoff" as const, context: backoff(context.session, ended) }
    : { target: "none" as const, context: none(ended, true) };
};

export const mountMachine: FSM<{}, MountEvent, keyof Contexts, MountSnapshot, unknown> = setup({
  schemas: {
    events: {
      "hub.live": types<Events["hub.live"]>(),
      "hub.down": types<Events["hub.down"]>(),
      "page.attach": types<Events["page.attach"]>(),
      "page.detach": types<Events["page.detach"]>(),
      "page.unmount": types<Events["page.unmount"]>(),
      "frame.screen": types<Events["frame.screen"]>(),
      "frame.resumed": types<Events["frame.resumed"]>(),
      "frame.exited": types<Events["frame.exited"]>(),
      "frame.closed": types<Events["frame.closed"]>(),
      "frame.error": types<Events["frame.error"]>(),
      "attachment.threw": types<Events["attachment.threw"]>(),
      "reopen.due": types<Events["reopen.due"]>(),
    },
  },
  states: {
    none: { schemas: { context: types<Contexts["none"]>() } },
    waiting: { schemas: { context: types<Contexts["waiting"]>() } },
    opening: { schemas: { context: types<Contexts["opening"]>() } },
    attached: { schemas: { context: types<Contexts["attached"]>() } },
    exited: { schemas: { context: types<Contexts["exited"]>() } },
    closed: { schemas: { context: types<Contexts["closed"]>() } },
    backoff: { schemas: { context: types<Contexts["backoff"]>() } },
    disposed: { schemas: { context: types<Contexts["disposed"]>() } },
  },
}).createFSM({
  initial: "none",
  context: none(undefined, false),
  states: {
    none: {
      on: {
        "hub.live": { target: "none", context: none(undefined, true) },
        "hub.down": { target: "none", context: none(undefined, false) },
        "page.attach": attach,
        "page.unmount": unmount,
      },
    },
    waiting: {
      on: {
        "hub.live": ({ context, event: { open } }) => ({
          target: "opening",
          context: opening(context.session, context.attachment ?? open.fresh, open),
        }),
        "page.attach": attach,
        "page.detach": detach,
        "page.unmount": unmount,
      },
    },
    opening: {
      on: {
        "frame.screen": screen,
        "frame.resumed": screen,
        "frame.closed": closed,
        "frame.error": refused,
        "attachment.threw": threw,
        "hub.down": lost,
        "page.attach": attach,
        "page.detach": detach,
        "page.unmount": unmount,
      },
    },
    attached: {
      on: {
        "frame.screen": screen,
        "frame.exited": ({ context, event: { exit } }) => ({ target: "exited", context: { ...line(context), exit } }),
        "frame.closed": closed,
        "frame.error": refused,
        "attachment.threw": threw,
        "hub.down": lost,
        "page.attach": attach,
        "page.detach": detach,
        "page.unmount": unmount,
      },
    },
    exited: {
      on: {
        "frame.screen": screen,
        "frame.closed": closed,
        "frame.error": refused,
        "attachment.threw": threw,
        "hub.down": lost,
        "page.attach": attach,
        "page.detach": detach,
        "page.unmount": unmount,
      },
    },
    closed: {
      on: {
        "hub.down": lost,
        "page.attach": attach,
        "page.detach": detach,
        "page.unmount": unmount,
      },
    },
    backoff: {
      on: {
        "reopen.due": ({ context, event: { open } }) =>
          open
            ? { target: "opening", context: opening(context.session, open.fresh, open) }
            : { target: "waiting", context: waiting(context.session, undefined) },
        "hub.live": ({ context, event: { open } }) => ({
          target: "opening",
          context: opening(context.session, open.fresh, open),
        }),
        "page.attach": attach,
        "page.detach": detach,
        "page.unmount": unmount,
      },
    },
    disposed: {},
  },
});

export function lineOf(snapshot: MountSnapshot): Line | undefined {
  switch (snapshot.value) {
    case "opening":
    case "attached":
    case "exited":
    case "closed":
      return snapshot.context;
    default:
      return undefined;
  }
}

export function linkOf(snapshot: MountSnapshot): Link | undefined {
  switch (snapshot.value) {
    case "none":
      return snapshot.context.gaveUp ?? (snapshot.context.live ? { state: "connected", session: undefined } : undefined);
    case "attached":
      return { state: "connected", session: snapshot.context.described };
    case "exited": {
      const { exit } = snapshot.context;
      return { state: "ended", reason: "exited", ...(exit && { exit }), retry: false };
    }
    case "closed":
      return closedWith(snapshot.context.reason, snapshot.context.message);
    case "backoff":
      return snapshot.context.ended;
    default:
      return undefined;
  }
}
