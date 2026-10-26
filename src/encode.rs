//! VA-API encoding parameters and a thread-confined native RM session.
#![allow(non_upper_case_globals)] // Match libva's public ABI constant spelling.
use crate::{
    array, count, dimensions, external_buffer_attr, gpu, image_format, import::{Dma, Kind}, input_image, new_input, out, surface_attr, va::*,
    Completion, Driver, ENC_MAX_DIM, FAILED, INVALID, MAX_DIM, NV12, P010, Q416, R, YUV444P,
};
use nvrm::video::enc::{EncConfig, Encoder, ENC_AV1, ENC_H264, ENC_HEVC};
use std::{mem::size_of, sync::{mpsc, Arc}, thread};

pub const PROFILES: &[VAProfile] = &[
    VAProfileH264ConstrainedBaseline,
    VAProfileH264Main,
    VAProfileH264High,
    VAProfileHEVCMain,
    VAProfileHEVCMain10,
    VAProfileHEVCMain444,
    VAProfileHEVCMain444_10,
    VAProfileAV1Profile0,
];
pub fn rt(p: VAProfile) -> u32 {
    match p {
        VAProfileH264High => VA_RT_FORMAT_YUV420 | VA_RT_FORMAT_YUV444,
        VAProfileHEVCMain10 => VA_RT_FORMAT_YUV420_10,
        VAProfileHEVCMain444 => VA_RT_FORMAT_YUV444,
        VAProfileHEVCMain444_10 => VA_RT_FORMAT_YUV444_10,
        VAProfileAV1Profile0 => VA_RT_FORMAT_YUV420 | VA_RT_FORMAT_YUV420_10,
        _ => VA_RT_FORMAT_YUV420,
    }
}
pub fn attribute(p: VAProfile, t: VAConfigAttribType) -> u32 {
    match t {
        VAConfigAttribRTFormat => rt(p),
        VAConfigAttribRateControl => VA_RC_CQP | VA_RC_CBR | VA_RC_VBR,
        VAConfigAttribEncPackedHeaders => {
            if p == VAProfileAV1Profile0 {
                VA_ENC_PACKED_HEADER_SEQUENCE
            } else {
                VA_ENC_PACKED_HEADER_SEQUENCE
                    | VA_ENC_PACKED_HEADER_PICTURE
                    | VA_ENC_PACKED_HEADER_SLICE
                    | VA_ENC_PACKED_HEADER_RAW_DATA
            }
        }
        VAConfigAttribEncMaxRefFrames
        | VAConfigAttribEncMaxSlices
        | VAConfigAttribPredictionDirection => 1,
        VAConfigAttribMaxPictureWidth | VAConfigAttribMaxPictureHeight => ENC_MAX_DIM,
        VAConfigAttribEncAV1 if p == VAProfileAV1Profile0 => 0,
        VAConfigAttribEncAV1Ext1 if p == VAProfileAV1Profile0 => {
            let mut v = VAConfigAttribValEncAV1Ext1::default();
            unsafe {
                v.bits.set_min_segid_block_size_accepted(32);
                v.value
            }
        }
        VAConfigAttribEncAV1Ext2 if p == VAProfileAV1Profile0 => {
            let mut v = VAConfigAttribValEncAV1Ext2::default();
            unsafe {
                v.bits.set_obu_size_bytes_minus1(3);
                v.bits.set_tx_mode_support(4);
                v.bits.set_max_tile_num_minus1(63);
                v.value
            }
        }
        VAConfigAttribEncHEVCFeatures
            if matches!(
                p,
                VAProfileHEVCMain
                    | VAProfileHEVCMain10
                    | VAProfileHEVCMain444
                    | VAProfileHEVCMain444_10
            ) =>
        {
            let mut v = VAConfigAttribValEncHEVCFeatures::default();
            unsafe {
                v.bits.set_sao(1);
                v.bits.set_temporal_mvp(1);
                v.bits.set_strong_intra_smoothing(1);
                v.bits.set_dependent_slices(1);
                v.bits.set_sign_data_hiding(1);
                v.bits.set_constrained_intra_pred(1);
                v.bits.set_weighted_prediction(1);
                v.bits.set_deblocking_filter_disable(1);
                v.value
            }
        }
        VAConfigAttribEncHEVCBlockSizes
            if matches!(
                p,
                VAProfileHEVCMain
                    | VAProfileHEVCMain10
                    | VAProfileHEVCMain444
                    | VAProfileHEVCMain444_10
            ) =>
        {
            let mut v = VAConfigAttribValEncHEVCBlockSizes::default();
            unsafe {
                v.bits.set_log2_max_coding_tree_block_size_minus3(2);
                v.bits.set_log2_min_coding_tree_block_size_minus3(2);
                v.bits.set_log2_min_luma_coding_block_size_minus3(1);
                v.bits.set_log2_max_luma_transform_block_size_minus2(3);
                v.bits.set_log2_max_pcm_coding_block_size_minus3(2);
                v.bits.set_log2_min_pcm_coding_block_size_minus3(1);
                v.value
            }
        }
        _ => VA_ATTRIB_NOT_SUPPORTED,
    }
}
fn record<T>(b: &[u8]) -> R<T> {
    if b.len() < size_of::<T>() {
        Err(INVALID)
    } else {
        Ok(unsafe { b.as_ptr().cast::<T>().read_unaligned() })
    }
}
#[derive(Clone)]
pub struct Input {
    pub bytes: Vec<u8>,
    pub fourcc: u32,
    pub pitch: usize,
    pub offsets: [usize; 3],
}
/// What the encoder reads for one picture: CPU-uploaded planes, or a dma-buf (a decode surface's
/// exported backing or a client's imported buffer) that the copy engine ingests, once the work that
/// produces it completed.
pub enum Frame {
    Cpu(Input),
    Dma { key: u32, dma: Arc<Dma>, ready: Vec<Arc<Completion>> },
}
enum Message {
    Encode(Request),
    /// A surface was destroyed: drop its cached import.
    Forget(u32),
}
struct Request {
    cfg: EncConfig,
    force: bool,
    frame: Frame,
    output: Vec<u8>,
    reply: mpsc::Sender<(Option<Input>, Vec<u8>, R)>,
}
pub struct Session {
    tx: mpsc::Sender<Message>,
    join: Option<thread::JoinHandle<()>>,
    pub cfg: EncConfig,
    pub target: Option<u32>,
    pub coded: Option<u32>,
    force: bool,
    pic_qp: i32,
    misc_qp: bool,
}
impl Session {
    pub fn new(p: VAProfile, rc: u32, pci: [u32; 4], w: u32, h: u32) -> R<Self> {
        let cfg = EncConfig {
            codec: match p {
                VAProfileAV1Profile0 => ENC_AV1,
                VAProfileH264ConstrainedBaseline | VAProfileH264Main | VAProfileH264High => {
                    ENC_H264
                }
                _ => ENC_HEVC,
            },
            width: w as i32,
            height: h as i32,
            qp_i: 26,
            qp_p: 26,
            num_refs: 1,
            fps: 30,
            idr_period: 120,
            bitrate: 4_000_000,
            rc_mode: if rc & VA_RC_CBR != 0 {
                2
            } else if rc & VA_RC_VBR != 0 {
                1
            } else {
                0
            },
            bit_depth: if matches!(p, VAProfileHEVCMain10 | VAProfileHEVCMain444_10) {
                10
            } else {
                8
            },
            chroma444: matches!(p, VAProfileHEVCMain444 | VAProfileHEVCMain444_10),
            h264_profile: match p {
                VAProfileH264ConstrainedBaseline => 66,
                VAProfileH264Main => 77,
                _ => 100,
            },
            cabac: p != VAProfileH264ConstrainedBaseline,
            ..Default::default()
        };
        let (tx, rx) = mpsc::channel::<Message>();
        let join = thread::Builder::new()
            .name("rustnv-nvenc".into())
            .spawn(move || {
                let mut enc: Option<Encoder> = None;
                let mut first = true;
                let mut since_idr = 0u32;
                let mut chroma = Vec::new();
                while let Ok(message) = rx.recv() {
                    let mut r = match message {
                        Message::Encode(r) => r,
                        Message::Forget(key) => {
                            if let Some(e) = enc.as_mut() {
                                e.forget_dmabuf(u64::from(key));
                            }
                            continue;
                        }
                    };
                    let result = (|| {
                        if enc.is_none() {
                            enc = Some(gpu(
                                Encoder::open_on(&r.cfg, Some(pci)).map_err(|e| e.to_string())
                            )?);
                        }
                        let e = enc.as_mut().unwrap();
                        e.cfg.qp_i = r.cfg.qp_i;
                        e.cfg.qp_p = r.cfg.qp_p;
                        e.cfg.min_qp = r.cfg.min_qp;
                        e.cfg.max_qp = r.cfg.max_qp;
                        match &r.frame {
                            Frame::Cpu(input) => upload(e, input, &mut chroma)?,
                            Frame::Dma { key, dma, ready } => ingest(e, *key, dma, ready)?,
                        }
                        r.output.clear();
                        let t = gpu(e.submit(r.force).map_err(|e| e.to_string()))?;
                        // NVENC generates H.264/HEVC parameter sets; application copies are ignored.
                        if first
                            || r.force
                            || (e.cfg.idr_period > 0 && since_idr >= e.cfg.idr_period as u32)
                        {
                            e.headers(&mut r.output);
                        }
                        let frame = gpu(e.wait(t, &mut r.output).map_err(|e| e.to_string()))?;
                        first = false;
                        since_idr = if frame.idr { 1 } else { since_idr + 1 };
                        if frame.ucode_error != 0 {
                            return Err(FAILED);
                        }
                        Ok(())
                    })();
                    let input = match r.frame {
                        Frame::Cpu(input) => Some(input),
                        Frame::Dma { .. } => None,
                    };
                    let _ = r.reply.send((input, r.output, result));
                }
            })
            .map_err(|_| FAILED)?;
        Ok(Self {
            tx,
            join: Some(join),
            cfg,
            target: None,
            coded: None,
            force: false,
            pic_qp: 26,
            misc_qp: false,
        })
    }
    pub fn begin(&mut self, target: u32) -> R {
        if self.target.is_some() {
            return Err(INVALID);
        }
        self.target = Some(target);
        self.coded = None;
        self.force = false;
        Ok(())
    }
    fn qp(&mut self, q: i32) {
        if q > 0 && !self.misc_qp {
            let q = q.clamp(1, if self.cfg.codec == ENC_AV1 { 255 } else { 51 });
            self.cfg.qp_i = q;
            self.cfg.qp_p = q;
        }
    }
    pub fn render(&mut self, kind: VABufferType, b: &[u8]) -> R {
        match kind {
            VAEncSequenceParameterBufferType => match self.cfg.codec {
                ENC_H264 => {
                    let s: VAEncSequenceParameterBufferH264 = record(b)?;
                    let crop = if s.frame_cropping_flag == 0 {
                        0
                    } else if unsafe { s.seq_fields.bits.chroma_format_idc() } == 3 {
                        1
                    } else {
                        2
                    };
                    self.cfg.width = (s.picture_width_in_mbs as u32 * 16
                        - crop * (s.frame_crop_left_offset + s.frame_crop_right_offset))
                        as i32;
                    self.cfg.height = (s.picture_height_in_mbs as u32 * 16
                        - crop * (s.frame_crop_top_offset + s.frame_crop_bottom_offset))
                        as i32;
                    self.cfg.idr_period = if s.intra_idr_period > 0 {
                        s.intra_idr_period
                    } else {
                        s.intra_period
                    } as i32;
                    if s.bits_per_second > 0 {
                        self.cfg.bitrate = s.bits_per_second as i32;
                    }
                    self.cfg.h264_level = s.level_idc as i32;
                    if s.time_scale > 0
                        && unsafe { s.vui_fields.bits.timing_info_present_flag() } != 0
                    {
                        let den = s.num_units_in_tick.max(1) * 2;
                        self.cfg.fps = ((s.time_scale + den / 2) / den).max(1) as i32;
                    }
                }
                ENC_HEVC => {
                    let s: VAEncSequenceParameterBufferHEVC = record(b)?;
                    self.cfg.width = s.pic_width_in_luma_samples as i32;
                    self.cfg.height = s.pic_height_in_luma_samples as i32;
                    self.cfg.idr_period = if s.intra_idr_period > 0 {
                        s.intra_idr_period
                    } else {
                        s.intra_period
                    } as i32;
                    if s.bits_per_second > 0 {
                        self.cfg.bitrate = s.bits_per_second as i32;
                    }
                    if s.vui_time_scale > 0
                        && unsafe { s.vui_fields.bits.vui_timing_info_present_flag() } != 0
                    {
                        let den = s.vui_num_units_in_tick.max(1);
                        self.cfg.fps = ((s.vui_time_scale + den / 2) / den).max(1) as i32;
                    }
                }
                _ => {
                    let s: VAEncSequenceParameterBufferAV1 = record(b)?;
                    self.cfg.idr_period = s.intra_period as i32;
                    if s.bits_per_second > 0 {
                        self.cfg.bitrate = s.bits_per_second as i32;
                    }
                }
            },
            VAEncPictureParameterBufferType => match self.cfg.codec {
                ENC_H264 => {
                    let p: VAEncPictureParameterBufferH264 = record(b)?;
                    self.coded = Some(p.coded_buf);
                    self.force = unsafe { p.pic_fields.bits.idr_pic_flag() != 0 };
                    self.pic_qp = p.pic_init_qp as i32;
                    if self.cfg.rc_mode == 0 {
                        self.qp(self.pic_qp)
                    }
                }
                ENC_HEVC => {
                    let p: VAEncPictureParameterBufferHEVC = record(b)?;
                    self.coded = Some(p.coded_buf);
                    self.force = unsafe { p.pic_fields.bits.idr_pic_flag() != 0 };
                    self.pic_qp = p.pic_init_qp as i32;
                    if self.cfg.rc_mode == 0 {
                        self.qp(self.pic_qp)
                    }
                }
                _ => {
                    let p: VAEncPictureParameterBufferAV1 = record(b)?;
                    self.cfg.width = p.frame_width_minus_1 as i32 + 1;
                    self.cfg.height = p.frame_height_minus_1 as i32 + 1;
                    self.coded = Some(p.coded_buf);
                    self.force = unsafe { p.picture_flags.bits.frame_type() == 0 };
                    if self.cfg.rc_mode == 0 {
                        self.qp(p.base_qindex as i32)
                    }
                }
            },
            VAEncSliceParameterBufferType if self.cfg.rc_mode == 0 => match self.cfg.codec {
                ENC_H264 => {
                    let s: VAEncSliceParameterBufferH264 = record(b)?;
                    self.qp(self.pic_qp + s.slice_qp_delta as i32)
                }
                ENC_HEVC => {
                    let s: VAEncSliceParameterBufferHEVC = record(b)?;
                    self.qp(self.pic_qp + s.slice_qp_delta as i32)
                }
                _ => {}
            },
            VAEncMiscParameterBufferType => {
                let m: VAEncMiscParameterBuffer = record(b)?;
                let b = &b[size_of::<VAEncMiscParameterBuffer>()..];
                match m.type_ {
                    VAEncMiscParameterTypeRateControl => {
                        let r: VAEncMiscParameterRateControl = record(b)?;
                        if r.bits_per_second > 0 {
                            self.cfg.max_bitrate = r.bits_per_second as i32;
                            self.cfg.bitrate = (r.bits_per_second as u64
                                * if r.target_percentage == 0 {
                                    100
                                } else {
                                    r.target_percentage.min(100)
                                } as u64
                                / 100) as i32;
                        }
                        let limit = if self.cfg.codec == ENC_AV1 { 255 } else { 51 };
                        if r.min_qp > 0 {
                            self.cfg.min_qp = Some(r.min_qp.min(limit) as u8);
                        }
                        if r.max_qp > 0 {
                            self.cfg.max_qp =
                                Some(r.max_qp.min(limit).max(r.min_qp.min(limit)) as u8);
                        }
                        if r.initial_qp > 0 {
                            self.cfg.qp_i = r.initial_qp as i32;
                            self.cfg.qp_p = r.initial_qp as i32;
                            self.misc_qp = true;
                        }
                    }
                    VAEncMiscParameterTypeFrameRate => {
                        let f: VAEncMiscParameterFrameRate = record(b)?;
                        let n = f.framerate & 65535;
                        let d = (f.framerate >> 16).max(1);
                        if n > 0 {
                            self.cfg.fps = ((n + d / 2) / d).max(1) as i32;
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        Ok(())
    }
    pub fn encode(&mut self, frame: Frame, output: Vec<u8>) -> R<(Option<Input>, Vec<u8>)> {
        self.target.take().ok_or(INVALID)?;
        let fourcc = match &frame {
            Frame::Cpu(input) => input.fourcc,
            Frame::Dma { dma, .. } => dma.fourcc,
        };
        self.cfg.bit_depth = if matches!(fourcc, P010 | Q416) || Kind::of(fourcc) == Some(Kind::P01x) {
            10
        } else {
            8
        };
        self.cfg.chroma444 = matches!(fourcc, YUV444P | Q416);
        let lo = self.cfg.min_qp.unwrap_or(0) as i32;
        let hi = (self
            .cfg
            .max_qp
            .unwrap_or(if self.cfg.codec == ENC_AV1 { 255 } else { 51 }) as i32)
            .max(lo);
        self.cfg.qp_i = self.cfg.qp_i.clamp(lo, hi);
        self.cfg.qp_p = self.cfg.qp_p.clamp(lo, hi);
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Message::Encode(Request {
                cfg: self.cfg.clone(),
                force: self.force,
                frame,
                output,
                reply: tx,
            }))
            .map_err(|_| FAILED)?;
        let (i, o, r) = rx.recv().map_err(|_| FAILED)?;
        r?;
        Ok((i, o))
    }
    /// A surface was destroyed: its cached GPU import is dropped by the encoder thread.
    pub fn forget(&self, surface: u32) {
        let _ = self.tx.send(Message::Forget(surface));
    }
}
/// CPU-side upload of planes the client wrote through `vaPutImage` or a derived image.
fn upload(e: &mut Encoder, i: &Input, chroma: &mut Vec<u8>) -> R {
    let y = &i.bytes[i.offsets[0]..];
    let uv = &i.bytes[i.offsets[1]..];
    gpu(match i.fourcc {
        P010 => e.load_p010(y, i.pitch, uv, i.pitch),
        YUV444P => e.load_yuv444(y, i.pitch, uv, i.pitch, &i.bytes[i.offsets[2]..], i.pitch),
        Q416 => {
            let w = e.cfg.width as usize;
            let h = e.cfg.height as usize;
            chroma.resize(w * h * 4, 0);
            let v = &i.bytes[i.offsets[2]..];
            for row in 0..h {
                for x in 0..w {
                    let s = row * i.pitch + 2 * x;
                    let d = row * w * 4 + 4 * x;
                    chroma[d..d + 2].copy_from_slice(&uv[s..s + 2]);
                    chroma[d + 2..d + 4].copy_from_slice(&v[s..s + 2]);
                }
            }
            e.load_x444(y, i.pitch, chroma, w * 4)
        }
        _ => e.load_nv12(y, i.pitch, uv, i.pitch),
    }
    .map_err(|e| e.to_string()))
}
/// GPU-side ingest of a dma-buf once the decode and copy that produce it completed.
fn ingest(e: &mut Encoder, key: u32, dma: &Dma, ready: &[Arc<Completion>]) -> R {
    for work in ready {
        work.wait()?;
    }
    let source = dma.source()?;
    gpu(match dma.kind {
        Kind::Bgra => e.load_dmabuf_bgra(u64::from(key), &source),
        _ => e.load_dmabuf(u64::from(key), &source),
    }
    .map_err(|e| e.to_string()))
}
impl Drop for Session {
    fn drop(&mut self) {
        let (dummy, _) = mpsc::channel();
        drop(std::mem::replace(&mut self.tx, dummy));
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

pub(super) const INPUT_FORMATS: [u32; 4] = [NV12, P010, YUV444P, Q416];

pub(super) unsafe fn query_profiles(list: *mut VAProfile, n: *mut i32) -> R {
    if list.is_null() {
        return Err(INVALID);
    }
    let mut profiles = crate::PROFILES.to_vec();
    for profile in PROFILES {
        if !profiles.contains(profile) {
            profiles.push(*profile);
        }
    }
    std::ptr::copy_nonoverlapping(profiles.as_ptr(), list, profiles.len());
    out(n, profiles.len() as i32)
}

pub(super) unsafe fn query_entrypoints(
    profile: VAProfile,
    list: *mut VAEntrypoint,
    n: *mut i32,
) -> R {
    let mut entries = Vec::new();
    if crate::PROFILES.contains(&profile) {
        entries.push(VAEntrypointVLD);
    }
    if PROFILES.contains(&profile) {
        entries.push(VAEntrypointEncSlice);
    }
    if entries.is_empty() {
        return Err(VA_STATUS_ERROR_UNSUPPORTED_PROFILE as VAStatus);
    }
    if list.is_null() {
        return Err(INVALID);
    }
    std::ptr::copy_nonoverlapping(entries.as_ptr(), list, entries.len());
    out(n, entries.len() as i32)
}

pub(super) unsafe fn get_attributes(profile: VAProfile, attrs: *mut VAConfigAttrib, n: i32) -> R {
    if !PROFILES.contains(&profile) {
        return Err(VA_STATUS_ERROR_UNSUPPORTED_PROFILE as VAStatus);
    }
    for i in 0..count(n)? {
        let attr = array(attrs, count(n)?)?[i];
        (*attrs.add(i)).value = attribute(profile, attr.type_);
    }
    Ok(())
}

pub(super) unsafe fn create_config(
    d: &mut Driver,
    profile: VAProfile,
    attrs: *mut VAConfigAttrib,
    n: i32,
    id: *mut u32,
) -> R {
    if !PROFILES.contains(&profile) {
        return Err(VA_STATUS_ERROR_UNSUPPORTED_PROFILE as VAStatus);
    }
    let mut rc = VA_RC_CQP;
    for attr in array(attrs, count(n)?)? {
        let supported = attribute(profile, attr.type_);
        if supported == VA_ATTRIB_NOT_SUPPORTED || attr.value & !supported != 0 {
            return Err(VA_STATUS_ERROR_ATTR_NOT_SUPPORTED as VAStatus);
        }
        if attr.type_ == VAConfigAttribRateControl {
            rc = attr.value;
        }
    }
    let key = d.id()?;
    d.enc_configs.insert(key, (profile, rc));
    out(id, key)
}

pub(super) unsafe fn query_config(
    profile: VAProfile,
    p: *mut VAProfile,
    e: *mut VAEntrypoint,
    attrs: *mut VAConfigAttrib,
    n: *mut i32,
) -> R {
    out(p, profile)?;
    out(e, VAEntrypointEncSlice)?;
    out(
        attrs,
        VAConfigAttrib {
            type_: VAConfigAttribRTFormat,
            value: rt(profile),
        },
    )?;
    out(n, 1)
}

pub(super) unsafe fn create_context(
    d: &mut Driver,
    config: (VAProfile, u32),
    w: i32,
    h: i32,
    targets: *mut u32,
    n: i32,
    id: *mut u32,
) -> R {
    if id.is_null() || w <= 0 || h <= 0 {
        return Err(INVALID);
    }
    dimensions(w as u32, h as u32, ENC_MAX_DIM)?;
    for target in array(targets, count(n)?)? {
        d.surface(*target)?;
    }
    let key = d.id()?;
    let (profile, rc) = config;
    d.enc_contexts
        .insert(key, Session::new(profile, rc, d.pci, w as u32, h as u32)?);
    out(id, key)
}

pub(super) unsafe fn render_picture(d: &mut Driver, id: u32, ids: *mut u32, n: i32) -> R {
    for key in array(ids, count(n)?)? {
        let buffer = d.buffer(*key)?;
        if buffer.context != Some(id) || buffer.mapped {
            return Err(INVALID);
        }
        let buffer = &d.buffers[key];
        let bytes = &buffer.bytes[..buffer.size as usize * buffer.count as usize];
        d.enc_contexts
            .get_mut(&id)
            .unwrap()
            .render(buffer.kind, bytes)?;
    }
    Ok(())
}

pub(super) unsafe fn map_buffer(d: &mut Driver, id: u32, p: *mut *mut std::ffi::c_void) -> R {
    if let Some(surface) = d.buffer(id)?.derived {
        let buffer = d.buffers.get_mut(&id).unwrap();
        if buffer.mapped {
            return Err(INVALID);
        }
        let input = d
            .surfaces
            .get_mut(&surface)
            .ok_or(INVALID)?
            .input
            .as_mut()
            .ok_or(INVALID)?;
        std::mem::swap(&mut input.bytes, &mut buffer.bytes);
    }
    let buffer = d
        .buffers
        .get_mut(&id)
        .ok_or(VA_STATUS_ERROR_INVALID_BUFFER as VAStatus)?;
    if buffer.mapped {
        return Err(INVALID);
    }
    let mapped = if let Some(segment) = buffer.coded.as_mut() {
        segment.buf = buffer.bytes.as_mut_ptr().cast();
        segment.size = buffer.bytes.len() as u32;
        segment.status = VA_CODED_BUF_STATUS_SINGLE_NALU;
        segment.as_mut() as *mut VACodedBufferSegment as *mut std::ffi::c_void
    } else {
        buffer.bytes.as_mut_ptr().cast()
    };
    out(p, mapped)?;
    buffer.mapped = true;
    Ok(())
}

pub(super) fn unmap_buffer(d: &mut Driver, id: u32) -> R {
    let buffer = d
        .buffers
        .get_mut(&id)
        .ok_or(VA_STATUS_ERROR_INVALID_BUFFER as VAStatus)?;
    if !buffer.mapped {
        return Err(INVALID);
    }
    buffer.mapped = false;
    if let Some(surface) = buffer.derived {
        let input = d
            .surfaces
            .get_mut(&surface)
            .ok_or(INVALID)?
            .input
            .as_mut()
            .ok_or(INVALID)?;
        std::mem::swap(&mut input.bytes, &mut buffer.bytes);
    }
    Ok(())
}

pub(super) unsafe fn query_images(formats: *mut VAImageFormat, n: *mut i32) -> R {
    if formats.is_null() {
        return Err(INVALID);
    }
    for (i, fourcc) in INPUT_FORMATS.iter().enumerate() {
        let mut format = image_format();
        format.fourcc = *fourcc;
        format.bits_per_pixel = match *fourcc {
            P010 | YUV444P => 24,
            Q416 => 48,
            _ => 12,
        };
        formats.add(i).write(format);
    }
    out(n, INPUT_FORMATS.len() as i32)
}

pub(super) unsafe fn create_image(
    d: &mut Driver,
    format: *mut VAImageFormat,
    w: i32,
    h: i32,
    image: *mut VAImage,
) -> R {
    if format.is_null() || image.is_null() || w <= 0 || h <= 0 {
        return Err(INVALID);
    }
    if !INPUT_FORMATS.contains(&(*format).fourcc) {
        return Err(VA_STATUS_ERROR_INVALID_IMAGE_FORMAT as VAStatus);
    }
    let input = new_input((*format).fourcc, w as u32, h as u32);
    let value = input_image(d, &input, w as u32, h as u32)?;
    d.buffers.get_mut(&value.buf).unwrap().bytes = input.bytes;
    out(image, value)
}

pub(super) fn derive_input_image(d: &mut Driver, surface: u32) -> R<Option<VAImage>> {
    let s = d.surface(surface)?;
    if s.import.is_some() {
        return Err(VA_STATUS_ERROR_INVALID_SURFACE as VAStatus);
    }
    if s.decode.is_some() {
        return Ok(None);
    }
    let (w, h, fourcc) = (s.width, s.height, s.fourcc);
    let input = d
        .surfaces
        .get_mut(&surface)
        .unwrap()
        .input
        .take()
        .unwrap_or_else(|| new_input(fourcc, w, h));
    let image = input_image(d, &input, w, h)?;
    d.buffers.get_mut(&image.buf).unwrap().derived = Some(surface);
    d.surfaces.get_mut(&surface).unwrap().input = Some(input);
    Ok(Some(image))
}

pub(super) fn put_image(
    d: &mut Driver,
    surface: u32,
    image: u32,
    src: (i32, i32, u32, u32),
    dst: (i32, i32, u32, u32),
) -> R {
    let image = *d
        .images
        .get(&image)
        .ok_or(VA_STATUS_ERROR_INVALID_IMAGE as VAStatus)?;
    let buffer = d.buffer(image.buf)?;
    let s = d.surface(surface)?;
    if s.import.is_some() {
        return Err(VA_STATUS_ERROR_INVALID_SURFACE as VAStatus);
    }
    let (sx, sy, sw, sh) = src;
    let (dx, dy, dw, dh) = dst;
    if sx != 0
        || sy != 0
        || dx != 0
        || dy != 0
        || sw != dw
        || sh != dh
        || dw != s.width
        || dh != s.height
        || sw != image.width as u32
        || sh != image.height as u32
        || buffer.mapped
    {
        return Err(INVALID);
    }
    let bytes = &d.buffers[&image.buf].bytes;
    let s = d.surfaces.get_mut(&surface).unwrap();
    let input = s.input.get_or_insert_with(|| Input {
        bytes: Vec::new(),
        fourcc: image.format.fourcc,
        pitch: image.pitches[0] as usize,
        offsets: image.offsets.map(|v| v as usize),
    });
    input.bytes.resize(bytes.len(), 0);
    input.bytes.copy_from_slice(bytes);
    input.fourcc = image.format.fourcc;
    input.pitch = image.pitches[0] as usize;
    input.offsets = image.offsets.map(|v| v as usize);
    Ok(())
}

pub(super) unsafe fn query_surface_attributes(
    d: &Driver,
    config: u32,
    attrs: *mut VASurfaceAttrib,
    n: *mut u32,
) -> R {
    if !d.configs.contains_key(&config) && !d.enc_configs.contains_key(&config) {
        return Err(VA_STATUS_ERROR_INVALID_CONFIG as VAStatus);
    }
    if n.is_null() {
        return Err(INVALID);
    }
    let mut values = vec![
        surface_attr(VASurfaceAttribPixelFormat, NV12, true),
        surface_attr(VASurfaceAttribMinWidth, 2, false),
        surface_attr(VASurfaceAttribMinHeight, 2, false),
        surface_attr(VASurfaceAttribMaxWidth, MAX_DIM, false),
        surface_attr(VASurfaceAttribMaxHeight, MAX_DIM, false),
        surface_attr(VASurfaceAttribMemoryType, crate::offered_memory_types(), true),
        surface_attr(
            VASurfaceAttribUsageHint,
            VA_SURFACE_ATTRIB_USAGE_HINT_DECODER,
            true,
        ),
        external_buffer_attr(),
    ];
    if let Some(&(profile, _)) = d.enc_configs.get(&config) {
        values[3] = surface_attr(VASurfaceAttribMaxWidth, ENC_MAX_DIM, false);
        values[4] = surface_attr(VASurfaceAttribMaxHeight, ENC_MAX_DIM, false);
        values[6] = surface_attr(
            VASurfaceAttribUsageHint,
            VA_SURFACE_ATTRIB_USAGE_HINT_ENCODER,
            true,
        );
        let rt = rt(profile);
        values.retain(|value| value.type_ != VASurfaceAttribPixelFormat);
        for (mask, fourcc) in [
            (VA_RT_FORMAT_YUV420, NV12),
            (VA_RT_FORMAT_YUV420_10, P010),
            (VA_RT_FORMAT_YUV444, YUV444P),
            (VA_RT_FORMAT_YUV444_10, Q416),
        ] {
            if rt & mask != 0 {
                values.push(surface_attr(VASurfaceAttribPixelFormat, fourcc, true));
            }
        }
    }
    if attrs.is_null() {
        return out(n, values.len() as u32);
    }
    let available = *n;
    out(n, values.len() as u32)?;
    if available < values.len() as u32 {
        return Err(VA_STATUS_ERROR_MAX_NUM_EXCEEDED as VAStatus);
    }
    std::ptr::copy_nonoverlapping(values.as_ptr(), attrs, values.len());
    Ok(())
}
