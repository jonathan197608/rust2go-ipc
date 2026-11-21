use super::*;
use std::mem::align_of;

#[test]
#[should_panic(expected = "decode out of bounds")]
fn decoder_prim_panics_on_truncation() {
    let mut dec = Decoder::new(&[0x01, 0x02]);
    let _ = dec.prim::<u64>();
}

#[test]
#[should_panic(expected = "invalid bool byte")]
fn decoder_rejects_invalid_bool() {
    let mut dec = Decoder::new(&[0x02]);
    let _ = dec.prim::<bool>();
}

#[test]
fn decoder_accepts_valid_bool() {
    let mut dec = Decoder::new(&[0x00, 0x01]);
    assert!(!dec.prim::<bool>());
    assert!(dec.prim::<bool>());
}

#[test]
#[should_panic(expected = "decode out of bounds")]
fn decoder_string_rejects_huge_len() {
    // u64 len = 0xFFFF_FFFF_FFFF_FFFF, no payload follows.
    let mut dec = Decoder::new(&[0xff; 8]);
    let _ = dec.string();
}

#[test]
fn decoder_prim_list_is_aligned() {
    // 2 x i64 = 16 bytes. The restored pointer must be 8-byte aligned.
    let mut buf = Vec::new();
    buf.extend_from_slice(&2u64.to_le_bytes());
    buf.extend_from_slice(&1i64.to_le_bytes());
    buf.extend_from_slice(&2i64.to_le_bytes());
    let mut dec = Decoder::new(&buf);
    let list = dec.prim_list::<i64>();
    assert_eq!(list.len, 2);
    assert_eq!(list.ptr as usize % align_of::<i64>(), 0);
    // SAFETY: prim_list restored an aligned, owned i64 array in the arena.
    let vals = unsafe { from_raw_parts(list.ptr as *const i64, list.len) };
    assert_eq!(vals, &[1i64, 2i64]);
}

#[test]
fn wire_primitives_are_little_endian() {
    let mut enc = Encoder::new();
    enc.prim(0x0102_0304u32);
    enc.prim('A');
    assert_eq!(enc.take(), vec![4, 3, 2, 1, b'A', 0, 0, 0]);
    let mut dec = Decoder::new(&[4, 3, 2, 1]);
    assert_eq!(dec.prim::<u32>(), 0x0102_0304);
    let mut dec = Decoder::new(&[b'A', 0, 0, 0]);
    assert_eq!(dec.prim::<char>(), 'A');
}

#[test]
#[should_panic(expected = "list length")]
fn decoder_list_len_min_rejects_huge_count() {
    let mut dec = Decoder::new(&[0xff; 8]);
    let _ = dec.list_len_min(16);
}

#[test]
#[should_panic(expected = "list length")]
fn decoder_list_elements_rejects_huge_count() {
    let mut dec = Decoder::new(&[0xff; 8]);
    // n = u64::MAX elements, no payload -> must be rejected before allocating
    let _ = dec.list_elements::<u64>(u64::MAX as usize);
}
