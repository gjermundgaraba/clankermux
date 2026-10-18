import pin from "../../../../vendor/pin.json" with { type: "json" };

export { Decoder, readyEnd } from "./decoder.ts";
export { KeyEncoder, isText, keyName } from "./keys.ts";
export type { KeyInput } from "./keys.ts";
export { FLAG, NO_COLOR, RenderState } from "./render.ts";
export type { Cell, Frame, FrameRow } from "./render.ts";

/** Engine id of the vendored libghostty-vt build, matched by the server. */
export const ENGINE: string = pin.engine;

type Field = { offset: number; size: number; type: string };
/** Struct fields to set: numbers, or the fields of a nested struct. */
type Packed = { [field: string]: number | Packed };
/** A bit field of a packed type; a union arm nests its own bits. */
type Bits = { lsb: number; width: number; arms?: Record<string, { bits: Record<string, Bits> }> };
type TypeInfo =
  | { kind: "struct"; size: number; fields: Record<string, Field> }
  | { kind: "enum"; size: number; values: Record<string, number> }
  | { kind: "packed"; size: number; bits: Record<string, Bits> }
  | { kind: "pointer" | "array" | "union" | "opaque" | "primitive" | "alias"; size: number };

/** Position and width of a bit field, from the manifest. */
export interface BitField {
  lsb: number;
  width: number;
}

export class EngineError extends Error {
  constructor(
    readonly call: string,
    readonly code: number,
  ) {
    super(`${call} returned ${code}`);
  }
}

/** A loaded ghostty-vt.wasm instance. */
export class Engine {
  private readonly types: Record<string, TypeInfo>;
  /** The `GhosttyResult` members the wrapper branches on. */
  readonly result: { SUCCESS: number; OUT_OF_SPACE: number; NO_VALUE: number; INVALID_VALUE: number };

  private constructor(
    private readonly exports: Record<string, Function> & { memory: WebAssembly.Memory },
  ) {
    const ptr = this.call("ghostty_type_json") as number;
    this.types = JSON.parse(this.cString(ptr)).types;
    const result = (member: string) => this.enumValue("GhosttyResult", member);
    this.result = {
      SUCCESS: result("SUCCESS"),
      OUT_OF_SPACE: result("OUT_OF_SPACE"),
      NO_VALUE: result("NO_VALUE"),
      INVALID_VALUE: result("INVALID_VALUE"),
    };
  }

  static async load(wasm: BufferSource): Promise<Engine> {
    const { instance } = await WebAssembly.instantiate(wasm, {});
    return new Engine(instance.exports as Engine["exports"]);
  }

  /** The current linear memory; reacquired on every access because it may grow. */
  get memory(): Uint8Array {
    return new Uint8Array(this.exports.memory.buffer);
  }

  get view(): DataView {
    return new DataView(this.exports.memory.buffer);
  }

  call(name: string, ...args: number[]): number {
    const fn = this.exports[name];
    if (!fn) throw new Error(`wasm export ${name} missing`);
    return fn(...args) as number;
  }

  /** Calls a function returning GhosttyResult and throws on failure. */
  check(name: string, ...args: number[]): void {
    const code = this.call(name, ...args);
    if (code !== this.result.SUCCESS) throw new EngineError(name, code);
  }

  alloc(len: number): number {
    const ptr = this.call("ghostty_wasm_alloc", len);
    if (ptr === 0) throw new Error("wasm allocation failed");
    return ptr;
  }

  free(ptr: number, len: number): void {
    this.call("ghostty_wasm_free", ptr, len);
  }

  /** Runs `f` with `bytes` copied into wasm memory, or with a null
   * pointer for no bytes (which the allocator cannot serve). */
  withBytes<T>(bytes: Uint8Array, f: (ptr: number) => T): T {
    if (bytes.length === 0) return f(0);
    const ptr = this.alloc(bytes.length);
    this.memory.set(bytes, ptr);
    try {
      return f(ptr);
    } finally {
      this.free(ptr, bytes.length);
    }
  }

  /** Runs `f` with a scratch allocation of `len` bytes. */
  scratch<T>(len: number, f: (ptr: number) => T): T {
    const ptr = this.alloc(len);
    try {
      return f(ptr);
    } finally {
      this.free(ptr, len);
    }
  }

  /** Runs `f` with an opaque-handle slot; the handle is taken out before the slot is freed. */
  private withSlot<T>(f: (slot: number) => T): T {
    const slot = this.call("ghostty_wasm_alloc_opaque");
    try {
      return f(slot);
    } finally {
      this.call("ghostty_wasm_free_opaque", slot);
    }
  }

  /** Creates an opaque handle through a `(allocator, out_slot, ...args)` constructor. */
  construct(name: string, ...args: number[]): number {
    return this.withSlot((slot) => {
      this.check(name, 0, slot, ...args);
      return this.call("ghostty_wasm_take_opaque", slot);
    });
  }

  /** Takes an opaque handle from a `(...args, out_slot)` call. */
  take(name: string, ...args: number[]): number {
    return this.withSlot((slot) => {
      this.check(name, ...args, slot);
      return this.call("ghostty_wasm_take_opaque", slot);
    });
  }

  private cString(ptr: number): string {
    const mem = this.memory;
    let end = ptr;
    while (mem[end] !== 0) end++;
    return new TextDecoder().decode(mem.subarray(ptr, end));
  }

  private struct(name: string): Extract<TypeInfo, { kind: "struct" }> {
    const info = this.types[name];
    if (!info || info.kind !== "struct") throw new Error(`unknown struct ${name}`);
    return info;
  }

  /** Builds a zeroed, sized struct with the given fields set. */
  pack(name: string, values: Packed): Uint8Array {
    const info = this.struct(name);
    const out = new Uint8Array(info.size);
    const view = new DataView(out.buffer);
    const sizeField = info.fields["size"];
    if (sizeField) view.setUint32(sizeField.offset, info.size, true);
    for (const [key, value] of Object.entries(values)) {
      const field = info.fields[key];
      if (!field) throw new Error(`${name} has no field ${key}`);
      if (typeof value === "object") {
        out.set(this.pack(field.type, value), field.offset);
      } else if (field.size === 4) {
        view.setInt32(field.offset, value, true);
      } else if (field.size === 2) {
        view.setUint16(field.offset, value, true);
      } else if (field.size === 1) {
        out[field.offset] = value;
      } else {
        throw new Error(`${name}.${key}: unsupported size ${field.size}`);
      }
    }
    return out;
  }

  /** Reads one numeric or boolean field of a struct at `ptr`. */
  field(name: string, ptr: number, key: string): number {
    const field = this.struct(name).fields[key];
    if (!field) throw new Error(`${name} has no field ${key}`);
    const view = this.view;
    const at = ptr + field.offset;
    switch (field.size) {
      case 1:
        return view.getUint8(at);
      case 2:
        return view.getUint16(at, true);
      case 4:
        return field.type.startsWith("u") ? view.getUint32(at, true) : view.getInt32(at, true);
      default:
        throw new Error(`${name}.${key}: unsupported size ${field.size}`);
    }
  }

  /** A bit field of a packed type: `path` names the field, or a union
   * field, its arm and the arm's field. */
  bits(name: string, ...path: [field: string] | [field: string, arm: string, inner: string]): BitField {
    const info = this.types[name];
    if (!info || info.kind !== "packed") throw new Error(`unknown packed type ${name}`);
    const top = info.bits[path[0]];
    if (!top) throw new Error(`${name} has no bit field ${path[0]}`);
    if (path.length === 1) return { lsb: top.lsb, width: top.width };
    const [field, arm, inner] = path;
    const nested = top.arms?.[arm]?.bits[inner];
    if (!nested) throw new Error(`${name}.${field} has no ${arm}.${inner}`);
    return { lsb: top.lsb + nested.lsb, width: nested.width };
  }

  /** Reads a bit field of a 64-bit packed value at `ptr`. */
  readBits(ptr: number, field: BitField): number {
    const value = this.view.getBigUint64(ptr, true);
    return Number((value >> BigInt(field.lsb)) & ((1n << BigInt(field.width)) - 1n));
  }

  /** Runs an encoder that reports `OUT_OF_SPACE` with the needed size,
   * growing the output buffer until it fits. */
  growing(name: string, initial: number, encode: (buf: number, cap: number, len: number) => number): Uint8Array {
    let cap = initial;
    for (;;) {
      const out = this.scratch(cap, (buf) =>
        this.scratch(4, (len) => {
          const code = encode(buf, cap, len);
          const n = this.view.getUint32(len, true);
          if (code === this.result.OUT_OF_SPACE) return n;
          if (code !== this.result.SUCCESS) throw new EngineError(name, code);
          return this.memory.slice(buf, buf + n);
        }),
      );
      if (typeof out !== "number") return out;
      cap = out;
    }
  }

  /** Runs an `*_alloc` call that returns bytes through `(ptr, len)` out
   * parameters and frees them after copying. */
  allocated(call: (ptr: number, len: number) => void): Uint8Array {
    return this.scratch(8, (out) => {
      call(out, out + 4);
      const ptr = this.view.getUint32(out, true);
      const len = this.view.getUint32(out + 4, true);
      try {
        return this.memory.slice(ptr, ptr + len);
      } finally {
        this.call("ghostty_free", 0, ptr, len);
      }
    });
  }

  /** The offset of a struct field, for nested reads. */
  offsetOf(name: string, key: string): number {
    const field = this.struct(name).fields[key];
    if (!field) throw new Error(`${name} has no field ${key}`);
    return field.offset;
  }

  enumValue(name: string, member: string): number {
    const info = this.types[name];
    if (!info || info.kind !== "enum") throw new Error(`unknown enum ${name}`);
    const value = info.values[member];
    if (value === undefined) throw new Error(`${name} has no member ${member}`);
    return value;
  }

  hasEnumValue(name: string, member: string): boolean {
    const info = this.types[name];
    return info?.kind === "enum" && member in info.values;
  }

  sizeOf(name: string): number {
    const info = this.types[name];
    if (!info) throw new Error(`unknown type ${name}`);
    return info.size;
  }

  /** Reads a GhosttyString at `ptr` using the manifest's layout. */
  readString(ptr: number): string {
    const data = this.field("GhosttyString", ptr, "ptr");
    const len = this.field("GhosttyString", ptr, "len");
    return new TextDecoder().decode(this.memory.subarray(data, data + len));
  }

  /** A `GhosttyColorRgb` at `ptr` as 0xRRGGBB. */
  readRgb(ptr: number): number {
    const channel = (key: string) => this.field("GhosttyColorRgb", ptr, key);
    return ((channel("r") << 16) | (channel("g") << 8) | channel("b")) >>> 0;
  }

  terminalNew(cols: number, rows: number): Terminal {
    return new Terminal(this, this.construct("ghostty_terminal_new", cols, rows));
  }

  /** Decodes a complete snapshot held in memory. */
  decodeSnapshot(bytes: Uint8Array): Terminal {
    return this.withBytes(bytes, (src) => {
      const decoder = this.construct("ghostty_snapshot_decoder_new_buf", src, bytes.length);
      try {
        return new Terminal(this, this.take("ghostty_snapshot_decoder_decode", decoder));
      } finally {
        this.call("ghostty_snapshot_decoder_free", decoder);
      }
    });
  }

  /**
   * Paste text encoded for the PTY: unsafe bytes blanked, wrapped in
   * bracketed paste when `bracketed`, newlines to carriage returns otherwise.
   */
  encodePaste(text: string, bracketed: boolean): Uint8Array {
    const data = new TextEncoder().encode(text);
    if (data.length === 0) return data;
    return this.withBytes(data, (src) =>
      this.growing("ghostty_paste_encode", data.length + 16, (buf, cap, len) =>
        this.call("ghostty_paste_encode", src, data.length, bracketed ? 1 : 0, buf, cap, len),
      ),
    );
  }
}

/** `ghostty_terminal_new` with the given `GhosttyTerminal` handle wrapped. */
export class Terminal {
  constructor(
    readonly engine: Engine,
    readonly handle: number,
  ) {}

  write(bytes: Uint8Array): void {
    if (bytes.length === 0) return;
    this.engine.withBytes(bytes, (ptr) => this.engine.call("ghostty_terminal_vt_write", this.handle, ptr, bytes.length));
  }

  private get(key: string, size: number): number {
    return this.engine.scratch(size, (out) => {
      this.engine.check(
        "ghostty_terminal_get",
        this.handle,
        this.engine.enumValue("GhosttyTerminalData", key),
        out,
      );
      return size === 2 ? this.engine.view.getUint16(out, true) : this.engine.view.getUint32(out, true);
    });
  }

  get cols(): number {
    return this.get("COLS", 2);
  }

  get rows(): number {
    return this.get("ROWS", 2);
  }

  get scrollbackRows(): number {
    return this.get("SCROLLBACK_ROWS", 4);
  }

  get title(): string {
    const size = this.engine.sizeOf("GhosttyString");
    return this.engine.scratch(size, (out) => {
      const code = this.engine.call(
        "ghostty_terminal_get",
        this.handle,
        this.engine.enumValue("GhosttyTerminalData", "TITLE"),
        out,
      );
      if (code === this.engine.result.NO_VALUE) return "";
      if (code !== this.engine.result.SUCCESS) throw new EngineError("ghostty_terminal_get", code);
      return this.engine.readString(out);
    });
  }

  /** Whether DEC private mode `number` is set. */
  mode(number: number): boolean {
    const config = this.engine.pack("GhosttyTerminalModeConfig", { mode: number });
    return this.engine.withBytes(config, (ptr) => {
      this.engine.check(
        "ghostty_terminal_get",
        this.handle,
        this.engine.enumValue("GhosttyTerminalData", "MODE"),
        ptr,
      );
      return this.engine.field("GhosttyTerminalModeConfig", ptr, "value") !== 0;
    });
  }

  /** No cell size in pixels: `RESIZED` carries none. */
  resize(cols: number, rows: number): void {
    this.engine.check("ghostty_terminal_resize", this.handle, cols, rows, 0, 0);
  }

  /** Sets a default colour (0xRRGGBB), as OSC 10, 11 and 12 report it. */
  setColor(kind: "foreground" | "background" | "cursor", color: number): void {
    const option = this.engine.enumValue("GhosttyTerminalOption", `COLOR_${kind.toUpperCase()}`);
    const rgb = this.engine.pack("GhosttyColorRgb", { r: (color >> 16) & 0xff, g: (color >> 8) & 0xff, b: color & 0xff });
    this.engine.withBytes(rgb, (ptr) => this.engine.check("ghostty_terminal_set", this.handle, option, ptr));
  }

  /** The whole terminal, history included, as a snapshot. */
  snapshot(): Uint8Array {
    return this.engine.allocated((ptr, len) =>
      this.engine.check("ghostty_snapshot_encode_alloc", this.handle, 0, ptr, len),
    );
  }

  /** The active screen as plain text, one line per row. */
  formatPlain(): string {
    const options = this.engine.pack("GhosttyFormatterTerminalOptions", {
      emit: this.engine.enumValue("GhosttyFormatterFormat", "PLAIN"),
      extra: { screen: {} },
    });
    return this.engine.withBytes(options, (optionsPtr) => {
      const formatter = this.engine.construct("ghostty_formatter_terminal_new", this.handle, optionsPtr);
      try {
        return new TextDecoder().decode(
          this.engine.allocated((ptr, len) =>
            this.engine.check("ghostty_formatter_format_alloc", formatter, 0, ptr, len),
          ),
        );
      } finally {
        this.engine.call("ghostty_formatter_free", formatter);
      }
    });
  }

  free(): void {
    this.engine.call("ghostty_terminal_free", this.handle);
  }
}
