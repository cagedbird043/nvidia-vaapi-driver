//! AV1 VA headers and tile payloads drive the RM descriptor machinery directly.
use super::{read, Frontend, Picture};
use crate::{va, Format};
use nvrm::video::{
    self,
    av1::{
        codec::Av1Decoder,
        desc,
        header::{FrameHeader, SequenceHeader, TileInfo, TileRange},
    },
    Allocator, BuildContext, CodecDecoder, DecodeJob, Job, PictureInfo, VResult, VideoError,
};
use std::{cell::RefCell, collections::HashMap, rc::Rc};

struct Parameters {
    target: u32,
    seq: SequenceHeader,
    hdr: FrameHeader,
    tiles: Vec<TileRange>,
    references: [Option<(usize, usize)>; 8],
}
#[derive(Default)]
struct State {
    pending: Option<Parameters>,
    surfaces: HashMap<u32, (usize, usize)>, // VA id -> session surface, codec slot
    refs: Vec<usize>,
}
struct Front {
    state: Rc<RefCell<State>>,
    width: u32,
    height: u32,
    format: Format,
}
fn flag(word: u32, bit: u32) -> bool {
    word & (1 << bit) != 0
}
fn starts(total: u32, count: u8, uniform: bool, widths: &[u16]) -> Result<Vec<u32>, String> {
    let mut result = vec![0];
    let step = total.div_ceil(u32::from(count).next_power_of_two());
    for i in 0..usize::from(count) {
        let previous = *result.last().unwrap();
        let end = if i + 1 == usize::from(count) {
            total
        } else {
            previous
                + if uniform {
                    step
                } else {
                    u32::from(widths[i]) + 1
                }
        };
        if end <= previous || end > total {
            return Err("AV1 invalid tile geometry".into());
        }
        result.push(end);
    }
    Ok(result)
}
impl Frontend for Front {
    fn stage(&mut self, picture: &Picture) -> Result<Vec<u8>, String> {
        let p: va::VADecPictureParameterBufferAV1 = read(&picture.params, "AV1 picture")?;
        let sf = unsafe { p.seq_info_fields.value };
        let pf = unsafe { p.pic_info_fields.value };
        let mc = unsafe { p.mode_control_fields.value };
        let lf = u32::from(unsafe { p.loop_filter_info_fields.value });
        let qm = u32::from(unsafe { p.qmatrix_fields.value });
        let lr = u32::from(unsafe { p.loop_restoration_fields.value });
        let sg = unsafe { p.seg_info.segment_info_fields.value };
        if p.profile != 0
            || u32::from(p.bit_depth_idx) * 2 + 8 != self.format.bit_depth
            || flag(sf, 10)
            || !flag(sf, 12)
            || !flag(sf, 13)
        {
            return Err(format!(
                "AV1 requires Profile 0, {}-bit 4:2:0 as the context's surfaces (no monochrome)",
                self.format.bit_depth
            ));
        }
        if flag(sf, 15) || unsafe { p.film_grain_info.film_grain_info_fields.value } & 1 != 0 {
            return Err("AV1 film grain is unsupported by the VA backend".into());
        }
        if flag(pf, 16) || p.anchor_frames_num != 0 {
            return Err("AV1 large-scale tiles are unsupported".into());
        }
        let width = u32::from(p.frame_width_minus1) + 1;
        let height = u32::from(p.frame_height_minus1) + 1;
        if width > self.width || height > self.height {
            return Err("AV1 picture exceeds context dimensions".into());
        }
        if p.primary_ref_frame > 7
            || (pf & 1 != 0 && p.ref_frame_idx.iter().any(|&i| i > 7))
            || p.order_hint_bits_minus_1 > 7
            || p.cdef_bits > 3
            || (mc >> 7) & 3 > 2
            || p.interp_filter > 4
        {
            return Err("AV1 invalid header parameters".into());
        }
        if p.tile_cols == 0
            || p.tile_cols > 64
            || p.tile_rows == 0
            || p.tile_rows > 64
            || u32::from(p.tile_cols) * u32::from(p.tile_rows) > 512
        {
            return Err("AV1 invalid tile count".into());
        }
        let seq = SequenceHeader {
            seq_profile: 0,
            bit_depth: self.format.bit_depth as u8,
            subsampling_x: true,
            subsampling_y: true,
            still_picture: flag(sf, 0),
            use_128x128_superblock: flag(sf, 1),
            enable_filter_intra: flag(sf, 2),
            enable_intra_edge_filter: flag(sf, 3),
            enable_interintra_compound: flag(sf, 4),
            enable_masked_compound: flag(sf, 5),
            enable_dual_filter: flag(sf, 6),
            enable_order_hint: flag(sf, 7),
            enable_jnt_comp: flag(sf, 8),
            enable_cdef: flag(sf, 9),
            color_range: flag(sf, 11),
            order_hint_bits: if flag(sf, 7) {
                u32::from(p.order_hint_bits_minus_1) + 1
            } else {
                0
            },
            ..Default::default()
        };
        let mut h = FrameHeader {
            frame_type: (pf & 3) as u8,
            frame_is_intra: pf & 1 == 0,
            show_frame: flag(pf, 2),
            showable_frame: flag(pf, 3),
            error_resilient_mode: flag(pf, 4),
            disable_cdf_update: flag(pf, 5),
            allow_screen_content_tools: flag(pf, 6),
            force_integer_mv: flag(pf, 7),
            allow_intrabc: flag(pf, 8),
            use_superres: flag(pf, 9),
            allow_high_precision_mv: flag(pf, 10),
            is_motion_mode_switchable: flag(pf, 11),
            use_ref_frame_mvs: flag(pf, 12),
            disable_frame_end_update_cdf: flag(pf, 13),
            allow_warped_motion: flag(pf, 15),
            upscaled_width: width,
            frame_width: width,
            frame_height: height,
            render_width: width,
            render_height: height,
            order_hint: u32::from(p.order_hint),
            primary_ref_frame: p.primary_ref_frame,
            // Intra frames carry no reference indices: clients leave them unset (GStreamer: 255).
            ref_frame_idx: if pf & 1 == 0 { [0; 7] } else { p.ref_frame_idx },
            interpolation_filter: p.interp_filter,
            is_filter_switchable: p.interp_filter == 4,
            base_q_idx: p.base_qindex,
            delta_q_y_dc: p.y_dc_delta_q,
            delta_q_u_dc: p.u_dc_delta_q,
            delta_q_u_ac: p.u_ac_delta_q,
            delta_q_v_dc: p.v_dc_delta_q,
            delta_q_v_ac: p.v_ac_delta_q,
            using_qmatrix: flag(qm, 0),
            qm_y: ((qm >> 1) & 15) as u8,
            qm_u: ((qm >> 5) & 15) as u8,
            qm_v: ((qm >> 9) & 15) as u8,
            segmentation_enabled: flag(sg, 0),
            segmentation_update_map: flag(sg, 1),
            segmentation_temporal_update: flag(sg, 2),
            segmentation_update_data: flag(sg, 3),
            delta_q_present: flag(mc, 0),
            delta_q_res: ((mc >> 1) & 3) as u8,
            delta_lf_present: flag(mc, 3),
            delta_lf_res: ((mc >> 4) & 3) as u8,
            delta_lf_multi: flag(mc, 6),
            tx_mode: ((mc >> 7) & 3) as u8,
            reference_select: flag(mc, 9),
            reduced_tx_set: flag(mc, 10),
            skip_mode_present: flag(mc, 11),
            loop_filter_level: [
                p.filter_level[0],
                p.filter_level[1],
                p.filter_level_u,
                p.filter_level_v,
            ],
            loop_filter_sharpness: (lf & 7) as u8,
            loop_filter_delta_enabled: flag(lf, 3),
            loop_filter_ref_deltas: p.ref_deltas,
            loop_filter_mode_deltas: p.mode_deltas,
            cdef_damping_minus_3: p.cdef_damping_minus_3,
            cdef_bits: p.cdef_bits,
            frame_restoration_type: [(lr & 3) as u8, ((lr >> 2) & 3) as u8, ((lr >> 4) & 3) as u8],
            lr_unit_shift: ((lr >> 6) & 3) as u8,
            lr_uv_shift: ((lr >> 8) & 1) as u8,
            ..Default::default()
        };
        if h.use_superres {
            if !h.frame_is_intra {
                return Err("AV1 inter-frame superres is unsupported".into());
            }
            if !(9..=16).contains(&p.superres_scale_denominator) {
                return Err("AV1 invalid superres denominator".into());
            }
            h.superres_denom = u32::from(p.superres_scale_denominator);
            h.frame_width = (width * 8 + h.superres_denom / 2) / h.superres_denom;
        }
        h.mi_cols = 2 * h.frame_width.div_ceil(8);
        h.mi_rows = 2 * height.div_ceil(8);
        h.coded_lossless = true;
        for s in 0..8 {
            for f in 0..8 {
                h.feature_enabled[s][f] =
                    h.segmentation_enabled && p.seg_info.feature_mask[s] & (1 << f) != 0;
                if h.feature_enabled[s][f] {
                    h.feature_data[s][f] = p.seg_info.feature_data[s][f];
                    h.last_active_seg_id = s as u8;
                    h.seg_id_pre_skip |= f >= 5;
                }
            }
            let q = (i32::from(h.base_q_idx) + i32::from(h.feature_data[s][0])).clamp(0, 255);
            h.coded_lossless &= q == 0
                && [
                    h.delta_q_y_dc,
                    h.delta_q_u_dc,
                    h.delta_q_u_ac,
                    h.delta_q_v_dc,
                    h.delta_q_v_ac,
                ]
                .iter()
                .all(|&v| v == 0);
            h.cdef_y_pri[s] = p.cdef_y_strengths[s] >> 2;
            h.cdef_y_sec[s] = p.cdef_y_strengths[s] & 3;
            h.cdef_uv_pri[s] = p.cdef_uv_strengths[s] >> 2;
            h.cdef_uv_sec[s] = p.cdef_uv_strengths[s] & 3;
        }
        h.all_lossless = h.coded_lossless && !h.use_superres;
        for i in 0..7 {
            h.gm_type[i + 1] = p.wm[i].wmtype as u8;
            h.gm_params[i + 1].copy_from_slice(&p.wm[i].wmmat[..6]);
        }
        let sb = if seq.use_128x128_superblock { 128 } else { 64 };
        h.tile = TileInfo {
            tile_cols: u32::from(p.tile_cols),
            tile_rows: u32::from(p.tile_rows),
            tile_cols_log2: u32::from(p.tile_cols).next_power_of_two().ilog2(),
            tile_rows_log2: u32::from(p.tile_rows).next_power_of_two().ilog2(),
            col_start_sb: starts(
                h.frame_width.div_ceil(sb),
                p.tile_cols,
                flag(pf, 14),
                &p.width_in_sbs_minus_1,
            )?,
            row_start_sb: starts(
                height.div_ceil(sb),
                p.tile_rows,
                flag(pf, 14),
                &p.height_in_sbs_minus_1,
            )?,
            context_update_tile_id: u32::from(p.context_update_tile_id),
            ..Default::default()
        };
        let count = usize::from(p.tile_cols) * usize::from(p.tile_rows);
        if usize::from(p.context_update_tile_id) >= count || picture.slices.len() != count {
            return Err("AV1 missing tiles or invalid context tile".into());
        }
        let mut tiles = vec![TileRange { start: 0, end: 0 }; count];
        let mut bits = Vec::new();
        for slice in &picture.slices {
            let t: va::VASliceParameterBufferAV1 = read(&slice.param, "AV1 tile")?;
            if t.tile_row >= u16::from(p.tile_rows)
                || t.tile_column >= u16::from(p.tile_cols)
                || t.slice_data_flag != 0
            {
                return Err("AV1 invalid or fragmented tile".into());
            }
            let i = usize::from(t.tile_row) * usize::from(p.tile_cols) + usize::from(t.tile_column);
            if tiles[i].end != 0 {
                return Err("AV1 duplicate tile".into());
            }
            let bytes = picture
                .data
                .get(slice.data.clone())
                .ok_or("AV1 tile range out of bounds")?;
            if bytes.is_empty() || bytes.len() != t.slice_data_size as usize {
                return Err("AV1 tile size mismatch".into());
            }
            let start = u32::try_from(bits.len()).map_err(|_| "AV1 bitstream too large")?;
            bits.extend_from_slice(bytes);
            tiles[i] = TileRange {
                start,
                end: u32::try_from(bits.len()).map_err(|_| "AV1 bitstream too large")?,
            };
        }
        let mut state = self.state.borrow_mut();
        let mut references = [None; 8];
        for (i, &id) in p.ref_frame_map.iter().enumerate() {
            if id != u32::MAX {
                let (surface, slot) = *state
                    .surfaces
                    .get(&id)
                    .ok_or_else(|| format!("AV1 missing reference surface {id}"))?;
                references[i] = Some((slot, surface));
            }
        }
        state.refs = references.iter().flatten().map(|r| r.1).collect();
        state.refs.sort_unstable();
        state.refs.dedup();
        state.pending = Some(Parameters {
            target: picture.target,
            seq,
            hdr: h,
            tiles,
            references,
        });
        Ok(bits)
    }
    fn release(&mut self, target: u32) {
        self.state.borrow_mut().surfaces.remove(&target);
    }
}
struct VaCodec {
    state: Rc<RefCell<State>>,
    machinery: Av1Decoder,
    pending: Option<Parameters>,
    bits: Option<Vec<u8>>,
    slot: usize,
    format: Format,
}
fn invalid(s: &str) -> VideoError {
    VideoError::Invalid(s.into())
}
impl CodecDecoder for VaCodec {
    fn codec(&self) -> u32 {
        video::CODEC_AV1
    }
    fn bit_depth(&self) -> u32 {
        self.format.bit_depth
    }
    fn dpb_size(&self) -> usize {
        8
    }
    fn allocate(&mut self, a: &mut dyn Allocator, w: u32, h: u32) -> VResult<()> {
        self.machinery.allocate(a, w, h)
    }
    fn surfaces_needed(&self, _: &[u8]) -> VResult<usize> {
        Ok(1)
    }
    fn push_unit(&mut self, bits: &[u8]) -> VResult<()> {
        self.pending = self.state.borrow_mut().pending.take();
        self.bits = Some(bits.to_vec());
        Ok(())
    }
    fn next_job(&mut self) -> VResult<Option<Job>> {
        let Some(bitstream) = self.bits.take() else {
            return Ok(None);
        };
        let p = self
            .pending
            .as_ref()
            .ok_or_else(|| invalid("AV1 missing VA parameters"))?;
        self.slot = self.machinery.build_picture(
            p.seq.clone(),
            p.hdr.clone(),
            bitstream.clone(),
            p.tiles.clone(),
            p.references,
        )?;
        Ok(Some(Job::Decode(DecodeJob {
            info: PictureInfo {
                width: p.hdr.upscaled_width,
                height: p.hdr.frame_height,
                crop: [0, 0, p.hdr.upscaled_width, p.hdr.frame_height],
                bit_depth: self.format.bit_depth,
                keyframe: p.hdr.frame_type == 0,
                full_range: p.seq.color_range,
                ..Default::default()
            },
            show: true,
            display_key: None,
            bitstream,
            readback: None,
        })))
    }
    fn build(&mut self, job: &DecodeJob, ctx: &BuildContext, d: &mut [u8]) -> VResult<Vec<u32>> {
        self.machinery.build(job, ctx, d)
    }
    fn finish(&mut self, job: &DecodeJob, current: usize, data: Option<&[u8]>) -> VResult<()> {
        self.machinery.finish(job, current, data)?;
        let p = self
            .pending
            .take()
            .ok_or_else(|| invalid("AV1 finish without picture"))?;
        let mut state = self.state.borrow_mut();
        state
            .surfaces
            .retain(|_, value| value.1 != self.slot && value.0 != current);
        state.surfaces.insert(p.target, (current, self.slot));
        Ok(())
    }
    fn referenced(&self) -> Vec<usize> {
        self.state.borrow().refs.clone()
    }
    fn max_referenced(&self) -> Option<usize> {
        Some(8)
    }
    fn status_offset(&self) -> Option<usize> {
        Some(desc::DESC_STATUS)
    }
    fn flush(&mut self) {
        self.machinery.flush();
        self.pending = None;
        self.bits = None;
        let mut state = self.state.borrow_mut();
        state.pending = None;
        state.surfaces.clear();
        state.refs.clear();
    }
}
pub(super) fn create(
    _class: u32,
    width: u32,
    height: u32,
    format: Format,
) -> Result<(Box<dyn Frontend>, Box<dyn CodecDecoder>), String> {
    let state = Rc::new(RefCell::new(State::default()));
    let machinery = Av1Decoder::new(width, height, format.bit_depth, &[]).map_err(|e| e.to_string())?;
    Ok((
        Box::new(Front {
            state: state.clone(),
            width,
            height,
            format,
        }),
        Box::new(VaCodec {
            state,
            machinery,
            pending: None,
            bits: None,
            slot: 0,
            format,
        }),
    ))
}
