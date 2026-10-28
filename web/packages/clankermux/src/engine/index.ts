export { Decoder, readyEnd } from "./decoder.ts";
export { KeyEncoder, isText, keyName } from "./keys.ts";
export type { KeyInput } from "./keys.ts";
export { FLAG, NO_COLOR, RenderState } from "./render.ts";
export type { Cell, Frame, FrameRow, PackedCells, PackedFrame } from "./render.ts";
export { ENGINE } from "./pin.ts";
export { SelectionGesture } from "./selection.ts";
export type { CellPoint } from "./selection.ts";

type Field = { offset: number; size: number; type: string };
type Packed = { [field: string]: number | Packed };
type Bits = { lsb: number; width: number; arms?: Record<string, { bits: Record<string, Bits> }> };
type TypeInfo = { size: number; align: number } & (
  | { kind: "struct"; fields: Record<string, Field> }
  | { kind: "enum"; values: Record<string, number> }
  | { kind: "packed"; bits: Record<string, Bits> }
  | { kind: "pointer" | "array" | "union" | "opaque" | "primitive" | "alias" }
);

/** The viewport's place in the screen's rows, history included. */
export interface Scrollbar {
  /** Rows of the screen, history and the active area. */
  total: number;
  /** The viewport's first row. */
  offset: number;
  /** The viewport's rows. */
  len: number;
}

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

export class Engine {
  private readonly types: Record<string, TypeInfo>;
  private memoryView: Uint8Array | undefined;
  private dataView: DataView | undefined;
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

  /** Cached views are valid only until wasm grows its linear memory. */
  get memory(): Uint8Array {
    const buffer = this.exports.memory.buffer;
    if (this.memoryView?.buffer !== buffer) this.memoryView = new Uint8Array(buffer);
    return this.memoryView;
  }

  get view(): DataView {
    const buffer = this.exports.memory.buffer;
    if (this.dataView?.buffer !== buffer) this.dataView = new DataView(buffer);
    return this.dataView;
  }

  call(name: string, ...args: number[]): number {
    const fn = this.exports[name];
    if (!fn) throw new Error(`wasm export ${name} missing`);
    return fn(...args) as number;
  }

  check(name: string, ...args: number[]): void {
    const code = this.call(name, ...args);
    if (code !== this.result.SUCCESS) throw new EngineError(name, code);
  }

  present(name: string, ...args: number[]): boolean {
    const code = this.call(name, ...args);
    if (code === this.result.NO_VALUE) return false;
    if (code !== this.result.SUCCESS) throw new EngineError(name, code);
    return true;
  }

  alloc(len: number): number {
    const ptr = this.call("ghostty_wasm_alloc", len);
    if (ptr === 0) throw new Error("wasm allocation failed");
    return ptr;
  }

  free(ptr: number, len: number): void {
    this.call("ghostty_wasm_free", ptr, len);
  }

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

  scratch<T>(len: number, f: (ptr: number) => T): T {
    const ptr = this.alloc(len);
    try {
      return f(ptr);
    } finally {
      this.free(ptr, len);
    }
  }

  private withSlot<T>(f: (slot: number) => T): T {
    const slot = this.call("ghostty_wasm_alloc_opaque");
    try {
      return f(slot);
    } finally {
      this.call("ghostty_wasm_free_opaque", slot);
    }
  }

  construct(name: string, ...args: number[]): number {
    return this.withSlot((slot) => {
      this.check(name, 0, slot, ...args);
      return this.call("ghostty_wasm_take_opaque", slot);
    });
  }

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

  readBits(ptr: number, field: BitField): number {
    const bit = field.lsb % 32;
    if (bit + field.width <= 32) {
      const word = this.view.getUint32(ptr + 4 * Math.floor(field.lsb / 32), true) >>> bit;
      // A 32-bit mask would be signed; preserve the unsigned full word.
      return field.width === 32 ? word : word & (2 ** field.width - 1);
    }
    const value = this.view.getBigUint64(ptr, true);
    return Number((value >> BigInt(field.lsb)) & ((1n << BigInt(field.width)) - 1n));
  }

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
    return this.type(name).size;
  }

  alignOf(name: string): number {
    return this.type(name).align;
  }

  private type(name: string): TypeInfo {
    const info = this.types[name];
    if (!info) throw new Error(`unknown type ${name}`);
    return info;
  }

  readString(ptr: number): string {
    const data = this.field("GhosttyString", ptr, "ptr");
    const len = this.field("GhosttyString", ptr, "len");
    return new TextDecoder().decode(this.memory.subarray(data, data + len));
  }

  readRgb(ptr: number): number {
    const channel = (key: string) => this.field("GhosttyColorRgb", ptr, key);
    return ((channel("r") << 16) | (channel("g") << 8) | channel("b")) >>> 0;
  }

  terminalNew(cols: number, rows: number): Terminal {
    return new Terminal(this, this.construct("ghostty_terminal_new", cols, rows));
  }

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

  get alternate(): boolean {
    return this.get("ACTIVE_SCREEN", 4) === this.engine.enumValue("GhosttyTerminalScreen", "ALTERNATE");
  }

  get scrollbar(): Scrollbar {
    const e = this.engine;
    return e.scratch(e.sizeOf("GhosttyTerminalScrollbar"), (out) => {
      e.check("ghostty_terminal_get", this.handle, e.enumValue("GhosttyTerminalData", "SCROLLBAR"), out);
      const u64 = (key: string) => Number(e.view.getBigUint64(out + e.offsetOf("GhosttyTerminalScrollbar", key), true));
      return { total: u64("total"), offset: u64("offset"), len: u64("len") };
    });
  }

  scroll(to: number | "top" | "bottom"): void {
    const e = this.engine;
    const size = e.sizeOf("GhosttyTerminalScrollViewport");
    e.scratch(size, (ptr) => {
      e.memory.fill(0, ptr, ptr + size);
      const tag = typeof to === "number" ? "DELTA" : to.toUpperCase();
      e.view.setInt32(ptr + e.offsetOf("GhosttyTerminalScrollViewport", "tag"), e.enumValue("GhosttyTerminalScrollViewportTag", tag), true);
      if (typeof to === "number") e.view.setInt32(ptr + e.offsetOf("GhosttyTerminalScrollViewport", "value"), to, true);
      // The tagged union is passed by value, which the wasm ABI passes by address.
      e.call("ghostty_terminal_scroll_viewport", this.handle, ptr);
    });
  }

  get title(): string {
    const size = this.engine.sizeOf("GhosttyString");
    return this.engine.scratch(size, (out) =>
      this.engine.present("ghostty_terminal_get", this.handle, this.engine.enumValue("GhosttyTerminalData", "TITLE"), out)
        ? this.engine.readString(out)
        : "",
    );
  }

  gridRef(at: { x: number; y: number }): Uint8Array {
    const e = this.engine;
    const point = e.sizeOf("GhosttyPoint");
    const ref = e.sizeOf("GhosttyGridRef");
    const x = Math.min(Math.max(0, Math.floor(at.x)), this.cols - 1);
    const y = Math.min(Math.max(0, Math.floor(at.y)), this.rows - 1);
    return e.scratch(point + ref, (ptr) => {
      e.memory.fill(0, ptr, ptr + point + ref);
      e.view.setInt32(ptr + e.offsetOf("GhosttyPoint", "tag"), e.enumValue("GhosttyPointTag", "VIEWPORT"), true);
      const coordinate = ptr + e.offsetOf("GhosttyPoint", "value");
      e.view.setUint16(coordinate + e.offsetOf("GhosttyPointCoordinate", "x"), x, true);
      e.view.setUint32(coordinate + e.offsetOf("GhosttyPointCoordinate", "y"), y, true);
      e.view.setUint32(ptr + point, ref, true);
      // The point is passed by value, which the wasm ABI passes by address.
      e.check("ghostty_terminal_grid_ref", this.handle, ptr, ptr + point);
      return e.memory.slice(ptr + point, ptr + point + ref);
    });
  }

  select(selection: Uint8Array | undefined): void {
    const option = this.engine.enumValue("GhosttyTerminalOption", "SELECTION");
    if (!selection) this.engine.check("ghostty_terminal_set", this.handle, option, 0);
    else this.engine.withBytes(selection, (ptr) => this.engine.check("ghostty_terminal_set", this.handle, option, ptr));
  }

  selectionText(): string | undefined {
    const e = this.engine;
    const options = e.pack("GhosttyTerminalSelectionFormatOptions", {
      emit: e.enumValue("GhosttyFormatterFormat", "PLAIN"),
      unwrap: 1,
      trim: 1,
    });
    return e.withBytes(options, (optionsPtr) =>
      e.scratch(8, (out) => {
        e.view.setUint32(out, 0, true);
        e.view.setUint32(out + 4, 0, true);
        // The options are passed by value, which the wasm ABI passes by address.
        if (!e.present("ghostty_terminal_selection_format_alloc", this.handle, 0, optionsPtr, out, out + 4)) return undefined;
        const ptr = e.view.getUint32(out, true);
        const len = e.view.getUint32(out + 4, true);
        try {
          return new TextDecoder().decode(e.memory.slice(ptr, ptr + len));
        } finally {
          e.call("ghostty_free", 0, ptr, len);
        }
      }),
    );
  }

  get selected(): boolean {
    const e = this.engine;
    const size = e.sizeOf("GhosttySelection");
    return e.scratch(size, (out) => {
      e.view.setUint32(out, size, true);
      return e.present("ghostty_terminal_get", this.handle, e.enumValue("GhosttyTerminalData", "SELECTION"), out);
    });
  }

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

  resize(cols: number, rows: number): void {
    this.engine.check("ghostty_terminal_resize", this.handle, cols, rows, 0, 0);
  }

  setColor(kind: "foreground" | "background" | "cursor", color: number): void {
    const option = this.engine.enumValue("GhosttyTerminalOption", `COLOR_${kind.toUpperCase()}`);
    const rgb = this.engine.pack("GhosttyColorRgb", { r: (color >> 16) & 0xff, g: (color >> 8) & 0xff, b: color & 0xff });
    this.engine.withBytes(rgb, (ptr) => this.engine.check("ghostty_terminal_set", this.handle, option, ptr));
  }

  snapshot(): Uint8Array {
    return this.engine.allocated((ptr, len) =>
      this.engine.check("ghostty_snapshot_encode_alloc", this.handle, 0, ptr, len),
    );
  }

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
