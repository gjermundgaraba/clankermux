import pin from "../../../../vendor/pin.json" with { type: "json" };

/** Engine id of the vendored libghostty-vt build, matched by the server. */
export const ENGINE: string = pin.engine;

type Field = { offset: number; size: number; type: string };
type TypeInfo =
  | { kind: "struct"; size: number; fields: Record<string, Field> }
  | { kind: "enum"; size: number; values: Record<string, number> }
  | { kind: "pointer" | "array" | "union" | "opaque" | "primitive"; size: number };

const RESULT_SUCCESS = 0;
const RESULT_NO_VALUE = -4;

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

  get memory(): Uint8Array {
    return new Uint8Array(this.exports.memory.buffer);
  }

  call(name: string, ...args: number[]): number {
    const fn = this.exports[name];
    if (!fn) throw new Error(`wasm export ${name} missing`);
    return fn(...args) as number;
  }

  /** Calls a function returning GhosttyResult and throws on failure. */
  check(name: string, ...args: number[]): void {
    const code = this.call(name, ...args);
    if (code !== RESULT_SUCCESS) throw new EngineError(name, code);
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

  private cString(ptr: number): string {
    const mem = this.memory;
    let end = ptr;
    while (mem[end] !== 0) end++;
    return new TextDecoder().decode(mem.subarray(ptr, end));
  }

  /** Builds a zeroed, sized struct with the given fields set. */
  struct(name: string, values: Record<string, number | boolean | Record<string, unknown>>): Uint8Array {
    const info = this.types[name];
    if (!info || info.kind !== "struct") throw new Error(`unknown struct ${name}`);
    const out = new Uint8Array(info.size);
    const view = new DataView(out.buffer);
    const sizeField = info.fields["size"];
    if (sizeField) view.setUint32(sizeField.offset, info.size, true);
    for (const [key, value] of Object.entries(values)) {
      const field = info.fields[key];
      if (!field) throw new Error(`${name} has no field ${key}`);
      if (typeof value === "object") {
        out.set(this.struct(field.type, value as Record<string, number | boolean>), field.offset);
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

  enumValue(name: string, member: string): number {
    const info = this.types[name];
    if (!info || info.kind !== "enum") throw new Error(`unknown enum ${name}`);
    const value = info.values[member];
    if (value === undefined) throw new Error(`${name} has no member ${member}`);
    return value;
  }

  terminalNew(cols: number, rows: number): Terminal {
    const slot = this.call("ghostty_wasm_alloc_opaque");
    try {
      this.check("ghostty_terminal_new", 0, slot, cols, rows);
      return new Terminal(this, this.call("ghostty_wasm_take_opaque", slot));
    } finally {
      this.call("ghostty_wasm_free_opaque", slot);
    }
  }

  /** Reads a GhosttyString at `ptr` using the manifest's layout. */
  readString(ptr: number): string {
    const info = this.types["GhosttyString"];
    if (!info || info.kind !== "struct") throw new Error("GhosttyString missing from manifest");
    const view = new DataView(this.memory.buffer);
    const data = view.getUint32(ptr + info.fields["ptr"]!.offset, true);
    const len = view.getUint32(ptr + info.fields["len"]!.offset, true);
    return new TextDecoder().decode(this.memory.subarray(data, data + len));
  }

  sizeOf(name: string): number {
    const info = this.types[name];
    if (!info) throw new Error(`unknown type ${name}`);
    return info.size;
  }

  /** Decodes a complete snapshot held in memory. */
  decodeSnapshot(bytes: Uint8Array): Terminal {
    const src = this.copyIn(bytes);
    const slot = this.call("ghostty_wasm_alloc_opaque");
    try {
      this.check("ghostty_snapshot_decoder_new_buf", 0, slot, src, bytes.length);
      const decoder = this.call("ghostty_wasm_take_opaque", slot);
      try {
        this.check("ghostty_snapshot_decoder_decode", decoder, slot);
        return new Terminal(this, this.call("ghostty_wasm_take_opaque", slot));
      } finally {
        this.call("ghostty_snapshot_decoder_free", decoder);
      }
    } finally {
      this.call("ghostty_wasm_free_opaque", slot);
      this.free(src, bytes.length);
    }
  }

}

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

  private getU16(key: string): number {
    const out = this.engine.alloc(2);
    try {
      this.engine.check(
        "ghostty_terminal_get",
        this.handle,
        this.engine.enumValue("GhosttyTerminalData", key),
        out,
      );
      return new DataView(this.engine.memory.buffer).getUint16(out, true);
    } finally {
      this.engine.free(out, 2);
    }
  }

  get cols(): number {
    return this.getU16("COLS");
  }

  get rows(): number {
    return this.getU16("ROWS");
  }

  get title(): string {
    const size = this.engine.sizeOf("GhosttyString");
    const out = this.engine.alloc(size);
    try {
      const code = this.engine.call(
        "ghostty_terminal_get",
        this.handle,
        this.engine.enumValue("GhosttyTerminalData", "TITLE"),
        out,
      );
      if (code === RESULT_NO_VALUE) return "";
      if (code !== RESULT_SUCCESS) throw new EngineError("ghostty_terminal_get", code);
      return this.engine.readString(out);
    } finally {
      this.engine.free(out, size);
    }
  }

  /** The active screen as plain text, one line per row. */
  formatPlain(): string {
    const options = this.engine.struct("GhosttyFormatterTerminalOptions", {
      emit: this.engine.enumValue("GhosttyFormatterFormat", "PLAIN"),
      extra: { screen: {} },
    });
    const optionsPtr = this.engine.copyIn(options);
    const slot = this.engine.call("ghostty_wasm_alloc_opaque");
    const outPtr = this.engine.alloc(4);
    const outLen = this.engine.alloc(4);
    try {
      this.engine.check("ghostty_formatter_terminal_new", 0, slot, this.handle, optionsPtr);
      const formatter = this.engine.call("ghostty_wasm_take_opaque", slot);
      try {
        this.engine.check("ghostty_formatter_format_alloc", formatter, 0, outPtr, outLen);
        const view = new DataView(this.engine.memory.buffer);
        const ptr = view.getUint32(outPtr, true);
        const len = view.getUint32(outLen, true);
        try {
          return new TextDecoder().decode(this.engine.memory.slice(ptr, ptr + len));
        } finally {
          this.engine.call("ghostty_free", 0, ptr, len);
        }
      } finally {
        this.engine.call("ghostty_formatter_free", formatter);
      }
    } finally {
      this.engine.free(outLen, 4);
      this.engine.free(outPtr, 4);
      this.engine.call("ghostty_wasm_free_opaque", slot);
      this.engine.free(optionsPtr, options.length);
    }
  }

  free(): void {
    this.engine.call("ghostty_terminal_free", this.handle);
  }
}
