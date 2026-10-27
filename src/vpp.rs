//! VA VideoProc (`VAProfileNone` + `VAEntrypointVideoProc`) on the RM video processor
//! (`nvrm::video::vpp`): bilinear scaling, cropping, output regions with a background, and
//! conversion between every surface format, all on the GPU. No filters. Offered only on GPUs whose
//! compute kernels are hardware-validated ([`Processor::supported`]).
//!
//! A processed surface keeps its picture in its exported backing (or the client's imported
//! buffer); export, encoding, read-back and further processing read it there.
use crate::{
    array, count, encode::Input, import::Dma, log, out, va, va::*, Completion, Destination, Driver, Format, Pending, FAILED, INVALID, NV12, P010, P012, P016, Q416, R, RGB_BGRA, RGB_RGBA, UNIMPL, YUV444P, ENC_MAX_DIM,
};
use nvrm::video::vpp::{self as rm, Colour, Matrix, Operation, Processor, Rect};
use std::{
    ffi::c_void,
    mem::size_of,
    sync::{mpsc, Arc},
    thread,
};

/// Surface formats VideoProc reads and writes.
pub(crate) const FORMATS: [u32; 12] = [
    NV12, P010, P012, P016, YUV444P, Q416,
    VA_FOURCC_BGRA, VA_FOURCC_BGRX, VA_FOURCC_ARGB, VA_FOURCC_XRGB, VA_FOURCC_RGBA, VA_FOURCC_RGBX,
];
pub(crate) const RT_FORMATS: u32 = VA_RT_FORMAT_YUV420 | VA_RT_FORMAT_YUV420_10 | VA_RT_FORMAT_YUV420_12
    | VA_RT_FORMAT_YUV444 | VA_RT_FORMAT_YUV444_10 | VA_RT_FORMAT_YUV444_12 | VA_RT_FORMAT_RGB32;
const STANDARDS: [VAProcColorStandardType; 12] = [
    VAProcColorStandardBT601,
    VAProcColorStandardBT709,
    VAProcColorStandardBT470M,
    VAProcColorStandardBT470BG,
    VAProcColorStandardSMPTE170M,
    VAProcColorStandardSMPTE240M,
    VAProcColorStandardGenericFilm,
    VAProcColorStandardSRGB,
    VAProcColorStandardXVYCC601,
    VAProcColorStandardXVYCC709,
    VAProcColorStandardBT2020,
    VAProcColorStandardExplicit,
];
static STANDARD_LIST: [VAProcColorStandardType; 12] = STANDARDS;
static FORMAT_LIST: [u32; 12] = FORMATS;

/// One side's colour description as the client gave it.
#[derive(Clone, Copy)]
pub(crate) struct Side {
    standard: VAProcColorStandardType,
    properties: VAProcColorProperties,
}

/// A rendered pipeline, its client pointers already read.
pub(crate) struct Pipeline {
    surface: u32,
    src_rect: Option<VARectangle>,
    dst_rect: Option<VARectangle>,
    input: Side,
    output: Side,
    background: u32,
}

#[derive(Default)]
pub(crate) struct Context {
    pub target: Option<u32>,
    pipeline: Option<Pipeline>,
}

/// What a pipeline reads: a dma-buf (key: its surface) or the planes a client uploaded.
pub(crate) enum Source {
    Dma(Arc<Dma>, u32),
    Host(Input, u32, u32),
}

enum Command {
    Process { src: Source, dst: Arc<Dma>, key: u32, op: Operation, wait: Vec<Arc<Completion>>, done: Pending },
    Download { src: Arc<Dma>, key: u32, rect: Rect, dest: Destination, pitch: usize, offsets: [usize; 3], wait: Vec<Arc<Completion>>, reply: mpsc::Sender<R> },
    Forget(u32),
    Quit,
}

/// Owns the RM processor (thread-confined like every RM object); runs commands in order.
pub(crate) struct Worker {
    tx: mpsc::Sender<Command>,
    join: Option<thread::JoinHandle<()>>,
}

fn failed(e: nvrm::video::VideoError) -> VAStatus {
    eprintln!("rustnv: VideoProc: {e}");
    FAILED
}

fn run(p: &mut Processor, src: &Source, dst: &Dma, key: u32, op: &Operation) -> R {
    let target = dst.picture(u64::from(key))?;
    match src {
        Source::Dma(dma, k) => p.process(&dma.picture(u64::from(*k))?, &target, op),
        Source::Host(input, w, h) => {
            let format = Format::of_fourcc(input.fourcc).or(if input.fourcc == P016 { Format::of_fourcc(P012) } else { None }).ok_or(INVALID)?;
            let pixels = pixels_of(format, input.fourcc);
            let picture = rm::Picture { pixels, width: *w, height: *h, memory: rm::Memory::Host { bytes: &input.bytes, pitch: input.pitch, offsets: input.offsets } };
            p.process(&picture, &target, op)
        }
    }
    .map_err(failed)
}

fn pixels_of(format: Format, fourcc: u32) -> rm::Pixels {
    if RGB_RGBA.contains(&fourcc) {
        rm::Pixels::Rgb(rm::RgbOrder::Rgba)
    } else if RGB_BGRA.contains(&fourcc) {
        rm::Pixels::Rgb(rm::RgbOrder::Bgra)
    } else {
        let bits = if fourcc == P016 { 16 } else { format.bit_depth };
        if format.chroma_444 { rm::Pixels::Yuv444 { bits } } else { rm::Pixels::Yuv420 { bits } }
    }
}

impl Worker {
    pub fn new(pci: [u32; 4]) -> R<Self> {
        let (tx, rx) = mpsc::channel::<Command>();
        let (ready_tx, ready_rx) = mpsc::channel();
        let join = thread::Builder::new()
            .name("rustnv-vpp".into())
            .spawn(move || {
                let mut p = match Processor::open(pci) {
                    Ok(p) => {
                        let _ = ready_tx.send(Ok(()));
                        p
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(failed(e)));
                        return;
                    }
                };
                while let Ok(command) = rx.recv() {
                    match command {
                        Command::Process { src, dst, key, op, wait, done } => {
                            let result = wait.iter().try_for_each(|w| w.wait()).and_then(|()| run(&mut p, &src, &dst, key, &op));
                            done.finish(result);
                        }
                        Command::Download { src, key, rect, dest, pitch, offsets, wait, reply } => {
                            // SAFETY: `Destination` documents the caller keeps the range valid.
                            let dest = unsafe { std::slice::from_raw_parts_mut(dest.0, dest.1) };
                            let result = wait.iter().try_for_each(|w| w.wait()).and_then(|()| {
                                p.download(&src.picture(u64::from(key))?, rect, dest, pitch, offsets).map_err(failed)
                            });
                            let _ = reply.send(result);
                        }
                        Command::Forget(key) => p.forget(u64::from(key)),
                        Command::Quit => break,
                    }
                }
            })
            .map_err(|_| FAILED)?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self { tx, join: Some(join) }),
            result => {
                let _ = join.join();
                Err(result.unwrap_or(Err(FAILED)).err().unwrap_or(FAILED))
            }
        }
    }
    /// Queues `op` from `src` into `dst` once `wait` completed.
    pub fn process(&self, src: Source, dst: Arc<Dma>, key: u32, op: Operation, wait: Vec<Arc<Completion>>) -> Arc<Completion> {
        let done = Completion::new();
        let _ = self.tx.send(Command::Process { src, dst, key, op, wait, done: Pending(done.clone()) });
        done
    }
    /// Reads the `rect` of `src` into `dest` (plane `p` at `offsets[p]`, `pitch` bytes per row)
    /// once `wait` completed.
    pub fn download(&self, src: Arc<Dma>, key: u32, rect: Rect, dest: &mut [u8], pitch: usize, offsets: [usize; 3], wait: Vec<Arc<Completion>>) -> R {
        let (tx, rx) = mpsc::channel();
        let dest = Destination(dest.as_mut_ptr(), dest.len());
        self.tx.send(Command::Download { src, key, rect, dest, pitch, offsets, wait, reply: tx }).map_err(|_| FAILED)?;
        rx.recv().map_err(|_| FAILED)?
    }
    pub fn forget(&self, key: u32) {
        let _ = self.tx.send(Command::Forget(key));
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Quit);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

// ---- Colour resolution -------------------------------------------------------------------------

/// Known colour properties of one side: the primaries class (0: unknown), the transfer class (0:
/// unknown; the SDR gamma family 1/6/11/13/14/15 is one class, the transfer every VA driver leaves
/// alone), and the matrix code (None: unknown).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Properties {
    primaries: u32,
    transfer: u32,
    matrix: Option<u8>,
}

/// The SDR-era primaries (BT.709, BT.470M/BG, SMPTE 170M/240M, generic film) are one class:
/// converting between them is a matrix change only, as in every VA driver and GStreamer's
/// videoconvert (`primaries-mode=none`); BT.2020, P3 and the rest are gamuts of their own.
fn primaries_class(p: u8) -> u32 {
    match p {
        0 | 2 => 0,
        1 | 4..=8 => 1,
        p => u32::from(p),
    }
}

fn transfer_class(t: u8) -> u32 {
    match t {
        0 | 2 => 0,
        1 | 6 | 11 | 13 | 14 | 15 => 1,
        t => u32::from(t),
    }
}

/// Step 1: the properties a side names. Fixed standards imply theirs (va_vpp.h); the explicit
/// fields are read only for `VAProcColorStandardExplicit`.
fn properties(side: &Side) -> R<Properties> {
    let fixed = |p: u8, t: u8, m: u8| Properties { primaries: primaries_class(p), transfer: transfer_class(t), matrix: Some(m) };
    Ok(match side.standard {
        va::VAProcColorStandardNone => Properties { primaries: 0, transfer: 0, matrix: None },
        va::VAProcColorStandardBT601 => fixed(6, 6, 6),
        va::VAProcColorStandardBT709 => fixed(1, 1, 1),
        va::VAProcColorStandardBT470M => fixed(4, 4, 4),
        va::VAProcColorStandardBT470BG => fixed(5, 5, 5),
        va::VAProcColorStandardSMPTE170M => fixed(6, 6, 6),
        va::VAProcColorStandardSMPTE240M => fixed(7, 7, 7),
        va::VAProcColorStandardGenericFilm => fixed(8, 1, 1),
        va::VAProcColorStandardSRGB => fixed(1, 13, 0),
        va::VAProcColorStandardXVYCC601 => fixed(1, 11, 5),
        va::VAProcColorStandardXVYCC709 => fixed(1, 11, 1),
        va::VAProcColorStandardBT2020 => fixed(9, 14, 9),
        va::VAProcColorStandardExplicit => {
            let p = side.properties;
            Properties {
                primaries: primaries_class(p.colour_primaries),
                transfer: transfer_class(p.transfer_characteristics),
                matrix: (p.matrix_coefficients != 2).then_some(p.matrix_coefficients),
            }
        }
        s => {
            log::log!("VideoProc: unsupported colour standard {s}");
            return Err(UNIMPL);
        }
    })
}

/// The YUV matrix of H.273 `matrix_coefficients`.
fn matrix(code: u8) -> R<Matrix> {
    Ok(match code {
        1 => Matrix::Bt709,
        4 => Matrix::Fcc,
        5 | 6 => Matrix::Bt601,
        7 => Matrix::Smpte240,
        9 => Matrix::Bt2020,
        m => {
            log::log!("VideoProc: unsupported YUV matrix_coefficients {m}");
            return Err(UNIMPL);
        }
    })
}

fn by_height(height: u32) -> Matrix {
    if height >= 720 { Matrix::Bt709 } else { Matrix::Bt601 }
}

fn siting(location: u8, pixels_420: bool) -> [f32; 2] {
    let x = if location & VA_CHROMA_SITING_HORIZONTAL_CENTER as u8 != 0 && pixels_420 { 1.0 } else { 0.5 };
    let y = match location & 3 {
        1 => 0.5,
        3 => 1.5,
        _ => 1.0,
    };
    [x, y]
}

/// Steps 2 and 3 of the colour resolution: unknown matrices and ranges are filled in per pixel
/// type (RGB: identity, full range; YUV: inherited or by picture height, limited range), unknown
/// primaries and transfer follow the other side, and known, different primaries or transfers are
/// refused rather than treated as a matrix change. The identity matrix named for a YUV picture
/// counts as unknown: clients copy it from RGB metadata (FFmpeg converting RGB to YUV).
fn resolve(input: &Side, in_rgb: bool, in_height: u32, output: &Side, out_rgb: bool, out_height: u32) -> R<(Colour, Colour)> {
    let (pi, po) = (properties(input)?, properties(output)?);
    if pi.primaries != 0 && po.primaries != 0 && pi.primaries != po.primaries {
        log::log!("VideoProc: colour primaries {} -> {} need a gamut conversion", pi.primaries, po.primaries);
        return Err(UNIMPL);
    }
    if pi.transfer != 0 && po.transfer != 0 && pi.transfer != po.transfer {
        log::log!("VideoProc: transfer {} -> {} needs tone mapping", pi.transfer, po.transfer);
        return Err(UNIMPL);
    }
    let yuv = |m: Option<u8>| m.filter(|&m| m != 0);
    let in_matrix = match (in_rgb, yuv(pi.matrix)) {
        (true, _) => Matrix::Rgb,
        (false, Some(m)) => matrix(m)?,
        (false, None) => by_height(in_height),
    };
    let out_matrix = match (out_rgb, yuv(po.matrix)) {
        (true, _) => Matrix::Rgb,
        (false, Some(m)) => matrix(m)?,
        (false, None) if !in_rgb => in_matrix,
        (false, None) => by_height(out_height),
    };
    let range = |side: &Side, rgb: bool| match u32::from(side.properties.color_range) {
        VA_SOURCE_RANGE_FULL => true,
        VA_SOURCE_RANGE_REDUCED => false,
        _ => rgb,
    };
    Ok((
        Colour { matrix: in_matrix, full_range: range(input, in_rgb), chroma_site: siting(input.properties.chroma_sample_location, true) },
        Colour { matrix: out_matrix, full_range: range(output, out_rgb), chroma_site: siting(output.properties.chroma_sample_location, true) },
    ))
}

// ---- VA entry points ----------------------------------------------------------------------------

pub(crate) unsafe fn get_attributes(d: &mut Driver, profile: VAProfile, attrs: *mut VAConfigAttrib, n: i32) -> R {
    if profile != VAProfileNone || !d.vpp_supported() {
        return Err(VA_STATUS_ERROR_UNSUPPORTED_PROFILE as VAStatus);
    }
    for i in 0..count(n)? {
        let a = array(attrs, count(n)?)?[i];
        (*attrs.add(i)).value = if a.type_ == VAConfigAttribRTFormat { RT_FORMATS } else { VA_ATTRIB_NOT_SUPPORTED };
    }
    Ok(())
}

pub(crate) unsafe fn create_config(d: &mut Driver, profile: VAProfile, attrs: *mut VAConfigAttrib, n: i32, id: *mut u32) -> R {
    if profile != VAProfileNone || !d.vpp_supported() {
        return Err(VA_STATUS_ERROR_UNSUPPORTED_PROFILE as VAStatus);
    }
    if id.is_null() {
        return Err(INVALID);
    }
    for a in array(attrs, count(n)?)? {
        if a.type_ != VAConfigAttribRTFormat || a.value & !RT_FORMATS != 0 {
            return Err(VA_STATUS_ERROR_ATTR_NOT_SUPPORTED as VAStatus);
        }
    }
    let key = d.id()?;
    d.vpp_configs.insert(key);
    out(id, key)
}

pub(crate) unsafe fn query_config(p: *mut VAProfile, e: *mut VAEntrypoint, attrs: *mut VAConfigAttrib, n: *mut i32) -> R {
    if p.is_null() || e.is_null() || attrs.is_null() || n.is_null() {
        return Err(INVALID);
    }
    out(p, VAProfileNone)?;
    out(e, VAEntrypointVideoProc)?;
    out(attrs, VAConfigAttrib { type_: VAConfigAttribRTFormat, value: RT_FORMATS })?;
    out(n, 1)
}

/// The context's size means nothing to VideoProc (GStreamer passes 0 x 0); each pipeline names
/// its own surfaces.
pub(crate) unsafe fn create_context(d: &mut Driver, w: i32, h: i32, id: *mut u32) -> R {
    if id.is_null() || w < 0 || h < 0 {
        return Err(INVALID);
    }
    let key = d.id()?;
    d.vpp_contexts.insert(key, Context::default());
    out(id, key)
}

pub(crate) fn buffer_shape(kind: VABufferType, size: u32) -> bool {
    match kind {
        va::VAProcPipelineParameterBufferType => size as usize == size_of::<VAProcPipelineParameterBuffer>(),
        // Accepted so that rendering it reports VA_STATUS_ERROR_UNSUPPORTED_FILTER.
        va::VAProcFilterParameterBufferType => size != 0,
        _ => false,
    }
}

pub(crate) fn begin_picture(d: &mut Driver, id: u32, target: u32) -> R {
    d.surface(target)?;
    if crate::busy(d, target) {
        return Err(VA_STATUS_ERROR_SURFACE_BUSY as VAStatus);
    }
    let c = d.vpp_contexts.get_mut(&id).ok_or(VA_STATUS_ERROR_INVALID_CONTEXT as VAStatus)?;
    if c.target.is_some() {
        return Err(INVALID);
    }
    c.target = Some(target);
    c.pipeline = None;
    Ok(())
}

pub(crate) unsafe fn render_picture(d: &mut Driver, id: u32, ids: *mut u32, n: i32) -> R {
    if d.vpp_contexts.get(&id).ok_or(VA_STATUS_ERROR_INVALID_CONTEXT as VAStatus)?.target.is_none() {
        return Err(INVALID);
    }
    for key in array(ids, count(n)?)? {
        let b = d.buffer(*key)?;
        if b.context != Some(id) || b.mapped {
            return Err(VA_STATUS_ERROR_INVALID_BUFFER as VAStatus);
        }
        if b.kind == va::VAProcFilterParameterBufferType {
            return Err(VA_STATUS_ERROR_UNSUPPORTED_FILTER as VAStatus);
        }
        if b.kind != va::VAProcPipelineParameterBufferType || b.count != 1 {
            return Err(VA_STATUS_ERROR_UNSUPPORTED_BUFFERTYPE as VAStatus);
        }
        let p: VAProcPipelineParameterBuffer = std::ptr::read_unaligned(b.bytes.as_ptr().cast());
        if p.num_filters != 0 {
            return Err(VA_STATUS_ERROR_UNSUPPORTED_FILTER as VAStatus);
        }
        let field = (VA_TOP_FIELD | VA_BOTTOM_FIELD) as u32;
        if p.num_forward_references != 0 || p.num_backward_references != 0 || p.num_additional_outputs != 0
            || p.rotation_state != VA_ROTATION_NONE as u32 || p.mirror_state != VA_MIRROR_NONE as u32
            || !p.blend_state.is_null() || p.filter_flags & field != 0
        {
            log::log!("VideoProc: unsupported pipeline (references, rotation, mirroring, blending or field processing)");
            return Err(UNIMPL);
        }
        let c = d.vpp_contexts.get_mut(&id).unwrap();
        if c.pipeline.is_some() {
            log::log!("VideoProc: one pipeline per picture (no composition)");
            return Err(UNIMPL);
        }
        let rect = |r: *const VARectangle| (!r.is_null()).then(|| *r);
        c.pipeline = Some(Pipeline {
            surface: p.surface,
            src_rect: rect(p.surface_region),
            dst_rect: rect(p.output_region),
            input: Side { standard: p.surface_color_standard, properties: p.input_color_properties },
            output: Side { standard: p.output_color_standard, properties: p.output_color_properties },
            background: p.output_background_color,
        });
    }
    Ok(())
}

/// Queues the rendered pipeline; vaSyncSurface on the target waits for it.
pub(crate) fn end_picture(d: &mut Driver, id: u32) -> R {
    let c = d.vpp_contexts.get_mut(&id).ok_or(VA_STATUS_ERROR_INVALID_CONTEXT as VAStatus)?;
    let target = c.target.take().ok_or(INVALID)?;
    let p = c.pipeline.take().ok_or(INVALID)?;
    let s = d.surface(p.surface)?;
    let (sw, sh, sfourcc) = (s.extent.0, s.extent.1, s.fourcc);
    let t = d.surface(target)?;
    let (tw, th, tfourcc) = (t.extent.0, t.extent.1, t.fourcc);
    let rgb = |fourcc: u32| Format::of_fourcc(fourcc).is_some_and(Format::rgb) || crate::import::Kind::of(fourcc).is_some_and(|k| matches!(k, crate::import::Kind::Bgra | crate::import::Kind::Rgba));
    let (src_colour, dst_colour) = resolve(&p.input, rgb(sfourcc), sh, &p.output, rgb(tfourcc), th)?;
    log::debug!(
        "VideoProc {} -> {target}: standards {} -> {}, range {} -> {}, colour {src_colour:?} -> {dst_colour:?}",
        p.surface, p.input.standard, p.output.standard, p.input.properties.color_range, p.output.properties.color_range
    );
    let rect = |r: Option<VARectangle>, w: u32, h: u32| {
        r.map_or(Rect { x: 0, y: 0, width: w, height: h }, |r| Rect { x: i32::from(r.x), y: i32::from(r.y), width: u32::from(r.width), height: u32::from(r.height) })
    };
    let op = Operation {
        src_rect: rect(p.src_rect, sw, sh),
        dst_rect: rect(p.dst_rect, tw, th),
        src_colour,
        dst_colour,
        background: p.background,
    };
    let s = op.src_rect;
    if s.x < 0 || s.y < 0 || s.width == 0 || s.height == 0 || s.x as u32 + s.width > sw || s.y as u32 + s.height > sh || op.dst_rect.width == 0 || op.dst_rect.height == 0 {
        return Err(INVALID);
    }
    let source = match d.gpu_view(p.surface)? {
        Some((dma, wait)) => (Source::Dma(dma, p.surface), wait),
        None => {
            let input = d.surface(p.surface)?.input.as_ref().ok_or(VA_STATUS_ERROR_INVALID_SURFACE as VAStatus)?;
            let copy = Input { bytes: input.bytes.clone(), fourcc: input.fourcc, pitch: input.pitch, offsets: input.offsets };
            (Source::Host(copy, sw, sh), Vec::new())
        }
    };
    let (dst, mut wait) = d.target_view(target)?;
    wait.extend(source.1);
    let done = d.vpp_worker()?.process(source.0, dst, target, op, wait);
    let t = d.surfaces.get_mut(&target).unwrap();
    t.processed = Some(done);
    t.decode = None;
    t.copy = None;
    t.input = None;
    t.output_dirty = false;
    Ok(())
}

/// Reads the `rect` of a surface whose picture lives in a dma-buf (processed, or imported).
pub(crate) fn readback(m: &std::sync::Mutex<Driver>, id: u32, rect: Rect, buffer: u32) -> R {
    let (dma, wait, fourcc) = {
        let mut d = crate::lock(m)?;
        let fourcc = d.surface(id)?.fourcc;
        let (dma, wait) = d.gpu_view(id)?.ok_or(VA_STATUS_ERROR_INVALID_SURFACE as VAStatus)?;
        d.vpp_worker()?;
        (dma, wait, fourcc)
    };
    for w in &wait {
        w.wait()?;
    }
    let format = Format::of_fourcc(if fourcc == P016 { P012 } else { fourcc }).ok_or(VA_STATUS_ERROR_INVALID_IMAGE_FORMAT as VAStatus)?;
    let (pitch, offsets, size) = format.packed(rect.width, rect.height);
    let mut guard = crate::lock(m)?;
    let d = &mut *guard;
    let worker = d.vpp.as_ref().ok_or(FAILED)?;
    let dest = d.buffers.get_mut(&buffer).ok_or(VA_STATUS_ERROR_INVALID_IMAGE as VAStatus)?;
    if dest.mapped || dest.bytes.len() < size {
        return Err(INVALID);
    }
    worker.download(dma, id, rect, &mut dest.bytes, pitch as usize, offsets.map(|v| v as usize), Vec::new())
}

// ---- vtable_vpp ---------------------------------------------------------------------------------

unsafe fn vpp_context(ctx: VADriverContextP, context: u32) -> R {
    match crate::invoke(ctx, |d| d.vpp_contexts.get(&context).map(|_| ()).ok_or(VA_STATUS_ERROR_INVALID_CONTEXT as VAStatus)) {
        0 => Ok(()),
        e => Err(e),
    }
}

pub(crate) unsafe extern "C" fn query_filters(ctx: VADriverContextP, context: u32, _filters: *mut VAProcFilterType, n: *mut u32) -> VAStatus {
    match vpp_context(ctx, context).and_then(|()| out(n, 0)) {
        Ok(()) => 0,
        Err(e) => e,
    }
}

pub(crate) unsafe extern "C" fn query_filter_caps(ctx: VADriverContextP, context: u32, _kind: VAProcFilterType, _caps: *mut c_void, _n: *mut u32) -> VAStatus {
    match vpp_context(ctx, context) {
        Ok(()) => VA_STATUS_ERROR_UNSUPPORTED_FILTER as VAStatus,
        Err(e) => e,
    }
}

/// Points a caps list at the driver's static list, as Mesa does. Clients do not consistently
/// initialise these fields (FFmpeg passes stack garbage), so nothing is written through them.
unsafe fn list<T: Copy>(dst: *mut *mut T, n: *mut u32, values: &'static [T]) {
    *dst = values.as_ptr().cast_mut();
    *n = values.len() as u32;
}

pub(crate) unsafe extern "C" fn query_pipeline_caps(ctx: VADriverContextP, context: u32, _filters: *mut u32, n: u32, caps: *mut VAProcPipelineCaps) -> VAStatus {
    if let Err(e) = vpp_context(ctx, context) {
        return e;
    }
    if caps.is_null() {
        return INVALID;
    }
    if n != 0 {
        return VA_STATUS_ERROR_UNSUPPORTED_FILTER as VAStatus;
    }
    let c = &mut *caps;
    c.pipeline_flags = 0;
    c.filter_flags = VA_FILTER_SCALING_FAST | VA_FILTER_SCALING_HQ;
    c.num_forward_references = 0;
    c.num_backward_references = 0;
    list(&mut c.input_color_standards, &mut c.num_input_color_standards, &STANDARD_LIST);
    list(&mut c.output_color_standards, &mut c.num_output_color_standards, &STANDARD_LIST);
    c.rotation_flags = 1 << VA_ROTATION_NONE;
    c.blend_flags = 0;
    c.mirror_flags = 0;
    c.num_additional_outputs = 0;
    list(&mut c.input_pixel_format, &mut c.num_input_pixel_formats, &FORMAT_LIST);
    list(&mut c.output_pixel_format, &mut c.num_output_pixel_formats, &FORMAT_LIST);
    c.max_input_width = ENC_MAX_DIM;
    c.max_input_height = ENC_MAX_DIM;
    c.min_input_width = 2;
    c.min_input_height = 2;
    c.max_output_width = ENC_MAX_DIM;
    c.max_output_height = ENC_MAX_DIM;
    c.min_output_width = 2;
    c.min_output_height = 2;
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn side(standard: VAProcColorStandardType, primaries: u8, transfer: u8, matrix: u8, range: u32) -> Side {
        Side {
            standard,
            properties: VAProcColorProperties { colour_primaries: primaries, transfer_characteristics: transfer, matrix_coefficients: matrix, color_range: range as u8, ..Default::default() },
        }
    }

    #[test]
    fn colour_resolution() {
        let none = side(VAProcColorStandardNone, 0, 0, 0, 0);
        // Fixed standards ignore the explicit fields.
        let bt709 = side(VAProcColorStandardBT709, 9, 16, 9, 0);
        let (i, o) = resolve(&bt709, false, 1080, &bt709, false, 1080).unwrap();
        assert_eq!((i.matrix, o.matrix, i.full_range), (Matrix::Bt709, Matrix::Bt709, false));
        // Explicit: same primaries and transfer, matrix change accepted.
        let a = side(VAProcColorStandardExplicit, 1, 1, 1, 0);
        let b = side(VAProcColorStandardExplicit, 1, 1, 5, VA_SOURCE_RANGE_FULL);
        let (i, o) = resolve(&a, false, 1080, &b, false, 1080).unwrap();
        assert_eq!((i.matrix, o.matrix, o.full_range), (Matrix::Bt709, Matrix::Bt601, true));
        // One side unknown: inherits, not a colour change.
        let hdr = side(VAProcColorStandardExplicit, 9, 16, 9, 0);
        let (_, o) = resolve(&hdr, false, 2160, &none, false, 2160).unwrap();
        assert_eq!(o.matrix, Matrix::Bt2020);
        // Both unknown: matrix by height, YUV limited, RGB full.
        let (i, o) = resolve(&none, false, 480, &none, true, 480).unwrap();
        assert_eq!((i.matrix, i.full_range, o.matrix, o.full_range), (Matrix::Bt601, false, Matrix::Rgb, true));
        // RGB input, YUV output without a matrix: by output height.
        assert_eq!(resolve(&none, true, 1080, &none, false, 719).unwrap().1.matrix, Matrix::Bt601);
        assert_eq!(resolve(&none, true, 480, &none, false, 720).unwrap().1.matrix, Matrix::Bt709);
        // SDR-era primaries (BT.601 <-> BT.709) are one class: a matrix change. Known primaries of
        // different gamuts (BT.709 -> BT.2020) or transfers (PQ -> SDR): refused.
        let bt601 = side(VAProcColorStandardBT601, 0, 0, 0, 0);
        let (i, o) = resolve(&bt601, false, 480, &bt709, false, 480).unwrap();
        assert_eq!((i.matrix, o.matrix), (Matrix::Bt601, Matrix::Bt709));
        let bt2020 = side(VAProcColorStandardBT2020, 0, 0, 0, 0);
        assert_eq!(resolve(&bt709, false, 1080, &bt2020, false, 1080).unwrap_err(), UNIMPL);
        let sdr2020 = side(VAProcColorStandardExplicit, 9, 14, 9, 0);
        assert_eq!(resolve(&hdr, false, 2160, &sdr2020, false, 2160).unwrap_err(), UNIMPL);
        // BT.709 video to sRGB output: same primaries, SDR transfer family.
        let srgb = side(VAProcColorStandardSRGB, 0, 0, 0, 0);
        assert!(resolve(&bt709, false, 1080, &srgb, true, 1080).is_ok());
        // Chroma siting.
        let centre = Side { properties: VAProcColorProperties { chroma_sample_location: (VA_CHROMA_SITING_HORIZONTAL_CENTER | VA_CHROMA_SITING_VERTICAL_TOP) as u8, ..Default::default() }, ..none };
        assert_eq!(resolve(&centre, false, 480, &none, false, 480).unwrap().0.chroma_site, [1.0, 0.5]);
    }
}
