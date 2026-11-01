/** The small slice of protobuf wire format this proxy writes by hand.
 * Cursor's current exec messages include fields this generated schema does
 * not name. The runtime keeps those bytes, and writing them back needs the
 * length prefix that a length-delimited unknown field carries. */

export function concat(parts: Uint8Array[]): Uint8Array {
  const size = parts.reduce((sum, part) => sum + part.length, 0);
  const out = new Uint8Array(size);
  let offset = 0;
  for (const part of parts) {
    out.set(part, offset);
    offset += part.length;
  }
  return out;
}

export function varint(value: number): Uint8Array {
  const bytes: number[] = [];
  let rest = value >>> 0;
  while (rest > 0x7f) {
    bytes.push((rest & 0x7f) | 0x80);
    rest >>>= 7;
  }
  bytes.push(rest);
  return Uint8Array.from(bytes);
}

/** Length-delimited field body as an unknown-field payload: length, then bytes. */
export function lengthDelimited(payload: Uint8Array): Uint8Array {
  return concat([varint(payload.length), payload]);
}

export function readVarint(data: Uint8Array, offset: number): { value: number; next: number } {
  let value = 0;
  let shift = 0;
  let index = offset;
  while (index < data.length) {
    const byte = data[index]!;
    index += 1;
    value |= (byte & 0x7f) << shift;
    if ((byte & 0x80) === 0) return { value: value >>> 0, next: index };
    shift += 7;
  }
  throw new Error("truncated varint");
}

/** Drop the length prefix stored on a length-delimited unknown field. */
export function delimitedBody(data: Uint8Array): Uint8Array {
  const { value, next } = readVarint(data, 0);
  return data.subarray(next, next + value);
}

export interface WireField {
  field: number;
  wire: number;
  /** For length-delimited fields, the bytes after the length. */
  body: Uint8Array;
}

export function readMessage(data: Uint8Array): WireField[] {
  const fields: WireField[] = [];
  let offset = 0;
  while (offset < data.length) {
    const tag = readVarint(data, offset);
    offset = tag.next;
    const field = tag.value >>> 3;
    const wire = tag.value & 7;
    if (wire === 2) {
      const len = readVarint(data, offset);
      offset = len.next;
      fields.push({ field, wire, body: data.subarray(offset, offset + len.value) });
      offset += len.value;
      continue;
    }
    if (wire === 0) {
      const start = offset;
      const value = readVarint(data, offset);
      offset = value.next;
      fields.push({ field, wire, body: data.subarray(start, offset) });
      continue;
    }
    throw new Error(`unsupported wire type ${wire}`);
  }
  return fields;
}

export function stringField(field: number, value: string): Uint8Array {
  const bytes = new TextEncoder().encode(value);
  return concat([varint((field << 3) | 2), varint(bytes.length), bytes]);
}

export function messageField(field: number, payload: Uint8Array): Uint8Array {
  return concat([varint((field << 3) | 2), varint(payload.length), payload]);
}
