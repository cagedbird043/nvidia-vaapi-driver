//! VP8 (version 0 to 3): the engine parses the frame from its first partition on, so the frontend
//! passes the slice data and names the references.
use super::{read, Frontend, Picture as VaPicture};
use crate::va;
use nvrm::video::{
    legacy::{Field, Handle, LegacyCodec, Picture, Shared},
    vp8::{Params, Vp8},
    CodecDecoder,
};
use std::{cell::RefCell, rc::Rc};

/// `pic_fields`: key_frame 0 (0 = key frame, as in the frame tag), version 1-3,
/// segmentation_enabled 4, update_mb_segmentation_map 5, update_segment_feature_data 6.
const KEY_FRAME: u32 = 1;
const VERSION_SHIFT: u32 = 1;
const SEGMENTATION_ENABLED: u32 = 1 << 4;
const UPDATE_SEGMENT_FEATURE_DATA: u32 = 1 << 6;

struct Front {
    shared: Handle<Params>,
}

fn reference(id: u32) -> Option<u32> {
    (id != va::VA_INVALID_SURFACE).then_some(id)
}

impl Frontend for Front {
    fn stage(&mut self, picture: &VaPicture) -> Result<Vec<u8>, String> {
        let p: va::VAPictureParameterBufferVP8 = read(&picture.params, "VP8 picture")?;
        let [slice] = picture.slices.as_slice() else {
            return Err("VP8 requires one slice parameter element".into());
        };
        let s: va::VASliceParameterBufferVP8 = read(&slice.param, "VP8 slice")?;
        let f = unsafe { p.pic_fields.value };
        let key_frame = f & KEY_FRAME == 0;
        let params = Params {
            width: p.frame_width,
            height: p.frame_height,
            key_frame,
            version: ((f >> VERSION_SHIFT) & 7) as u8,
            // The client's first partition size excludes the bits it consumed.
            first_partition_size: s.partition_size[0] + s.macroblock_offset.div_ceil(8),
            update_segment_data: f & SEGMENTATION_ENABLED != 0 && f & UPDATE_SEGMENT_FEATURE_DATA != 0,
        };
        params.check().map_err(|e| e.to_string())?;
        let data = picture.data.get(slice.data.clone()).ok_or("VP8 slice data outside buffer")?;
        let refs = if key_frame {
            [None; 3]
        } else {
            [reference(p.last_ref_frame), reference(p.golden_ref_frame), reference(p.alt_ref_frame)]
        };
        self.shared.borrow_mut().pending = Some(Picture {
            target: picture.target,
            refs,
            field: Field::Frame,
            width: params.width,
            height: params.height,
            params,
        });
        Ok(data.to_vec())
    }
    fn release(&mut self, target: u32) {
        self.shared.borrow_mut().release(target);
    }
}

pub(super) fn create(width: u32, height: u32) -> Result<(Box<dyn Frontend>, Box<dyn CodecDecoder>), String> {
    let shared: Handle<Params> = Rc::new(RefCell::new(Shared::default()));
    let codec = LegacyCodec::new(Vp8::default(), shared.clone(), width, height);
    Ok((Box::new(Front { shared }), Box::new(codec)))
}
