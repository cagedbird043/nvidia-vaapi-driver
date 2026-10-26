//! MPEG-2 video (Simple and Main profile): the VA picture parameters, quantiser matrices and slice
//! data become the engine's MPEG-2 setup; slices keep their start codes, as the engine expects.
use super::{read, Frontend, Picture as VaPicture};
use crate::va;
use nvrm::video::{
    legacy::{Field, Handle, LegacyCodec, Picture, Shared},
    mpeg2::{self, Mpeg2, Params},
    CodecDecoder,
};
use std::{cell::RefCell, rc::Rc};

/// Position of every coefficient of a zig-zag scan (ISO 13818-2 Figure 7-2) in raster order.
pub(super) const ZIGZAG: [u8; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// `picture_coding_extension` bits (VA: intra_dc_precision 0-1, picture_structure 2-3,
/// top_field_first 4, frame_pred_frame_dct 5, concealment_motion_vectors 6, q_scale_type 7,
/// intra_vlc_format 8, alternate_scan 9, repeat_first_field 10, progressive_frame 11,
/// is_first_field 12).
const STRUCTURE_SHIFT: u32 = 2;
const TOP_FIELD_FIRST: u32 = 1 << 4;
const FRAME_PRED_FRAME_DCT: u32 = 1 << 5;
const CONCEALMENT_MOTION_VECTORS: u32 = 1 << 6;
const Q_SCALE_TYPE: u32 = 1 << 7;
const INTRA_VLC_FORMAT: u32 = 1 << 8;
const ALTERNATE_SCAN: u32 = 1 << 9;
const IS_FIRST_FIELD: u32 = 1 << 12;
const INTRA: i32 = 1;
const BIDIRECTIONAL: i32 = 3;

struct Front {
    shared: Handle<Params>,
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

impl Frontend for Front {
    fn stage(&mut self, picture: &VaPicture) -> Result<Vec<u8>, String> {
        let p: va::VAPictureParameterBufferMPEG2 = read(&picture.params, "MPEG-2 picture")?;
        let f = unsafe { p.picture_coding_extension.value };
        let structure = ((f >> STRUCTURE_SHIFT) & 3) as u8;
        let kind = p.picture_coding_type;
        if !(INTRA..=BIDIRECTIONAL).contains(&kind) || structure == 0 {
            return Err(format!("MPEG-2 picture type {kind}, structure {structure}"));
        }
        let (intra, inter) = match &picture.iq {
            Some(bytes) => {
                let q: va::VAIQMatrixBufferMPEG2 = read(bytes, "MPEG-2 quantiser matrices")?;
                (
                    raster(q.load_intra_quantiser_matrix, &q.intra_quantiser_matrix, &mpeg2::DEFAULT_INTRA),
                    raster(q.load_non_intra_quantiser_matrix, &q.non_intra_quantiser_matrix, &mpeg2::DEFAULT_INTER),
                )
            }
            None => (mpeg2::DEFAULT_INTRA, mpeg2::DEFAULT_INTER),
        };
        let field = match (structure, f & IS_FIRST_FIELD != 0) {
            (mpeg2::FRAME_PICTURE, _) => Field::Frame,
            (_, true) => Field::First,
            _ => Field::Second,
        };
        let code = p.f_code as u32;
        let params = Params {
            width: u32::from(p.horizontal_size),
            height: u32::from(p.vertical_size),
            picture_structure: structure,
            picture_coding_type: kind as u8,
            intra_dc_precision: (f & 3) as u8,
            frame_pred_frame_dct: f & FRAME_PRED_FRAME_DCT != 0,
            concealment_motion_vectors: f & CONCEALMENT_MOTION_VECTORS != 0,
            intra_vlc_format: f & INTRA_VLC_FORMAT != 0,
            q_scale_type: f & Q_SCALE_TYPE != 0,
            alternate_scan: f & ALTERNATE_SCAN != 0,
            top_field_first: f & TOP_FIELD_FIRST != 0,
            f_code: [(code >> 12) as u8 & 15, (code >> 8) as u8 & 15, (code >> 4) as u8 & 15, code as u8 & 15],
            intra_matrix: intra,
            inter_matrix: inter,
            slice_offsets: picture.slices.iter().map(|s| s.data.start as u32).collect(),
        };
        let forward = (kind != INTRA).then(|| reference(p.forward_reference_picture)).flatten();
        let backward = (kind == BIDIRECTIONAL).then(|| reference(p.backward_reference_picture)).flatten();
        self.shared.borrow_mut().pending = Some(Picture {
            target: picture.target,
            refs: [forward, backward, None],
            field,
            width: params.width,
            height: params.height,
            params,
        });
        Ok(picture.data.clone())
    }
    fn release(&mut self, target: u32) {
        self.shared.borrow_mut().release(target);
    }
}

pub(super) fn create(
    width: u32,
    height: u32,
) -> Result<(Box<dyn Frontend>, Box<dyn CodecDecoder>), String> {
    let shared: Handle<Params> = Rc::new(RefCell::new(Shared::default()));
    let codec = LegacyCodec::new(Mpeg2, shared.clone(), width, height);
    Ok((Box::new(Front { shared }), Box::new(codec)))
}
