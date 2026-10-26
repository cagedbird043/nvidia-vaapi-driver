//! MPEG-4 part 2 (Simple, Advanced Simple, Main without sprites): the engine parses the VOP header
//! and macroblock layer itself, so the bitstream handed over starts at the VOP start code (a VA
//! client passes the whole packet, VOL headers included) and the VA parameters supply the video
//! object layer syntax and the per-VOP values the engine does not read.
use super::{mpeg2::ZIGZAG, read, Frontend, Picture as VaPicture};
use crate::va;
use nvrm::video::{
    legacy::{Field, Handle, LegacyCodec, Picture, Shared},
    mpeg4::{self, Mpeg4, Params},
    CodecDecoder,
};
use std::{cell::RefCell, rc::Rc};

/// ISO 14496-2 6.3.11 default quantiser matrices, raster order.
const DEFAULT_INTRA: [u8; 64] = [
    8, 17, 18, 19, 21, 23, 25, 27, 17, 18, 19, 21, 23, 25, 27, 28, 20, 21, 22, 23, 24, 26, 28, 30,
    21, 22, 23, 24, 26, 28, 30, 32, 22, 23, 24, 26, 28, 30, 32, 35, 23, 24, 26, 28, 30, 32, 35, 38,
    25, 26, 28, 30, 32, 35, 38, 41, 27, 28, 30, 32, 35, 38, 41, 45,
];
const DEFAULT_INTER: [u8; 64] = [
    16, 17, 18, 19, 20, 21, 22, 23, 17, 18, 19, 20, 21, 22, 23, 24, 18, 19, 20, 21, 22, 23, 24, 25,
    19, 20, 21, 22, 23, 24, 26, 27, 20, 21, 22, 23, 25, 26, 27, 28, 21, 22, 23, 24, 26, 27, 28, 30,
    22, 23, 24, 26, 27, 28, 30, 31, 23, 24, 25, 27, 28, 30, 31, 33,
];
/// Matrices of a stream without loaded ones are unused by the dequantiser the engine then runs.
const FLAT: [u8; 64] = [16; 64];
/// `vop_coding_type` of an S (sprite) VOP.
const VOP_SPRITE: u32 = 3;
const VOP_START_CODE: [u8; 4] = [0, 0, 1, 0xb6];

/// The VOP time stamps the header rebuild writes: VA carries the distances between references
/// (`TRD`) and from the forward one (`TRB`), so the stream gets a clock that reproduces them, with
/// the same bookkeeping as a decoder (seconds counted from the previous reference for B-VOPs).
#[derive(Default)]
struct Clock {
    time_base: i64,
    last_time_base: i64,
    last_ref: i64,
    forward_ref: i64,
}

impl Clock {
    /// `(modulo_time_base ones, vop_time_increment)` of a VOP.
    fn stamp(&mut self, kind: u8, trd: i64, trb: i64, resolution: i64) -> (i64, i64) {
        if kind == mpeg4::PTYPE_B {
            let time = self.forward_ref + trb;
            ((time / resolution - self.last_time_base).max(0), time % resolution)
        } else {
            let time = self.last_ref + trd.max(0);
            self.forward_ref = self.last_ref;
            self.last_ref = time;
            if kind == mpeg4::PTYPE_I {
                // As after a group-of-VOP header (stripped from what VA clients pass): an I-VOP
                // restarts the seconds count; the engine mishandles a modulo_time_base there.
                self.time_base = time / resolution;
                self.last_time_base = self.time_base;
                return (0, time % resolution);
            }
            self.last_time_base = self.time_base;
            self.time_base = (time / resolution).max(self.time_base);
            (self.time_base - self.last_time_base, time % resolution)
        }
    }
}

struct Front {
    shared: Handle<Params>,
    clock: Clock,
}

/// A matrix the client sent in zig-zag order, or the default when it did not load one.
fn raster(load: i32, zigzag: &[u8; 64], default: &[u8; 64]) -> [u8; 64] {
    if load == 0 {
        return *default;
    }
    let mut out = [0; 64];
    for (i, &v) in zigzag.iter().enumerate() {
        out[usize::from(ZIGZAG[i])] = v;
    }
    out
}

fn reference(id: u32) -> Option<u32> {
    (id != va::VA_INVALID_SURFACE).then_some(id)
}

/// Appends bits to a byte vector, most significant bit first.
struct BitWriter {
    bytes: Vec<u8>,
    /// Bits used in the last byte (0 = the next bit opens a new byte).
    used: u32,
}

impl BitWriter {
    fn put(&mut self, value: u32, n: usize) {
        for i in (0..n).rev() {
            if self.used == 0 {
                self.bytes.push(0);
            }
            let bit = (value >> i & 1) as u8;
            *self.bytes.last_mut().unwrap() |= bit << (7 - self.used);
            self.used = (self.used + 1) % 8;
        }
    }
    /// Appends `data` from bit `skip` of its first byte.
    fn put_from(&mut self, data: &[u8], skip: u32) {
        let Some((&head, rest)) = data.split_first() else { return };
        self.put(u32::from(head) & (0xff >> skip), (8 - skip) as usize);
        if self.used == 0 {
            self.bytes.extend_from_slice(rest);
            return;
        }
        let shift = self.used;
        let last = self.bytes.len() - 1;
        let mut carry = self.bytes[last];
        self.bytes.truncate(last);
        for &b in rest {
            self.bytes.push(carry | b >> shift);
            carry = b << (8 - shift);
        }
        self.bytes.push(carry);
    }
}

/// The engine reads the VOP header itself, but VA clients only pass the data after it (from the
/// byte holding the first macroblock bit): rebuild the header (ISO 14496-2 6.2.5) from the
/// parameters and append the macroblock data from its bit offset. 
fn vop_stream(
    p: &va::VAPictureParameterBufferMPEG4,
    v: &va::_VAPictureParameterBufferMPEG4__bindgen_ty_1__bindgen_ty_1,
    f: &va::_VAPictureParameterBufferMPEG4__bindgen_ty_2__bindgen_ty_1,
    time_bits: usize,
    time: (i64, i64),
    quant_scale: i32,
    macroblock_offset: u32,
    data: &[u8],
) -> Result<Vec<u8>, String> {
    if macroblock_offset > 7 || v.short_video_header() != 0 || v.data_partitioned() != 0 || v.reversible_vlc() != 0 {
        return Err("MPEG-4 short header, data partitioning, reversible VLC or unaligned macroblock offset".into());
    }
    let mut w = BitWriter { bytes: VOP_START_CODE.to_vec(), used: 0 };
    let kind = f.vop_coding_type();
    w.put(kind, 2);
    for _ in 0..time.0 {
        w.put(1, 1); // modulo_time_base
    }
    w.put(0, 1);
    w.put(1, 1);
    w.put(time.1 as u32, time_bits); // vop_time_increment
    w.put(1, 1);
    w.put(1, 1); // vop_coded
    if kind == u32::from(mpeg4::PTYPE_P) {
        w.put(f.vop_rounding_type(), 1);
    }
    w.put(f.intra_dc_vlc_thr(), 3);
    if v.interlaced() != 0 {
        w.put(f.top_field_first(), 1);
        w.put(f.alternate_vertical_scan_flag(), 1);
    }
    w.put(quant_scale as u32, p.quant_precision.into());
    if kind != u32::from(mpeg4::PTYPE_I) {
        w.put(p.vop_fcode_forward.into(), 3);
    }
    if kind == u32::from(mpeg4::PTYPE_B) {
        w.put(p.vop_fcode_backward.into(), 3);
    }
    w.put_from(data, macroblock_offset);
    Ok(w.bytes)
}

impl Frontend for Front {
    fn stage(&mut self, picture: &VaPicture) -> Result<Vec<u8>, String> {
        let p: va::VAPictureParameterBufferMPEG4 = read(&picture.params, "MPEG-4 picture")?;
        let (v, f) = unsafe { (p.vol_fields.bits, p.vop_fields.bits) };
        let kind = f.vop_coding_type();
        if kind >= VOP_SPRITE || v.sprite_enable() != 0 {
            return Err("MPEG-4 sprite (GMC) VOPs".into());
        }
        let (intra, inter) = match &picture.iq {
            Some(bytes) => {
                let q: va::VAIQMatrixBufferMPEG4 = read(bytes, "MPEG-4 quantiser matrices")?;
                (
                    raster(q.load_intra_quant_mat, &q.intra_quant_mat, &DEFAULT_INTRA),
                    raster(q.load_non_intra_quant_mat, &q.non_intra_quant_mat, &DEFAULT_INTER),
                )
            }
            None if v.quant_type() != 0 => (DEFAULT_INTRA, DEFAULT_INTER),
            None => (FLAT, FLAT),
        };
        let slice = picture.slices.first().ok_or("MPEG-4 picture without slice data")?;
        let first: va::VASliceParameterBufferMPEG4 = read(&slice.param, "MPEG-4 slice")?;
        let mut data = Vec::with_capacity(picture.data.len());
        for slice in &picture.slices {
            data.extend_from_slice(picture.data.get(slice.data.clone()).ok_or("MPEG-4 slice data outside buffer")?);
        }
        let ptype = kind as u8;
        let resolution = u32::from(p.vop_time_increment_resolution);
        let time_bits = (resolution.saturating_sub(1).max(1).ilog2() + 1) as usize;
        let time = self.clock.stamp(ptype, p.TRD.into(), p.TRB.into(), resolution.max(1).into());
        let bitstream = vop_stream(&p, &v, &f, time_bits, time, first.quant_scale, first.macroblock_offset, &data)?;
        let params = Params {
            width: u32::from(p.vop_width),
            height: u32::from(p.vop_height),
            time_increment_bits: time_bits as u8,
            resync_marker_disable: v.resync_marker_disable() != 0,
            interlaced: v.interlaced() != 0,
            quant_type: v.quant_type() != 0,
            quarter_sample: v.quarter_sample() != 0,
            short_video_header: v.short_video_header() != 0,
            ptype,
            rounding: f.vop_rounding_type() != 0,
            alternate_scan: f.alternate_vertical_scan_flag() != 0,
            top_field_first: f.top_field_first() != 0,
            fcode: [p.vop_fcode_forward, p.vop_fcode_backward],
            trd: p.TRD.max(0) as u16,
            trb: p.TRB.max(0) as u16,
            intra_matrix: intra,
            inter_matrix: inter,
        };
        let forward = (ptype != mpeg4::PTYPE_I).then(|| reference(p.forward_reference_picture)).flatten();
        let backward = (ptype == mpeg4::PTYPE_B).then(|| reference(p.backward_reference_picture)).flatten();
        self.shared.borrow_mut().pending = Some(Picture {
            target: picture.target,
            refs: [forward, backward, None],
            field: Field::Frame,
            width: params.width,
            height: params.height,
            params,
        });
        Ok(bitstream)
    }
    fn release(&mut self, target: u32) {
        self.shared.borrow_mut().release(target);
    }
}

pub(super) fn create(width: u32, height: u32) -> Result<(Box<dyn Frontend>, Box<dyn CodecDecoder>), String> {
    let shared: Handle<Params> = Rc::new(RefCell::new(Shared::default()));
    let codec = LegacyCodec::new(Mpeg4::default(), shared.clone(), width, height);
    Ok((Box::new(Front { shared, clock: Clock::default() }), Box::new(codec)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bit(data: &[u8], i: usize) -> u32 {
        u32::from(data[i / 8] >> (7 - i % 8) & 1)
    }

    /// Appending data from any bit offset after any header length equals appending bit by bit.
    #[test]
    fn put_from_matches_bitwise_append() {
        let data: Vec<u8> = (0..37u32).map(|i| (i * 73 + 11) as u8).collect();
        for header in 0..24 {
            for skip in 0..8 {
                let mut fast = BitWriter { bytes: Vec::new(), used: 0 };
                let mut slow = BitWriter { bytes: Vec::new(), used: 0 };
                for w in [&mut fast, &mut slow] {
                    for i in 0..header {
                        w.put(u32::from(i % 3 == 0), 1);
                    }
                }
                fast.put_from(&data, skip);
                for i in skip as usize..data.len() * 8 {
                    slow.put(bit(&data, i), 1);
                }
                assert_eq!(fast.bytes, slow.bytes, "header {header} bits, skip {skip}");
            }
        }
    }
}
