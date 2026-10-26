//! VP9 profile 0: VA owns surface identity and display order; RM retains the
//! four probability contexts and parses the complete frame's compressed header.
use super::{Frontend, Picture};
use crate::va;
use nv_bionic_native::video::{
    self, vp9::{VaPicture, Vp9Decoder}, Allocator, BuildContext, CodecDecoder,
    DecodeJob, Job, VResult, VideoError,
};
use std::{cell::RefCell, collections::HashMap, rc::Rc};

#[derive(Default)]
struct State {
    pending: Option<(u32, VaPicture)>,
    surfaces: HashMap<u32, (usize, u32, u32)>,
    refs: Vec<usize>,
}
struct Front {
    state: Rc<RefCell<State>>,
    width: u32,
    height: u32,
}
impl Frontend for Front {
    fn stage(&mut self, picture: &Picture) -> Result<Vec<u8>, String> {
        let p: va::VADecPictureParameterBufferVP9 = super::read(&picture.params, "VP9 picture")?;
        let f = unsafe { p.pic_fields.value };
        if p.profile != 0 || p.bit_depth != 8 || f & 3 != 3 {
            return Err("VP9 supports only profile 0, 8-bit 4:2:0".into());
        }
        let (w, h) = (u32::from(p.frame_width), u32::from(p.frame_height));
        if w == 0 || h == 0 || w > self.width || h > self.height {
            return Err(format!("VP9 coded size {w}x{h} exceeds context {}x{}", self.width, self.height));
        }
        if p.log2_tile_columns > 6 || p.log2_tile_rows > 2 {
            return Err("VP9 invalid tile layout".into());
        }
        if picture.slices.len() != 1 {
            return Err("VP9 requires one complete frame slice".into());
        }
        let slice = &picture.slices[0];
        let s: va::VASliceParameterBufferVP9 = super::read(&slice.param, "VP9 slice")?;
        if s.slice_data_flag != 0 {
            return Err("VP9 partial frame slices are unsupported".into());
        }
        let bits = picture.data.get(slice.data.clone()).ok_or("VP9 slice data outside buffer")?;
        if bits.len() != s.slice_data_size as usize || bits.is_empty() {
            return Err("VP9 invalid frame byte count".into());
        }
        let mut state = self.state.borrow_mut();
        let mut refs = [None; 8];
        let inter = f & (1 << 2) != 0 && f & (1 << 5) == 0;
        for (i, &id) in p.reference_frames.iter().enumerate() {
            if id != u32::MAX {
                refs[i] = Some(*state.surfaces.get(&id).ok_or_else(|| format!("VP9 missing VA reference surface {id}"))?);
            }
        }
        if inter {
            for index in [(f >> 19) & 7, (f >> 23) & 7, (f >> 27) & 7] {
                if refs[index as usize].is_none() {
                    return Err(format!("VP9 missing active reference slot {index}"));
                }
            }
        }
        state.refs = refs.iter().flatten().map(|r| r.0).collect();
        state.pending = Some((picture.target, VaPicture {
            references: refs, width: w, height: h, fields: f,
            filter_level: p.filter_level, sharpness: p.sharpness_level,
            tile_cols: u32::from(p.log2_tile_columns), tile_rows: u32::from(p.log2_tile_rows),
            uncompressed_size: usize::from(p.frame_header_length_in_bytes),
            header_size: usize::from(p.first_partition_size),
            tree_probs: p.mb_segment_tree_probs, pred_probs: p.segment_pred_probs,
            segment_flags: s.seg_param.map(|s| unsafe { s.segment_flags.value }),
        }));
        Ok(bits.to_vec())
    }
    fn release(&mut self, target: u32) {
        self.state.borrow_mut().surfaces.remove(&target);
    }
}
struct VaCodec {
    state: Rc<RefCell<State>>,
    decoder: Vp9Decoder,
    current: Option<(u32, u32, u32)>,
}
impl CodecDecoder for VaCodec {
    fn codec(&self) -> u32 { video::CODEC_VP9 }
    fn bit_depth(&self) -> u32 { 8 }
    fn dpb_size(&self) -> usize { 8 }
    fn allocate(&mut self, a: &mut dyn Allocator, w: u32, h: u32) -> VResult<()> {
        self.decoder.allocate(a, w, h)
    }
    fn surfaces_needed(&self, _: &[u8]) -> VResult<usize> { Ok(1) }
    fn push_unit(&mut self, bits: &[u8]) -> VResult<()> {
        let (target, params) = self.state.borrow_mut().pending.take()
            .ok_or_else(|| VideoError::Invalid("VP9 missing VA picture".into()))?;
        self.current = Some((target, params.width, params.height));
        self.decoder.set_va_picture(params);
        self.decoder.push_unit(bits)
    }
    fn next_job(&mut self) -> VResult<Option<Job>> { self.decoder.next_job() }
    fn build(&mut self, job: &DecodeJob, ctx: &BuildContext, d: &mut [u8]) -> VResult<Vec<u32>> {
        self.decoder.build(job, ctx, d)
    }
    fn finish(&mut self, job: &DecodeJob, surface: usize, counts: Option<&[u8]>) -> VResult<()> {
        self.decoder.finish(job, surface, counts)?;
        let (target, w, h) = self.current.take().ok_or_else(|| VideoError::Invalid("VP9 missing target".into()))?;
        self.state.borrow_mut().surfaces.insert(target, (surface, w, h));
        Ok(())
    }
    fn referenced(&self) -> Vec<usize> { self.state.borrow().refs.clone() }
    fn flush(&mut self) {
        self.decoder.flush();
        self.current = None;
        *self.state.borrow_mut() = State::default();
    }
}
pub(super) fn create(class: u32, width: u32, height: u32) -> Result<(Box<dyn Frontend>, Box<dyn CodecDecoder>), String> {
    let state = Rc::new(RefCell::new(State::default()));
    Ok((Box::new(Front { state: state.clone(), width, height }),
        Box::new(VaCodec { state, decoder: Vp9Decoder::new(class, width, height, 8), current: None })))
}
