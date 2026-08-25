//! Encode-only protobuf writer.
//!
//! OTLP responses carry nothing we act on, so maki never decodes protobuf.
//! That makes a writer with varints, length-delimited messages and the two
//! fixed widths enough to cover every field we emit.

const WIRE_VARINT: u32 = 0;
const WIRE_FIXED64: u32 = 1;
const WIRE_LEN: u32 = 2;

/// A `u64` varint is never longer than this.
const MAX_VARINT_LEN: usize = 10;

/// The encoded bytes and how many of them are used.
fn varint(mut value: u64) -> ([u8; MAX_VARINT_LEN], usize) {
    let mut out = [0u8; MAX_VARINT_LEN];
    let mut len = 0;
    while value >= 0x80 {
        out[len] = (value as u8) | 0x80;
        value >>= 7;
        len += 1;
    }
    out[len] = value as u8;
    (out, len + 1)
}

#[derive(Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn with_capacity(bytes: usize) -> Self {
        Self {
            buf: Vec::with_capacity(bytes),
        }
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    fn tag(&mut self, field: u32, wire: u32) {
        self.write_varint(u64::from(field << 3 | wire));
    }

    fn write_varint(&mut self, value: u64) {
        let (bytes, len) = varint(value);
        self.buf.extend_from_slice(&bytes[..len]);
    }

    pub fn uint64(&mut self, field: u32, value: u64) {
        self.tag(field, WIRE_VARINT);
        self.write_varint(value);
    }

    /// Negative values encode as ten-byte varints, matching protobuf `int64`.
    pub fn int64(&mut self, field: u32, value: i64) {
        self.uint64(field, value as u64);
    }

    pub fn int32(&mut self, field: u32, value: i32) {
        self.uint64(field, i64::from(value) as u64);
    }

    pub fn bool(&mut self, field: u32, value: bool) {
        self.tag(field, WIRE_VARINT);
        self.buf.push(u8::from(value));
    }

    pub fn fixed64(&mut self, field: u32, value: u64) {
        self.tag(field, WIRE_FIXED64);
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    pub fn sfixed64(&mut self, field: u32, value: i64) {
        self.fixed64(field, value as u64);
    }

    pub fn double(&mut self, field: u32, value: f64) {
        self.fixed64(field, value.to_bits());
    }

    pub fn string(&mut self, field: u32, value: &str) {
        self.bytes(field, value.as_bytes());
    }

    pub fn bytes(&mut self, field: u32, value: &[u8]) {
        self.tag(field, WIRE_LEN);
        self.write_varint(value.len() as u64);
        self.buf.extend_from_slice(value);
    }

    /// Writes a nested message in place, then splices its length in front. One
    /// memmove per nesting level beats a scratch buffer per message.
    pub fn message(&mut self, field: u32, body: impl FnOnce(&mut Self)) {
        self.tag(field, WIRE_LEN);
        let start = self.buf.len();
        body(self);
        let (prefix, len) = varint((self.buf.len() - start) as u64);
        self.buf.splice(start..start, prefix[..len].iter().copied());
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const FIELD: u32 = 1;

    fn encode(body: impl FnOnce(&mut Writer)) -> Vec<u8> {
        let mut w = Writer::default();
        body(&mut w);
        w.into_bytes()
    }

    #[test_case(0, &[0x08, 0x00]; "zero")]
    #[test_case(1, &[0x08, 0x01]; "one")]
    #[test_case(127, &[0x08, 0x7f]; "one_byte_max")]
    #[test_case(128, &[0x08, 0x80, 0x01]; "two_bytes")]
    #[test_case(300, &[0x08, 0xac, 0x02]; "three_hundred")]
    #[test_case(u64::MAX, &[0x08, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]; "max")]
    fn varints_match_the_wire_format(value: u64, expected: &[u8]) {
        assert_eq!(encode(|w| w.uint64(FIELD, value)), expected);
    }

    #[test]
    fn negative_int64_is_ten_bytes() {
        let bytes = encode(|w| w.int64(FIELD, -1));
        assert_eq!(
            bytes,
            vec![
                0x08, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01
            ]
        );
    }

    #[test]
    fn field_numbers_above_fifteen_use_two_tag_bytes() {
        assert_eq!(encode(|w| w.uint64(16, 1)), vec![0x80, 0x01, 0x01]);
    }

    #[test]
    fn strings_are_length_delimited() {
        assert_eq!(
            encode(|w| w.string(FIELD, "hi")),
            vec![0x0a, 0x02, b'h', b'i']
        );
    }

    #[test]
    fn doubles_are_little_endian_fixed64() {
        assert_eq!(
            encode(|w| w.double(FIELD, 1.5)),
            vec![0x09, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xf8, 0x3f]
        );
    }

    #[test]
    fn booleans_are_a_single_byte() {
        assert_eq!(encode(|w| w.bool(FIELD, true)), vec![0x08, 0x01]);
        assert_eq!(encode(|w| w.bool(FIELD, false)), vec![0x08, 0x00]);
    }

    #[test]
    fn nested_messages_carry_their_own_length() {
        let bytes = encode(|w| {
            w.message(FIELD, |inner| {
                inner.uint64(2, 7);
            });
        });
        assert_eq!(bytes, vec![0x0a, 0x02, 0x10, 0x07]);
    }

    #[test]
    fn deeply_nested_lengths_stay_correct() {
        let bytes = encode(|w| {
            w.message(1, |a| {
                a.message(1, |b| {
                    b.message(1, |c| {
                        c.string(1, "x");
                    });
                });
            });
        });
        assert_eq!(
            bytes,
            vec![0x0a, 0x07, 0x0a, 0x05, 0x0a, 0x03, 0x0a, 0x01, b'x']
        );
    }

    #[test]
    fn long_nested_messages_use_multi_byte_lengths() {
        let payload = "x".repeat(200);
        let bytes = encode(|w| {
            w.message(FIELD, |inner| {
                inner.string(1, &payload);
            });
        });
        assert_eq!(&bytes[..2], &[0x0a, 0xcb]);
        assert_eq!(bytes[2], 0x01);
        assert_eq!(bytes.len(), 2 + 1 + 3 + payload.len());
    }
}
