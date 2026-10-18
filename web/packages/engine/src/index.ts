import pin from "../../../../vendor/pin.json" with { type: "json" };

export { Decoder } from "./decoder.ts";
export { KeyEncoder, keyName } from "./keys.ts";
export type { KeyInput } from "./keys.ts";
export { RenderState } from "./render.ts";
export type { Cell, Frame, FrameRow } from "./render.ts";

/** Engine id of the vendored libghostty-vt build, matched by the server. */
export const ENGINE: string = pin.engine;

type Field = { offset: number; size: number; type: string };
export type TypeInfo =
  | { kind: "struct"; size: number; fields: Record<string, Field> }
  | { kind: "enum"; size: number; values: Record<string, number> }
  | { kind: "pointer" | "array" | "union" | "opaque" | "primitive" | "alias" | "packed"; size: number };

export const RESULT = {
  SUCCESS: 0,
  INVALID_VALUE: -2,
  OUT_OF_SPACE: -3,
  NO_VALUE: -4,
} as const;

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
  readonly types: Record<string, TypeInfo>;

  private constructor(
    private readonly exports: Record<string, Function> & { memory: WebAssembly.Memory },
  ) {
    const ptr = this.call("ghostty_type_json") as number;
    this.types = JSON.parse(this.cString(ptr)).types;
  }

  static async load(wasm: BufferSource | WebAssembly.Module): Promise<Engine> {
    const module = wasm instanceof WebAssembly.Module ? wasm : await WebAssembly.compile(wasm);
    const instance = await WebAssembly.instantiate(module, {});
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
    if (code !== RESULT.SUCCESS) throw new EngineError(name, code);
  }

  alloc(len: number): number {
    const ptr = this.call("ghostty_wasm_alloc", len);
    if (ptr === 0) throw new Error("wasm allocation failed");
    return ptr;
  }

  free(ptr: number, len: number): void {
    this.call("ghostty_wasm_free", ptr, len);
  }

  /** Copies bytes into wasm memory; the caller frees them. */
  copyIn(bytes: Uint8Array): number {
    const ptr = this.alloc(bytes.length);
    this.memory.set(bytes, ptr);
    return ptr;
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
  withSlot<T>(f: (slot: number) => T): T {
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

  struct(name: string): Extract<TypeInfo, { kind: "struct" }> {
    const info = this.types[name];
    if (!info || info.kind !== "struct") throw new Error(`unknown struct ${name}`);
    return info;
  }

  /** Builds a zeroed, sized struct with the given fields set. */
  pack(name: string, values: Record<string, number | boolean | Record<string, unknown>>): Uint8Array {
    const info = this.struct(name);
    const out = new Uint8Array(info.size);
    const view = new DataView(out.buffer);
    const sizeField = info.fields["size"];
    if (sizeField) view.setUint32(sizeField.offset, info.size, true);
    for (const [key, value] of Object.entries(values)) {
      const field = info.fields[key];
      if (!field) throw new Error(`${name} has no field ${key}`);
      if (typeof value === "object") {
        out.set(this.pack(field.type, value as Record<string, number | boolean>), field.offset);
      } else if (typeof value === "boolean") {
        out[field.offset] = value ? 1 : 0;
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
    if (field.type === "bool") return view.getUint8(at);
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

  /** An RGB colour at `ptr` as 0xRRGGBB. */
  readRgb(ptr: number): number {
    const mem = this.memory;
    return ((mem[ptr]! << 16) | (mem[ptr + 1]! << 8) | mem[ptr + 2]!) >>> 0;
  }

  terminalNew(cols: number, rows: number): Terminal {
    return new Terminal(this, this.construct("ghostty_terminal_new", cols, rows));
  }

  /** Decodes a complete snapshot held in memory. */
  decodeSnapshot(bytes: Uint8Array): Terminal {
    const src = this.copyIn(bytes);
    try {
      const decoder = this.construct("ghostty_snapshot_decoder_new_buf", src, bytes.length);
      try {
        return new Terminal(this, this.take("ghostty_snapshot_decoder_decode", decoder));
      } finally {
        this.call("ghostty_snapshot_decoder_free", decoder);
      }
    } finally {
      this.free(src, bytes.length);
    }
  }

  /**
   * Paste text encoded for the PTY: unsafe bytes blanked, wrapped in
   * bracketed paste when `bracketed`, newlines to carriage returns otherwise.
   */
  encodePaste(text: string, bracketed: boolean): Uint8Array {
    const data = new TextEncoder().encode(text);
    const src = this.copyIn(data);
    try {
      let cap = data.length + 16;
      for (;;) {
        const out = this.scratch(cap, (buf) =>
          this.scratch(4, (written) => {
            const code = this.call(
              "ghostty_paste_encode",
              src,
              data.length,
              bracketed ? 1 : 0,
              buf,
              cap,
              written,
            );
            const len = this.view.getUint32(written, true);
            if (code === RESULT.OUT_OF_SPACE) return len;
            if (code !== RESULT.SUCCESS) throw new EngineError("ghostty_paste_encode", code);
            return this.memory.slice(buf, buf + len);
          }),
        );
        if (typeof out !== "number") return out;
        cap = out;
      }
    } finally {
      this.free(src, data.length);
    }
  }
}

/** `ghostty_terminal_new` with the given `GhosttyTerminal` handle wrapped. */
export class Terminal {
  constructor(
    readonly engine: Engine,
    readonly handle: number,
  ) {}

  write(bytes: Uint8Array): void {
    const ptr = this.engine.copyIn(bytes);
    try {
      this.engine.call("ghostty_terminal_vt_write", this.handle, ptr, bytes.length);
    } finally {
      this.engine.free(ptr, bytes.length);
    }
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
      if (code === RESULT.NO_VALUE) return "";
      if (code !== RESULT.SUCCESS) throw new EngineError("ghostty_terminal_get", code);
      return this.engine.readString(out);
    });
  }

  /** Whether DEC private mode `number` is set. */
  mode(number: number): boolean {
    const config = this.engine.pack("GhosttyTerminalModeConfig", { mode: number });
    const ptr = this.engine.copyIn(config);
    try {
      this.engine.check(
        "ghostty_terminal_get",
        this.handle,
        this.engine.enumValue("GhosttyTerminalData", "MODE"),
        ptr,
      );
      return this.engine.field("GhosttyTerminalModeConfig", ptr, "value") !== 0;
    } finally {
      this.engine.free(ptr, config.length);
    }
  }

  resize(cols: number, rows: number, cellWidth = 0, cellHeight = 0): void {
    this.engine.check("ghostty_terminal_resize", this.handle, cols, rows, cellWidth, cellHeight);
  }

  /** The whole terminal, history included, as a snapshot. */
  snapshot(): Uint8Array {
    return this.engine.scratch(8, (out) => {
      this.engine.check("ghostty_snapshot_encode_alloc", this.handle, 0, out, out + 4);
      const ptr = this.engine.view.getUint32(out, true);
      const len = this.engine.view.getUint32(out + 4, true);
      try {
        return this.engine.memory.slice(ptr, ptr + len);
      } finally {
        this.engine.call("ghostty_free", 0, ptr, len);
      }
    });
  }

  /** The active screen as plain text, one line per row. */
  formatPlain(): string {
    const options = this.engine.pack("GhosttyFormatterTerminalOptions", {
      emit: this.engine.enumValue("GhosttyFormatterFormat", "PLAIN"),
      extra: { screen: {} },
    });
    const optionsPtr = this.engine.copyIn(options);
    try {
      const formatter = this.engine.construct("ghostty_formatter_terminal_new", this.handle, optionsPtr);
      try {
        return this.engine.scratch(8, (out) => {
          this.engine.check("ghostty_formatter_format_alloc", formatter, 0, out, out + 4);
          const ptr = this.engine.view.getUint32(out, true);
          const len = this.engine.view.getUint32(out + 4, true);
          try {
            return new TextDecoder().decode(this.engine.memory.slice(ptr, ptr + len));
          } finally {
            this.engine.call("ghostty_free", 0, ptr, len);
          }
        });
      } finally {
        this.engine.call("ghostty_formatter_free", formatter);
      }
    } finally {
      this.engine.free(optionsPtr, options.length);
    }
  }

  free(): void {
    this.engine.call("ghostty_terminal_free", this.handle);
  }
}
