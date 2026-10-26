//! VA picture parameters drive the RM session directly; display ordering belongs to libva's client.
use crate::va;
use nv_bionic_native::video::{
    self,
    h264::{codec::H264Decoder, desc, legacy, ps, MAX_SLOTS},
    session::{OutputAllocation, OutputAllocator, Session, SessionInfo, Target},
    Allocator, BuildContext, CodecDecoder, DecodeJob, Job, PictureInfo, SurfaceLayout, VResult,
    VideoError,
};
use std::{
    cell::RefCell,
    collections::HashMap,
    os::fd::{AsRawFd, OwnedFd},
    rc::Rc,
};

const BLOCK_LINEAR: u64 = 0x0300_0000_0060_6010;
fn invalid(s: impl Into<String>) -> VideoError {
    VideoError::Invalid(s.into())
}
fn valid(p: &va::VAPictureH264) -> bool {
    p.picture_id != u32::MAX && p.flags & va::VA_PICTURE_H264_INVALID == 0
}

pub struct Export {
    pub fds: [OwnedFd; 2],
    pub pitches: [u32; 2],
    pub offsets: [u32; 2],
    pub sizes: [u32; 2],
    pub modifier: u64,
}
impl Export {
    pub fn try_clone(&self) -> Result<Self, String> {
        Ok(Self {
            fds: [
                self.fds[0].try_clone().map_err(|e| e.to_string())?,
                self.fds[1].try_clone().map_err(|e| e.to_string())?,
            ],
            pitches: self.pitches,
            offsets: self.offsets,
            sizes: self.sizes,
            modifier: self.modifier,
        })
    }
}

struct OutputBacking {
    allocation: OutputAllocation,
    layout: SurfaceLayout,
    width: u32,
    height: u32,
}

/// Stable VA surface storage, including surfaces exported before their first decode.
/// This allocator is thread-confined like the RM device it owns.
pub struct OutputPool {
    allocator: OutputAllocator,
    surfaces: HashMap<u32, OutputBacking>,
}
impl OutputPool {
    pub fn new(pci: [u32; 4]) -> Result<Self, String> {
        let dev = video::VideoDevice::open_on(Some(pci)).map_err(|e| e.to_string())?;
        if !matches!(dev.info.nvdec_class, 0xc4b0 | 0xc7b0 | 0xc9b0) {
            return Err(
                "VA export requires verified Turing/Ampere/Ada block-linear NV12 layout".into(),
            );
        }
        Ok(Self {
            allocator: OutputAllocator::new(&dev).map_err(|e| e.to_string())?,
            surfaces: HashMap::new(),
        })
    }

    pub fn export(&mut self, id: u32, width: u32, height: u32) -> Result<Export, String> {
        if width == 0 || height == 0 || width > 4096 || height > 4096 {
            return Err("NV12 output dimensions must be within 1..4096".into());
        }
        if let Some(backing) = self.surfaces.get(&id) {
            if (backing.width, backing.height) != (width, height) {
                return Err("cannot change the dimensions of an existing surface backing".into());
            }
        } else {
            let layout = SurfaceLayout::new(width, height, 1);
            let allocation = self
                .allocator
                .allocate_zeroed(layout.size)
                .map_err(|e| e.to_string())?;
            self.surfaces.insert(
                id,
                OutputBacking {
                    allocation,
                    layout,
                    width,
                    height,
                },
            );
        }
        let backing = &self.surfaces[&id];
        let layout = backing.layout;
        Ok(Export {
            fds: [
                backing
                    .allocation
                    .fd
                    .try_clone()
                    .map_err(|e| e.to_string())?,
                backing
                    .allocation
                    .fd
                    .try_clone()
                    .map_err(|e| e.to_string())?,
            ],
            pitches: [layout.pitch; 2],
            offsets: [0, layout.chroma_offset as u32],
            sizes: [layout.size as u32; 2],
            modifier: BLOCK_LINEAR,
        })
    }

    pub fn release(&mut self, id: u32) {
        self.surfaces.remove(&id);
    }
}
struct Parameters {
    target: u32,
    pic: va::VAPictureParameterBufferH264,
    iq: va::VAIQMatrixBufferH264,
    slices: Vec<va::VASliceParameterBufferH264>,
    offsets: Vec<u32>,
}
#[derive(Default)]
struct State {
    pending: Option<Parameters>,
    // VA id -> (session surface, colocated-motion-vector slot).
    surfaces: HashMap<u32, (usize, u8)>,
    refs: Vec<usize>,
}
struct VaCodec {
    state: Rc<RefCell<State>>,
    machinery: H264Decoder,
    pending: Option<Parameters>,
    bits: Option<Vec<u8>>,
    offsets: Vec<u32>,
    built_slot: u8,
    previous_slot: u8,
    width: u32,
    height: u32,
}
impl CodecDecoder for VaCodec {
    fn codec(&self) -> u32 {
        video::CODEC_H264
    }
    fn bit_depth(&self) -> u32 {
        8
    }
    fn dpb_size(&self) -> usize {
        16
    }
    fn allocate(&mut self, a: &mut dyn Allocator, w: u32, h: u32) -> VResult<()> {
        self.machinery.allocate(a, w, h)
    }
    fn surfaces_needed(&self, _: &[u8]) -> VResult<usize> {
        Ok(1)
    }
    fn push_unit(&mut self, data: &[u8]) -> VResult<()> {
        self.pending = self.state.borrow_mut().pending.take();
        self.offsets = self
            .pending
            .as_mut()
            .map(|p| std::mem::take(&mut p.offsets))
            .unwrap_or_default();
        self.bits = Some(data.to_vec());
        Ok(())
    }
    fn next_job(&mut self) -> VResult<Option<Job>> {
        let Some(bitstream) = self.bits.take() else {
            return Ok(None);
        };
        let p = self
            .pending
            .as_ref()
            .ok_or_else(|| invalid("missing VA parameters"))?;
        Ok(Some(Job::Decode(DecodeJob {
            info: PictureInfo {
                width: (u32::from(p.pic.picture_width_in_mbs_minus1) + 1) * 16,
                height: (u32::from(p.pic.picture_height_in_mbs_minus1) + 1) * 16,
                crop: [0, 0, self.width, self.height],
                bit_depth: 8,
                ..Default::default()
            },
            show: true,
            display_key: None,
            bitstream,
            readback: None,
        })))
    }
    fn build(&mut self, job: &DecodeJob, ctx: &BuildContext, d: &mut [u8]) -> VResult<Vec<u32>> {
        let params = self
            .pending
            .as_ref()
            .ok_or_else(|| invalid("missing VA picture"))?;
        let p = &params.pic;
        // SAFETY: these unions are the libva ABI's packed bit-field words.
        let sf = unsafe { p.seq_fields.value };
        let pf = unsafe { p.pic_fields.value };
        let first = &params.slices[0];
        let mut scaling = ps::ScalingMatrices::flat();
        scaling.m4 = params.iq.ScalingList4x4;
        scaling.m8[..2].copy_from_slice(&params.iq.ScalingList8x8);
        let sps = ps::Sps {
            profile_idc: 100,
            constraint_flags: 0,
            level_idc: 0,
            id: 0,
            chroma_format_idc: sf & 3,
            separate_colour_plane: false,
            bit_depth_luma_minus8: 0,
            bit_depth_chroma_minus8: 0,
            qpprime_y_zero_transform_bypass: false,
            seq_scaling_matrix_present: true,
            scaling: scaling.clone(),
            log2_max_frame_num_minus4: (sf >> 8) & 15,
            pic_order_cnt_type: (sf >> 12) & 3,
            log2_max_poc_lsb_minus4: (sf >> 14) & 15,
            delta_pic_order_always_zero: sf & (1 << 18) != 0,
            offset_for_non_ref_pic: 0,
            offset_for_top_to_bottom_field: 0,
            offset_for_ref_frame: Vec::new(),
            max_num_ref_frames: u32::from(p.num_ref_frames),
            gaps_in_frame_num_allowed: sf & 8 != 0,
            pic_width_in_mbs: u32::from(p.picture_width_in_mbs_minus1) + 1,
            pic_height_in_map_units: u32::from(p.picture_height_in_mbs_minus1) + 1,
            frame_mbs_only: true,
            mb_adaptive_frame_field: false,
            direct_8x8_inference: sf & 64 != 0,
            crop: (0, 0, 0, 0),
            vui: ps::Vui::default(),
        };
        let pps = ps::Pps {
            id: 0,
            sps_id: 0,
            entropy_coding_mode: pf & 1 != 0,
            bottom_field_pic_order_in_frame_present: pf & (1 << 7) != 0,
            num_ref_idx_l0_default_active_minus1: u32::from(first.num_ref_idx_l0_active_minus1),
            num_ref_idx_l1_default_active_minus1: u32::from(first.num_ref_idx_l1_active_minus1),
            weighted_pred: pf & 2 != 0,
            weighted_bipred_idc: (pf >> 2) & 3,
            pic_init_qp_minus26: i32::from(p.pic_init_qp_minus26),
            chroma_qp_index_offset: i32::from(p.chroma_qp_index_offset),
            deblocking_filter_control_present: pf & (1 << 8) != 0,
            constrained_intra_pred: pf & (1 << 6) != 0,
            redundant_pic_cnt_present: pf & (1 << 9) != 0,
            transform_8x8_mode: pf & 16 != 0,
            second_chroma_qp_index_offset: i32::from(p.second_chroma_qp_index_offset),
            scaling,
        };
        let state = self.state.borrow();
        let mut dpb = [None; 16];
        let mut slots = [None; MAX_SLOTS];
        let mut used = [false; MAX_SLOTS];
        // The engine's POC type-0 parser uses the stock biased domain, including
        // references on both sides of an IDR or a POC-LSB wrap.
        let bias = if sps.pic_order_cnt_type == 0 {
            nv_bionic_native::video::h264::POC_TYPE0_BIAS
        } else {
            0
        };
        for (i, r) in p
            .ReferenceFrames
            .iter()
            .enumerate()
            .filter(|(_, r)| valid(r))
        {
            let non_existing = r.flags & va::VA_PICTURE_H264_NON_EXISTING != 0;
            let mapped = if non_existing {
                None
            } else {
                Some(*state.surfaces.get(&r.picture_id).ok_or_else(|| {
                    invalid(format!("missing reference surface {}", r.picture_id))
                })?)
            };
            if let Some((surface, slot)) = mapped {
                used[slot as usize] = true;
                slots[slot as usize] = Some(surface as u32);
            }
            dpb[i] = Some(desc::DescRef {
                slot: mapped.map(|(_, s)| s),
                state: 3,
                long_term: r.flags & va::VA_PICTURE_H264_LONG_TERM_REFERENCE != 0,
                non_existing,
                poc: [
                    r.TopFieldOrderCnt.saturating_add(bias),
                    r.BottomFieldOrderCnt.saturating_add(bias),
                ],
                frame_idx: r.frame_idx,
            });
        }
        let slot = used
            .iter()
            .position(|u| !u)
            .ok_or_else(|| invalid("no free H.264 colocated slot"))? as u8;
        self.built_slot = slot;
        slots[slot as usize] = Some(ctx.current as u32);
        let poc = [
            p.CurrPic.TopFieldOrderCnt.saturating_add(bias),
            p.CurrPic.BottomFieldOrderCnt.saturating_add(bias),
        ];
        let lists = desc::initial_lists(
            &dpb,
            u32::from(p.frame_num),
            poc[0].min(poc[1]),
            sps.max_frame_num(),
        );
        let ep = desc::EngineParams::for_class(ctx.decoder_class, Some(ctx.layout.pitch));
        // surface_id indexes the 17 hardware LUMA/CHROMA address pairs, not the
        // Session pool. The pool also contains client-held outputs and can exceed 17.
        let inp = desc::DescInput {
            sps: &sps,
            pps: &pps,
            dpb: &dpb,
            empty_slot: self.previous_slot,
            lists: [&lists[0], &lists[1], &lists[2]],
            cur_slot: slot,
            surface_id: u32::from(slot),
            bitstream_len: job.bitstream.len() as u32,
            frame_num: u32::from(p.frame_num),
            field_pic: false,
            bottom_field: false,
            second_field: false,
            ref_pic: pf & (1 << 10) != 0,
            intra_pic: params.slices.iter().all(|s| s.slice_type % 5 == 2),
            poc,
        };
        let (app, setup) = if ep.new_h264 {
            (desc::APP_ID_NEW_H264, desc::build(&inp, &ep))
        } else {
            let lay = legacy::LegacyLayout::stock_fit(
                ctx.layout.pitch,
                sps.pic_width_in_mbs,
                sps.frame_height_in_mbs(),
            );
            (
                legacy::APP_ID_H264,
                legacy::build(&inp, &ep, &lay, false, &self.offsets),
            )
        };
        let conceal = if ep.new_h264 {
            dpb.iter()
                .flatten()
                .find_map(|r| r.slot)
                .unwrap_or(self.previous_slot)
        } else {
            self.previous_slot
        };
        self.machinery
            .build_picture(ctx, d, &setup, &slots, app, desc::control_params(conceal))
    }
    fn finish(&mut self, _: &DecodeJob, current: usize, _: Option<&[u8]>) -> VResult<()> {
        let p = self
            .pending
            .take()
            .ok_or_else(|| invalid("missing finished VA picture"))?;
        self.state
            .borrow_mut()
            .surfaces
            .insert(p.target, (current, self.built_slot));
        // Stock legacy descriptors retain the last reference picture's colocated slot.
        if unsafe { p.pic.pic_fields.value } & (1 << 10) != 0 {
            self.previous_slot = self.built_slot;
        }
        Ok(())
    }
    fn referenced(&self) -> Vec<usize> {
        self.state.borrow().refs.clone()
    }
    fn status_offset(&self) -> Option<usize> {
        Some(desc::STATUS_OFFSET)
    }
    fn flush(&mut self) {
        self.pending = None;
        self.bits = None;
        let mut s = self.state.borrow_mut();
        s.pending = None;
        s.refs.clear();
        s.surfaces.clear();
    }
}

pub struct Decoder {
    session: Session,
    state: Rc<RefCell<State>>,
    pictures: HashMap<u32, u64>,
    /// Linear CPU-readable copy target, allocated on the first download and reused.
    staging: Option<OutputAllocation>,
    width: u32,
    height: u32,
}
impl Decoder {
    /// `surfaces` is the number of VA render targets bound to the context (0 when the client binds
    /// none up front). Each target retains at most one decoded picture and one more is being decoded.
    pub fn new(pci: [u32; 4], width: u32, height: u32, surfaces: usize) -> Result<Self, String> {
        let dev = video::VideoDevice::open_on(Some(pci)).map_err(|e| e.to_string())?;
        // This exact DRM modifier describes the verified pre-Blackwell NV12 layout.
        if !matches!(dev.info.nvdec_class, 0xc4b0 | 0xc7b0 | 0xc9b0) {
            return Err(
                "VA export requires verified Turing/Ampere/Ada block-linear NV12 layout".into(),
            );
        }
        let state = Rc::new(RefCell::new(State::default()));
        let codec = VaCodec {
            state: state.clone(),
            machinery: H264Decoder::new(dev.info.nvdec_class, width, height, &[])
                .map_err(|e| e.to_string())?,
            pending: None,
            bits: None,
            offsets: Vec::new(),
            built_slot: 0,
            previous_slot: 0,
            width,
            height,
        };
        let session = Session::with_codec(
            &dev,
            SessionInfo {
                codec: video::CODEC_H264,
                max_width: width,
                max_height: height,
                bit_depth: 8,
                // The session pool holds dpb_size() + 2 surfaces before the extra slots.
                extra_output_slots: if surfaces == 0 {
                    32
                } else {
                    u32::try_from((surfaces + 1).saturating_sub(16 + 2)).map_err(|_| "too many surfaces")?
                },
            },
            Box::new(codec),
        )
        .map_err(|e| e.to_string())?;
        Ok(Self {
            session,
            state,
            pictures: HashMap::new(),
            staging: None,
            width,
            height,
        })
    }
    pub fn decode(
        &mut self,
        target: u32,
        pic: &va::VAPictureParameterBufferH264,
        iq: &va::VAIQMatrixBufferH264,
        slices: &[va::VASliceParameterBufferH264],
        data: &[u8],
    ) -> Result<(), String> {
        let sf = unsafe { pic.seq_fields.value };
        let pf = unsafe { pic.pic_fields.value };
        if pic.bit_depth_luma_minus8 != 0 || pic.bit_depth_chroma_minus8 != 0 {
            return Err("10/12-bit H.264 is unsupported".into());
        }
        if sf & 16 == 0 || sf & 32 != 0 || pf & 32 != 0 || pic.CurrPic.flags & 6 != 0 {
            return Err("interlaced H.264 is unsupported".into());
        }
        if sf & 3 != 1 || sf & 4 != 0 || pic.num_slice_groups_minus1 != 0 || pic.num_ref_frames > 16
        {
            return Err("only non-FMO 8-bit 4:2:0 H.264 is supported".into());
        }
        if slices.is_empty() || slices.len() > 256 {
            return Err("H.264 requires 1..256 complete slices".into());
        }
        let coded_w = (u32::from(pic.picture_width_in_mbs_minus1) + 1) * 16;
        let coded_h = (u32::from(pic.picture_height_in_mbs_minus1) + 1) * 16;
        if coded_w != self.width.next_multiple_of(16) || coded_h != self.height.next_multiple_of(16)
        {
            return Err("H.264 coded dimensions do not match context".into());
        }
        let mut bits = Vec::with_capacity(data.len() + slices.len() * 3);
        let mut offsets = Vec::with_capacity(slices.len());
        for s in slices {
            if s.slice_data_flag != 0 || !matches!(s.slice_type % 5, 0 | 1 | 2) {
                return Err("fragmented or SP/SI H.264 slices are unsupported".into());
            }
            let start = s.slice_data_offset as usize;
            let end = start
                .checked_add(s.slice_data_size as usize)
                .ok_or("slice range overflow")?;
            let nal = data.get(start..end).ok_or("slice outside data buffer")?;
            let prefix = if nal.starts_with(&[0, 0, 0, 1]) {
                4
            } else if nal.starts_with(&[0, 0, 1]) {
                3
            } else {
                0
            };
            let nal = &nal[prefix..];
            if nal.is_empty() || !matches!(nal[0] & 31, 1 | 5) {
                return Err("slice has no H.264 VCL NAL header".into());
            }
            offsets.push(bits.len() as u32);
            bits.extend_from_slice(&[0, 0, 1]);
            bits.extend_from_slice(nal);
        }
        // VA supplies authoritative DPB state before each picture. Keep only those GPU
        // references, but retain every live VA output separately through Session picture ids.
        {
            let mut state = self.state.borrow_mut();
            let mut refs = Vec::new();
            for r in pic
                .ReferenceFrames
                .iter()
                .filter(|r| valid(r) && r.flags & va::VA_PICTURE_H264_NON_EXISTING == 0)
            {
                if r.picture_id == target {
                    return Err("decode target is also a reference".into());
                }
                let &(surface, _) = state
                    .surfaces
                    .get(&r.picture_id)
                    .ok_or_else(|| format!("unknown reference surface {}", r.picture_id))?;
                refs.push(surface);
            }
            state.refs = refs;
            state.pending = Some(Parameters {
                target,
                pic: *pic,
                iq: *iq,
                slices: slices.to_vec(),
                offsets,
            });
        }
        if let Some(id) = self.pictures.remove(&target) {
            self.session.release(id).map_err(|e| e.to_string())?;
        }
        self.session
            .decode(&bits, u64::from(target), 0)
            .map_err(|e| e.to_string())?;
        let (id, _, _) = self
            .session
            .next_picture()
            .ok_or("RM session produced no picture")?;
        self.pictures.insert(target, id);
        // EndPicture completes synchronously; VA surface-ready/sync never precedes GPU completion.
        self.session.wait_picture(id).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Copy decoded pixels into the surface's stable externally shared storage.
    /// The copy engine preserves block-linear layout; no pixels cross the CPU.
    pub fn copy_into(&mut self, target: u32, output: &Export) -> Result<(), String> {
        let id = *self
            .pictures
            .get(&target)
            .ok_or("surface has not been decoded")?;
        let target = Target {
            dmabuf_fd: [output.fds[0].as_raw_fd(), output.fds[1].as_raw_fd(), -1],
            offset: [
                u64::from(output.offsets[0]),
                u64::from(output.offsets[1]),
                0,
            ],
            pitch: [output.pitches[0], output.pitches[1], 0],
            modifier: output.modifier,
            format: video::FORMAT_NV12,
            width: self.width,
            height: self.height,
            acquire_fence_fd: -1,
        };
        self.session
            .copy_out_wait(id, &target)
            .map_err(|e| e.to_string())
    }
    pub fn readback(&mut self, target: u32) -> Result<Vec<u8>, String> {
        let id = *self
            .pictures
            .get(&target)
            .ok_or("surface has not been decoded")?;
        let pitch = self.width.next_multiple_of(2);
        let y = pitch as usize * self.height as usize;
        let size = y + pitch as usize * self.height.div_ceil(2) as usize;
        if self.staging.is_none() {
            let alloc = self
                .session
                .allocate_output(size.next_multiple_of(65536))
                .map_err(|e| e.to_string())?;
            self.staging = Some(alloc);
        }
        let alloc = self.staging.as_ref().unwrap();
        let t = Target {
            dmabuf_fd: [alloc.fd.as_raw_fd(), alloc.fd.as_raw_fd(), -1],
            offset: [0, y as u64, 0],
            pitch: [pitch, pitch, 0],
            modifier: 0,
            format: video::FORMAT_NV12,
            width: self.width,
            height: self.height,
            acquire_fence_fd: -1,
        };
        self.session
            .copy_out_wait(id, &t)
            .map_err(|e| e.to_string())?;
        let map = alloc.map().map_err(|e| e.to_string())?;
        let mut packed = vec![
            0;
            self.width as usize * self.height as usize
                + pitch as usize * self.height.div_ceil(2) as usize
        ];
        for row in 0..self.height as usize {
            map.read_bytes(
                row * pitch as usize,
                &mut packed[row * self.width as usize..(row + 1) * self.width as usize],
            )
            .map_err(|e| e.to_string())?;
        }
        map.read_bytes(y, &mut packed[self.width as usize * self.height as usize..])
            .map_err(|e| e.to_string())?;
        Ok(packed)
    }
    pub fn release_surface(&mut self, target: u32) {
        if let Some(id) = self.pictures.remove(&target) {
            let _ = self.session.release(id);
        }
        self.state.borrow_mut().surfaces.remove(&target);
    }
}
