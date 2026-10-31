import { expect, test } from "vitest";

import type { Session } from "../../src/proto/index.ts";

import type { Attachment } from "../../src/client/attachment.ts";
import { ClientError, type Connection } from "../../src/client/connection.ts";
import { type Link, type MountEvent, type MountSnapshot, type Open, linkOf, mountMachine } from "../../src/client/link.ts";
import { session } from "./bodies.ts";

let channels = 0;
function offer(): Open {
  return { conn: {} as Connection, channel: ++channels, fresh: {} as Attachment };
}

function run(...events: MountEvent[]): MountSnapshot {
  return events.reduce((snapshot, event) => mountMachine.transition(snapshot, event)[0], mountMachine.initialState);
}

const attached = (s: Session = session("s")): MountEvent[] => [
  { type: "hub.live", open: offer() },
  { type: "page.attach", session: s.id, open: offer() },
  { type: "frame.screen", session: s },
];
const retryable: MountEvent = { type: "frame.error", error: { code: "internal", message: "later" }, endsAttachment: true };

test("a reopen timer that fires after a detach or a new attach is rejected", () => {
  const detached = run(...attached(), retryable, { type: "page.detach", live: true });
  expect(detached.value).toBe("none");
  expect(mountMachine.transition(detached, { type: "reopen.due", open: offer() })[0]).toBe(detached);

  const reattached = run(...attached(), retryable, { type: "page.attach", session: "t", open: offer() });
  expect(reattached.value).toBe("opening");
  expect(mountMachine.transition(reattached, { type: "reopen.due", open: offer() })[0]).toBe(reattached);

  const due = run(...attached(), retryable);
  expect(due.value).toBe("backoff");
  expect(mountMachine.transition(due, { type: "reopen.due", open: offer() })[0].value).toBe("opening");
  expect(mountMachine.transition(due, { type: "reopen.due", open: undefined })[0].value).toBe("waiting");
});

test("linkOf projects every state onto the Link the page sees today", () => {
  const s = session("s");
  const exited = { ...session("s", { status: "exited" }), exit: { signal: "HUP" } };
  const cases: [MountEvent[], Link | undefined][] = [
    [[], undefined],
    [[{ type: "hub.live", open: offer() }], { state: "connected", session: undefined }],
    [[{ type: "hub.live", open: offer() }, { type: "hub.down" }], undefined],
    [[{ type: "page.attach", session: "s", open: undefined }], undefined],
    [[{ type: "page.attach", session: "s", open: offer() }], undefined],
    [attached(s), { state: "connected", session: s }],
    [[...attached(exited), { type: "frame.exited", exit: exited.exit }], { state: "ended", reason: "exited", exit: { signal: "HUP" }, retry: false }],
    [[...attached(), { type: "frame.exited", exit: undefined }], { state: "ended", reason: "exited", retry: false }],
    [[...attached(), { type: "frame.closed", reason: "stopping", message: undefined }], { state: "ended", reason: "stopping", retry: true }],
    [[...attached(), { type: "frame.closed", reason: "closed", message: "bye" }], { state: "ended", reason: "closed", message: "bye", retry: false }],
    [[...attached(), { type: "frame.closed", reason: "detached", message: "taken over" }], { state: "ended", reason: "detached", message: "taken over", retry: false }],
    [[...attached(), { type: "frame.closed", reason: "removed", message: undefined }, { type: "hub.down" }], undefined],
    [[...attached(), retryable], { state: "ended", reason: "error", code: "internal", message: "later", retry: true }],
    [
      [{ type: "page.attach", session: "gone", open: offer() }, { type: "frame.error", error: { code: "no_such_session", message: "no session gone" }, endsAttachment: true }],
      { state: "ended", reason: "no_such_session", code: "no_such_session", message: "no session gone", retry: false },
    ],
    [[...attached(), { type: "attachment.threw", error: ClientError.desync("OUTPUT at 3, expected 0") }], { state: "ended", reason: "error", message: "OUTPUT at 3, expected 0", retry: true }],
    [[...attached(), { type: "attachment.threw", error: ClientError.protocol("short OUTPUT") }], { state: "ended", reason: "error", message: "short OUTPUT", retry: false }],
    [[...attached(), { type: "page.unmount" }], undefined],
  ];
  for (const [events, link] of cases) expect(linkOf(run(...events))).toEqual(link);
});

test("an error that keeps the attachment changes nothing, and a disposed mount takes no event", () => {
  const live = run(...attached());
  const kept = { type: "frame.error", error: { code: "read_only", message: "read only" }, endsAttachment: false } as const;
  expect(mountMachine.transition(live, kept)[0]).toBe(live);

  const disposed = run(...attached(), { type: "page.unmount" });
  for (const event of [...attached(), { type: "page.detach", live: true }, { type: "reopen.due", open: offer() }] as MountEvent[]) {
    expect(mountMachine.transition(disposed, event)[0]).toBe(disposed);
  }
});

test("a mount that lets go of its attachment holds no channel, connection or attachment afterwards", () => {
  const gaveUp = run(...attached(), { type: "frame.closed", reason: "removed", message: undefined });
  expect(gaveUp.value).toBe("none");
  const waiting = mountMachine.transition(gaveUp, { type: "page.attach", session: "t", open: undefined })[0];
  expect(waiting.value).toBe("waiting");
  for (const snapshot of [gaveUp, waiting]) {
    expect(snapshot.context).toMatchObject({ attachment: undefined, channel: undefined, conn: undefined, described: undefined });
  }

  const lost = run(...attached(), { type: "hub.down" });
  expect(lost.value).toBe("waiting");
  expect(lost.context).toMatchObject({ session: "s", channel: undefined, conn: undefined });
  expect(lost.context.attachment).toBeDefined();
});
