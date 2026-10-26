//! VC-1 (Simple, Main, Advanced): the engine parses the picture layer from the slice data, so the
//! frontend forwards the sequence and entry-point syntax and the slices; Advanced slices carry the
//! start code the engine expects.
use super::{read, Frontend, Picture as VaPicture};
use crate::va;
use nvrm::video::{
    legacy::{Field, Handle, LegacyCodec, Picture, Shared},
    vc1::{Params, Vc1},
    CodecDecoder,
};
use std::{cell::RefCell, rc::Rc};

/// `sequence_fields.profile` of an Advanced profile stream.
const PROFILE_ADVANCED: u32 = 3;
const FCM_FIELD_INTERLACE: u32 = 2;
/// Picture types (VA `picture_type`, PTYPE): I, P, B, BI, skipped P.
const PTYPE_I: u32 = 0;
const PTYPE_P: u32 = 1;
const PTYPE_B: u32 = 2;
const PTYPE_BI: u32 = 3;
/// FPTYPE of an interlaced field pair: type of its first and second field.
const FIRST_FIELD: [u32; 8] = [PTYPE_I, PTYPE_I, PTYPE_P, PTYPE_P, PTYPE_B, PTYPE_B, PTYPE_BI, PTYPE_BI];
const SECOND_FIELD: [u32; 8] = [PTYPE_I, PTYPE_P, PTYPE_I, PTYPE_P, PTYPE_B, PTYPE_BI, PTYPE_B, PTYPE_BI];
/// Start code suffixes: frame, second field, slice.
const START_FRAME: u8 = 0x0d;
const START_FIELD: u8 = 0x0c;
const START_SLICE: u8 = 0x0b;

struct Front {
    shared: Handle<Params>,
}

fn reference(id: u32) -> Option<u32> {
    (id != va::VA_INVALID_SURFACE).then_some(id)
}

impl Frontend for Front {
    fn stage(&mut self, picture: &VaPicture) -> Result<Vec<u8>, String> {
        let p: va::VAPictureParameterBufferVC1 = read(&picture.params, "VC-1 picture")?;
        let (s, pic, e, mv, q, t) = unsafe {
            (
                p.sequence_fields.bits,
                p.picture_fields.bits,
                p.entrypoint_fields.bits,
                p.mv_fields.bits,
                p.pic_quantizer_fields.bits,
                p.transform_fields.bits,
            )
        };
        let advanced = s.profile() == PROFILE_ADVANCED;
        let fcm = pic.frame_coding_mode();
        if fcm > FCM_FIELD_INTERLACE || fcm != 0 && !(s.interlace() != 0 && advanced) {
            return Err(format!("VC-1 frame coding mode {fcm} outside an interlaced Advanced stream"));
        }
        let fields = s.interlace() != 0 && fcm == FCM_FIELD_INTERLACE;
        let second = fields && pic.is_first_field() == 0;
        let mut kind = pic.picture_type();
        if fields {
            let table = if second { &SECOND_FIELD } else { &FIRST_FIELD };
            kind = *table.get(kind as usize).ok_or("VC-1 field pair type")?;
        }
        let mut bitstream = Vec::with_capacity(picture.data.len() + 4 * picture.slices.len());
        let mut offsets = Vec::with_capacity(picture.slices.len());
        for (i, slice) in picture.slices.iter().enumerate() {
            let data = picture.data.get(slice.data.clone()).ok_or("VC-1 slice data outside buffer")?;
            offsets.push(bitstream.len() as u32);
            if advanced && !data.starts_with(&[0, 0, 1]) {
                let marker = match (i, second) {
                    (0, false) => START_FRAME,
                    (0, true) => START_FIELD,
                    _ => START_SLICE,
                };
                bitstream.extend_from_slice(&[0, 0, 1, marker]);
            }
            bitstream.extend_from_slice(data);
        }
        let params = Params {
            width: u32::from(p.coded_width),
            height: u32::from(p.coded_height),
            advanced,
            postproc: p.post_processing != 0,
            pulldown: s.pulldown() != 0,
            interlace: s.interlace() != 0,
            tfcntr: s.tfcntrflag() != 0,
            finterp: s.finterpflag() != 0,
            psf: s.psf() != 0,
            multires: s.multires() != 0,
            syncmarker: s.syncmarker() != 0,
            rangered: s.rangered() != 0,
            max_b_frames: s.max_b_frames() as u8,
            dquant: q.dquant() as u8,
            panscan: e.panscan_flag() != 0,
            refdist: unsafe { p.reference_fields.bits }.reference_distance_flag() != 0,
            quantizer: q.quantizer() as u8,
            extended_mv: mv.extended_mv_flag() != 0,
            extended_dmv: mv.extended_dmv_flag() != 0,
            overlap: s.overlap() != 0,
            vstransform: t.variable_sized_transform_flag() != 0,
            loopfilter: e.loopfilter() != 0,
            fastuvmc: p.fast_uvmc_flag != 0,
            b_picture: kind == PTYPE_B || kind == PTYPE_BI,
            slice_offsets: offsets,
        };
        let field = match (fields, second) {
            (false, _) => Field::Frame,
            (true, false) => Field::First,
            (true, true) => Field::Second,
        };
        let forward = (kind != PTYPE_I && kind != PTYPE_BI).then(|| reference(p.forward_reference_picture)).flatten();
        let backward = params.b_picture.then(|| reference(p.backward_reference_picture)).flatten();
        self.shared.borrow_mut().pending = Some(Picture {
            target: picture.target,
            refs: [forward, backward, None],
            field,
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
    let codec = LegacyCodec::new(Vc1::default(), shared.clone(), width, height);
    Ok((Box::new(Front { shared }), Box::new(codec)))
}
