//! HEVC Main: VA owns picture/reference state and display order; NVDEC parses slice payloads.
//! All complete slice NALs are retained: NVDEC consumes list modifications, weights,
//! CABAC headers, tile entry points and WPP directly, including multi-slice pictures.
//! VA IQ matrices are raster-order; the RM scaling representation is diagonal-order.
//! PCM, interlaced pictures, non-4:2:0 and non-8-bit surfaces are rejected.
use super::{Frontend, Picture};
use crate::va;
use nvrm::video::{self, bits::{unescape, BitReader}, hevc::{self, codec::HevcDecoder, desc, ps}, Allocator, BuildContext, CodecDecoder, DecodeJob, Job, PictureInfo, VResult, VideoError};
use std::{cell::RefCell, collections::HashMap, rc::Rc};

fn invalid(s: impl Into<String>) -> VideoError { VideoError::Invalid(s.into()) }
fn valid(r: &va::VAPictureHEVC) -> bool { r.picture_id != u32::MAX && r.flags & va::VA_PICTURE_HEVC_INVALID == 0 }
fn flag(v: u32, n: u32) -> bool { v & (1 << n) != 0 }
fn log2(v: u32) -> u32 { if v <= 1 { 0 } else { 32 - (v - 1).leading_zeros() } }
struct Parameters {
    target: u32,
    pic: va::VAPictureParameterBufferHEVC,
    iq: Option<va::VAIQMatrixBufferHEVC>,
    slices: Vec<va::VASliceParameterBufferHEVC>,
    first_nal: Vec<u8>,
}
#[derive(Default)]
struct State {
    pending: Option<Parameters>,
    surfaces: HashMap<u32, (usize, u8)>,
    refs: Vec<usize>,
}
struct Front { state: Rc<RefCell<State>>, width: u32, height: u32 }
struct VaCodec {
    state: Rc<RefCell<State>>, machinery: HevcDecoder,
    pending: Option<Parameters>, bits: Option<Vec<u8>>, slot: u8,
    width: u32, height: u32,
}

impl Parameters {
    fn parameter_sets(&self) -> (ps::Sps, ps::Pps) {
        let p = &self.pic;
        let f = unsafe { p.pic_fields.value };
        let h = unsafe { p.slice_parsing_fields.value };
        let mut scaling = ps::ScalingList::default_lists();
        if let Some(iq) = &self.iq {
            let scan4 = ps::diag_scan(4);
            let scan8 = ps::diag_scan(8);
            for m in 0..6 {
                for (i, &(x, y)) in scan4.iter().enumerate() { scaling.lists[0][m][i] = iq.ScalingList4x4[m][y * 4 + x]; }
                for (i, &(x, y)) in scan8.iter().enumerate() {
                    scaling.lists[1][m][i] = iq.ScalingList8x8[m][y * 8 + x];
                    scaling.lists[2][m][i] = iq.ScalingList16x16[m][y * 8 + x];
                }
                scaling.dc[0][m] = iq.ScalingListDC16x16[m];
            }
            for (j, m) in [0, 3].into_iter().enumerate() {
                for (i, &(x, y)) in scan8.iter().enumerate() { scaling.lists[3][m][i] = iq.ScalingList32x32[j][y * 8 + x]; }
                scaling.dc[1][m] = iq.ScalingListDC32x32[j];
            }
        }
        let sps = ps::Sps {
            id: 0, profile_idc: 1, chroma_format_idc: 1, separate_colour_plane: false,
            width: p.pic_width_in_luma_samples.into(), height: p.pic_height_in_luma_samples.into(), conf_win: [0; 4],
            bit_depth_luma: 8, bit_depth_chroma: 8, log2_max_poc_lsb: u32::from(p.log2_max_pic_order_cnt_lsb_minus4) + 4,
            max_dec_pic_buffering: u32::from(p.sps_max_dec_pic_buffering_minus1) + 1, max_num_reorder_pics: 0,
            log2_min_cb: u32::from(p.log2_min_luma_coding_block_size_minus3) + 3,
            log2_ctb: u32::from(p.log2_min_luma_coding_block_size_minus3) + 3 + u32::from(p.log2_diff_max_min_luma_coding_block_size),
            log2_min_tb: u32::from(p.log2_min_transform_block_size_minus2) + 2,
            log2_max_tb: u32::from(p.log2_min_transform_block_size_minus2) + 2 + u32::from(p.log2_diff_max_min_transform_block_size),
            max_th_depth_inter: p.max_transform_hierarchy_depth_inter.into(), max_th_depth_intra: p.max_transform_hierarchy_depth_intra.into(),
            scaling_list_enabled: flag(f, 4), scaling_list: Some(scaling), amp: flag(f, 6), sao: flag(h, 7), pcm: false,
            pcm_bit_depth_luma: 8, pcm_bit_depth_chroma: 8, log2_min_pcm_cb: 3, log2_max_pcm_cb: 3, pcm_loop_filter_disabled: false,
            st_rps: vec![ps::StRps::default(); p.num_short_term_ref_pic_sets as usize],
            long_term_ref_pics_present: flag(h, 1), num_long_term_ref_pics_sps: p.num_long_term_ref_pic_sps.into(),
            temporal_mvp: flag(h, 2), strong_intra_smoothing: flag(f, 7),
        };
        let pps = ps::Pps {
            id: 0, sps_id: 0, dependent_slice_segments_enabled: flag(h, 5), output_flag_present: flag(h, 4),
            num_extra_slice_header_bits: p.num_extra_slice_header_bits.into(), sign_data_hiding: flag(f, 8), cabac_init_present: flag(h, 3),
            num_ref_idx_l0_default: u32::from(p.num_ref_idx_l0_default_active_minus1) + 1,
            num_ref_idx_l1_default: u32::from(p.num_ref_idx_l1_default_active_minus1) + 1,
            init_qp_minus26: p.init_qp_minus26.into(), constrained_intra_pred: flag(f, 9), transform_skip: flag(f, 5),
            cu_qp_delta: flag(f, 10), diff_cu_qp_delta_depth: p.diff_cu_qp_delta_depth.into(),
            cb_qp_offset: p.pps_cb_qp_offset.into(), cr_qp_offset: p.pps_cr_qp_offset.into(), slice_chroma_qp_offsets_present: flag(h, 6),
            weighted_pred: flag(f, 11), weighted_bipred: flag(f, 12), transquant_bypass: flag(f, 13),
            tiles: flag(f, 14).then(|| ps::Tiles { uniform: false,
                cols: u32::from(p.num_tile_columns_minus1) + 1, rows: u32::from(p.num_tile_rows_minus1) + 1,
                col_minus1: p.column_width_minus1[..p.num_tile_columns_minus1 as usize].iter().map(|&v| u32::from(v)).collect(),
                row_minus1: p.row_height_minus1[..p.num_tile_rows_minus1 as usize].iter().map(|&v| u32::from(v)).collect(), loop_filter_across_tiles: flag(f, 17) }),
            entropy_coding_sync: flag(f, 15), loop_filter_across_slices: flag(f, 16), deblocking_control_present: true,
            deblocking_override_enabled: flag(h, 8), deblocking_disabled: flag(h, 9), beta_offset_div2: p.pps_beta_offset_div2.into(), tc_offset_div2: p.pps_tc_offset_div2.into(),
            scaling_list: None, lists_modification_present: flag(h, 0), log2_parallel_merge_level: u32::from(p.log2_parallel_merge_level_minus2) + 2,
            slice_header_extension_present: flag(h, 10),
        };
        (sps, pps)
    }

    // Only measure syntax absent from VA's descriptor fields; VA remains authoritative for RPS.
    fn rps_skip(&self, sps: &ps::Sps, pps: &ps::Pps) -> VResult<(u16, u32)> {
        let nal = unescape(&self.first_nal);
        let mut r = BitReader::new(&nal[2..]);
        let first = r.flag()?;
        let nut = (nal[0] >> 1) & 63;
        if (16..=23).contains(&nut) { r.skip(1)?; }
        r.ue()?;
        if !first { return Err(invalid("first HEVC slice must start the picture")); }
        r.skip(pps.num_extra_slice_header_bits as usize)?;
        r.ue()?;
        if pps.output_flag_present { r.skip(1)?; }
        if matches!(nut, 19 | 20) { return Ok((0, 0)); }
        let start = r.pos();
        r.skip(sps.log2_max_poc_lsb as usize)?;
        let from_sps = r.flag()?;
        let st_bits = if from_sps { log2(self.pic.num_short_term_ref_pic_sets.into()) } else { self.pic.st_rps_bits };
        r.skip(st_bits as usize)?;
        if sps.long_term_ref_pics_present {
            let ns = if sps.num_long_term_ref_pics_sps > 0 { r.ue()? } else { 0 };
            let np = r.ue()?;
            if ns > sps.num_long_term_ref_pics_sps || ns + np > 15 { return Err(invalid("HEVC long-term reference count exceeds DPB")); }
            for i in 0..ns + np {
                if i < ns { r.skip(log2(sps.num_long_term_ref_pics_sps) as usize)?; }
                else { r.skip(sps.log2_max_poc_lsb as usize + 1)?; }
                if r.flag()? { r.ue()?; }
            }
        }
        Ok((u16::try_from(r.pos() - start).map_err(|_| invalid("HEVC RPS syntax exceeds descriptor"))?, st_bits))
    }
}

impl CodecDecoder for VaCodec {
    fn codec(&self) -> u32 { video::CODEC_HEVC }
    fn bit_depth(&self) -> u32 { 8 }
    fn dpb_size(&self) -> usize { 16 }
    fn allocate(&mut self, a: &mut dyn Allocator, w: u32, h: u32) -> VResult<()> { self.machinery.allocate(a, w, h) }
    fn surfaces_needed(&self, _: &[u8]) -> VResult<usize> { Ok(1) }
    fn push_unit(&mut self, data: &[u8]) -> VResult<()> { self.pending = self.state.borrow_mut().pending.take(); self.bits = Some(data.to_vec()); Ok(()) }
    fn next_job(&mut self) -> VResult<Option<Job>> {
        let Some(bitstream) = self.bits.take() else { return Ok(None); };
        let p = self.pending.as_ref().ok_or_else(|| invalid("missing HEVC VA parameters"))?;
        let keyframe = p.slices.iter().all(|s| (unsafe { s.LongSliceFlags.value } >> 2) & 3 == 2);
        Ok(Some(Job::Decode(DecodeJob { info: PictureInfo { width: p.pic.pic_width_in_luma_samples.into(), height: p.pic.pic_height_in_luma_samples.into(), crop: [0, 0, self.width, self.height], bit_depth: 8, keyframe, ..Default::default() }, show: true, display_key: None, bitstream, readback: None })))
    }
    fn build(&mut self, job: &DecodeJob, ctx: &BuildContext, d: &mut [u8]) -> VResult<Vec<u32>> {
        let p = self.pending.as_ref().ok_or_else(|| invalid("missing HEVC VA picture"))?;
        let (sps, pps) = p.parameter_sets();
        let mut slots = [None; 17];
        let mut used = [false; 17];
        let mut diff = [0i16; 16];
        let mut before = Vec::new(); let mut after = Vec::new(); let mut lt = Vec::new();
        let mut longterm = 0u16;
        let state = self.state.borrow();
        for r in p.pic.ReferenceFrames.iter().filter(|r| valid(r)) {
            let &(surface, slot) = state.surfaces.get(&r.picture_id).ok_or_else(|| invalid("missing HEVC reference surface"))?;
            used[slot as usize] = true; slots[slot as usize] = Some(surface);
            diff[slot as usize] = i16::try_from(i64::from(p.pic.CurrPic.pic_order_cnt) - i64::from(r.pic_order_cnt))
                .map_err(|_| invalid("HEVC POC distance exceeds 16 bits"))?;
            if r.flags & va::VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE != 0 { before.push((r.pic_order_cnt, slot)); }
            if r.flags & va::VA_PICTURE_HEVC_RPS_ST_CURR_AFTER != 0 { after.push((r.pic_order_cnt, slot)); }
            if r.flags & va::VA_PICTURE_HEVC_RPS_LT_CURR != 0 { lt.push((r.pic_order_cnt, slot)); }
            if r.flags & va::VA_PICTURE_HEVC_LONG_TERM_REFERENCE != 0 && slot < 16 { longterm |= 1 << slot; }
        }
        self.slot = used[..16].iter().position(|&u| !u).ok_or_else(|| invalid("no free HEVC colocated slot"))? as u8;
        slots[self.slot as usize] = Some(ctx.current);
        before.sort_by_key(|v| std::cmp::Reverse(v.0)); after.sort_by_key(|v| v.0);
        let total = before.len() + after.len() + lt.len();
        let cyclic = |a: &[(i32, u8)], b: &[(i32, u8)]| {
            std::array::from_fn(|i| {
                if total == 0 { return 0; }
                let j = i % total;
                if j < a.len() { a[j].1 }
                else if j < a.len() + b.len() { b[j - a.len()].1 }
                else { lt[j - a.len() - b.len()].1 }
            })
        };
        let list0 = cyclic(&before, &after); let list1 = cyclic(&after, &before);
        let (skip, st_bits) = p.rps_skip(&sps, &pps)?;
        let h = unsafe { p.pic.slice_parsing_fields.value };
        let st = desc::PicState { stream_len: job.bitstream.len() as u32, idr: flag(h, 12), irap: flag(h, 11), curr_slot: self.slot, num_poc_total_curr: total as u32, list0, list1, diff, sw_hdr_skip_length: skip, num_bits_st_rps: st_bits, num_delta_pocs_ref: 0 };
        let mut setup = desc::build(&sps, &pps, &st, &self.machinery.session_inputs(), desc::StockOpaque::observed(8))?;
        setup[0x72..0x74].copy_from_slice(&longterm.to_le_bytes());
        self.machinery.build_picture(ctx, d, &setup, &slots, hevc::CONTROL_PARAMS_BASE | if total > 0 { u32::from(list0[0]) << 7 } else { 0 })
    }
    fn finish(&mut self, _: &DecodeJob, current: usize, _: Option<&[u8]>) -> VResult<()> {
        let p = self.pending.take().ok_or_else(|| invalid("missing finished HEVC picture"))?;
        self.state.borrow_mut().surfaces.insert(p.target, (current, self.slot)); Ok(())
    }
    fn referenced(&self) -> Vec<usize> { self.state.borrow().refs.clone() }
    fn status_offset(&self) -> Option<usize> { Some(0xd00) }
    fn flush(&mut self) { self.pending = None; self.bits = None; let mut s = self.state.borrow_mut(); s.pending = None; s.refs.clear(); s.surfaces.clear(); }
}

pub(super) fn create(class: u32, width: u32, height: u32) -> Result<(Box<dyn Frontend>, Box<dyn CodecDecoder>), String> {
    let state = Rc::new(RefCell::new(State::default()));
    let machinery = HevcDecoder::new(class, width, height, 8, &[]).map_err(|e| e.to_string())?;
    Ok((Box::new(Front { state: state.clone(), width, height }), Box::new(VaCodec { state, machinery, pending: None, bits: None, slot: 0, width, height })))
}

impl Frontend for Front {
    fn stage(&mut self, picture: &Picture) -> Result<Vec<u8>, String> {
        let pic: va::VAPictureParameterBufferHEVC = super::read(&picture.params, "HEVC picture parameters")?;
        let f = unsafe { pic.pic_fields.value };
        if pic.bit_depth_luma_minus8 != 0 || pic.bit_depth_chroma_minus8 != 0 || f & 7 != 1 { return Err("HEVC requires 8-bit 4:2:0 without separate colour planes".into()); }
        if f & 8 != 0 { return Err("HEVC PCM is unsupported".into()); }
        if pic.CurrPic.flags & (va::VA_PICTURE_HEVC_FIELD_PIC | va::VA_PICTURE_HEVC_BOTTOM_FIELD) != 0 { return Err("interlaced HEVC is unsupported".into()); }
        if pic.CurrPic.picture_id != picture.target { return Err("HEVC CurrPic is not render target".into()); }
        let min_cb = u32::from(pic.log2_min_luma_coding_block_size_minus3) + 3;
        let ctb = min_cb + u32::from(pic.log2_diff_max_min_luma_coding_block_size);
        if min_cb > 6 || ctb > 6 || pic.num_tile_columns_minus1 > 19 || pic.num_tile_rows_minus1 > 21 || pic.num_short_term_ref_pic_sets > 64 || pic.log2_max_pic_order_cnt_lsb_minus4 > 12 { return Err("invalid HEVC block, tile or RPS dimensions".into()); }
        if u32::from(pic.pic_width_in_luma_samples) != self.width.next_multiple_of(1 << min_cb) || u32::from(pic.pic_height_in_luma_samples) != self.height.next_multiple_of(1 << min_cb) { return Err("HEVC coded dimensions do not match context".into()); }
        if picture.slices.is_empty() { return Err("HEVC picture has no slices".into()); }
        let iq = picture.iq.as_ref().map(|b| super::read(b, "HEVC IQ matrix")).transpose()?;
        let mut slices = Vec::with_capacity(picture.slices.len());
        let mut bits = Vec::with_capacity(picture.data.len() + picture.slices.len() * 3);
        let mut first_nal = Vec::new();
        for slice in &picture.slices {
            let s: va::VASliceParameterBufferHEVC = super::read(&slice.param, "HEVC slice parameters")?;
            let nal = &picture.data[slice.data.clone()];
            let prefix = if nal.starts_with(&[0, 0, 0, 1]) { 4 } else if nal.starts_with(&[0, 0, 1]) { 3 } else { 0 };
            let nal = &nal[prefix..];
            if nal.len() < 3 || (nal[0] >> 1) & 63 >= 32 || nal[1] & 7 == 0 || s.slice_data_flag != va::VA_SLICE_DATA_FLAG_ALL { return Err("HEVC requires complete VCL NAL slices".into()); }
            let sf = unsafe { s.LongSliceFlags.value };
            if s.slice_data_byte_offset as usize > nal.len() || (sf >> 2) & 3 > 2 || s.slice_segment_address >= u32::from(pic.pic_width_in_luma_samples).div_ceil(1 << ctb) * u32::from(pic.pic_height_in_luma_samples).div_ceil(1 << ctb) { return Err("invalid HEVC slice header offset, type or CTB address".into()); }
            if slices.is_empty() {
                let header_len = s.slice_data_byte_offset as usize + usize::from(s.slice_data_num_emu_prevn_bytes);
                if header_len < 3 || header_len > nal.len() { return Err("invalid HEVC slice header length".into()); }
                first_nal.extend_from_slice(&nal[..header_len]);
            }
            for list in 0..2 {
                let count = if (sf >> 2) & 3 == 2 || (list == 1 && (sf >> 2) & 3 != 0) { 0 } else if list == 0 { usize::from(s.num_ref_idx_l0_active_minus1) + 1 } else { usize::from(s.num_ref_idx_l1_active_minus1) + 1 };
                if count > 15 { return Err("HEVC slice reference list exceeds 15 entries".into()); }
                for &idx in &s.RefPicList[list][..count] { if idx >= 15 || !valid(&pic.ReferenceFrames[idx as usize]) { return Err("HEVC slice references invalid ReferenceFrames index".into()); } }
            }
            bits.extend_from_slice(&[0, 0, 1]); bits.extend_from_slice(nal); slices.push(s);
        }
        let mut state = self.state.borrow_mut(); let mut refs = Vec::new();
        for r in pic.ReferenceFrames.iter().filter(|r| valid(r)) {
            if r.picture_id == picture.target { return Err("HEVC decode target is also a reference".into()); }
            refs.push(state.surfaces.get(&r.picture_id).ok_or_else(|| format!("unknown HEVC reference surface {}", r.picture_id))?.0);
        }
        state.refs = refs; state.pending = Some(Parameters { target: picture.target, pic, iq, slices, first_nal }); Ok(bits)
    }
    fn release(&mut self, target: u32) { self.state.borrow_mut().surfaces.remove(&target); }
}
