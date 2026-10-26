mod backend;
mod encode;
pub mod va;

use std::{
    collections::{HashMap, HashSet},
    ffi::c_void,
    mem::{size_of, zeroed},
    os::fd::IntoRawFd,
    panic::{catch_unwind, AssertUnwindSafe},
    ptr, slice,
    sync::{mpsc, Arc, Condvar, Mutex, MutexGuard},
    thread,
};
use va::*;
type R<T = ()> = Result<T, VAStatus>;
const INVALID: VAStatus = VA_STATUS_ERROR_INVALID_PARAMETER as VAStatus;
const FAILED: VAStatus = VA_STATUS_ERROR_OPERATION_FAILED as VAStatus;
const UNIMPL: VAStatus = VA_STATUS_ERROR_UNIMPLEMENTED as VAStatus;
const NV12: u32 = u32::from_le_bytes(*b"NV12");
const P010: u32 = u32::from_le_bytes(*b"P010");
const YUV444P: u32 = u32::from_le_bytes(*b"444P");
const Q416: u32 = u32::from_le_bytes(*b"Q416");
const ENC_MAX_DIM: u32 = 8192;
const MAX_DIM: u32 = 4096;
const PROFILES: [VAProfile; 7] = [
    VAProfileH264Baseline,
    VAProfileH264ConstrainedBaseline,
    VAProfileH264Main,
    VAProfileH264High,
    VAProfileHEVCMain,
    VAProfileVP9Profile0,
    VAProfileAV1Profile0,
];
fn codec_of(profile: VAProfile) -> R<backend::Codec> {
    use backend::Codec::*;
    Ok(match profile {
        va::VAProfileH264Baseline | va::VAProfileH264ConstrainedBaseline | va::VAProfileH264Main | va::VAProfileH264High => H264,
        va::VAProfileHEVCMain => Hevc,
        va::VAProfileVP9Profile0 => Vp9,
        va::VAProfileAV1Profile0 => Av1,
        _ => return Err(VA_STATUS_ERROR_UNSUPPORTED_PROFILE as VAStatus),
    })
}
/// Element sizes of the codec's picture, IQ-matrix and slice parameter buffers.
fn parameter_sizes(codec: backend::Codec) -> (usize, Option<usize>, usize) {
    use backend::Codec::*;
    match codec {
        H264 => (size_of::<VAPictureParameterBufferH264>(), Some(size_of::<VAIQMatrixBufferH264>()), size_of::<VASliceParameterBufferH264>()),
        Hevc => (size_of::<VAPictureParameterBufferHEVC>(), Some(size_of::<VAIQMatrixBufferHEVC>()), size_of::<VASliceParameterBufferHEVC>()),
        Vp9 => (size_of::<VADecPictureParameterBufferVP9>(), None, size_of::<VASliceParameterBufferVP9>()),
        Av1 => (size_of::<VADecPictureParameterBufferAV1>(), None, size_of::<VASliceParameterBufferAV1>()),
    }
}

/// Buffers rendered for one picture, kept raw: the codec backend interprets the parameters.
struct Frame {
    target: u32,
    pic: Option<Vec<u8>>,
    iq: Option<Vec<u8>>,
    groups: Vec<Vec<Vec<u8>>>, // slice parameter buffers, one entry per element
    data: Vec<Vec<u8>>,
    events: Vec<(bool, usize)>, // true: parameter group, false: data buffer
}
/// Outcome of work queued on a decoder thread. VA calls wait on it without holding the driver lock.
struct Completion {
    result: Mutex<Option<R>>,
    ready: Condvar,
}
impl Completion {
    fn new() -> Arc<Self> {
        Arc::new(Self { result: Mutex::new(None), ready: Condvar::new() })
    }
    fn done(result: R) -> Arc<Self> {
        Arc::new(Self { result: Mutex::new(Some(result)), ready: Condvar::new() })
    }
    fn set(&self, result: R) {
        if let Ok(mut slot) = self.result.lock() {
            slot.get_or_insert(result);
            self.ready.notify_all();
        }
    }
    fn poll(&self) -> Option<R> {
        *self.result.lock().ok()?
    }
    fn wait(&self) -> R {
        let mut slot = self.result.lock().map_err(|_| FAILED)?;
        loop {
            if let Some(result) = *slot {
                return result;
            }
            slot = self.ready.wait(slot).map_err(|_| FAILED)?;
        }
    }
}
/// The worker's handle on a Completion: dropping it unfinished (worker exit or panic) fails the work,
/// so no VA call waits forever.
struct Pending(Arc<Completion>);
impl Pending {
    fn finish(self, result: R) {
        self.0.set(result);
    }
}
impl Drop for Pending {
    fn drop(&mut self) {
        self.0.set(Err(FAILED));
    }
}
enum Command {
    Decode(backend::Picture, Pending),
    Copy(u32, backend::Export, Pending),
    Readback(u32, mpsc::Sender<Result<Vec<u8>, String>>),
    Release(u32),
    Quit,
}
/// Owns one context's RM decoder; executes commands in submission order, so a picture's decode
/// precedes its copies, read-backs and release, and references precede the pictures using them.
struct Worker {
    tx: mpsc::Sender<Command>,
    join: Option<thread::JoinHandle<()>>,
}
fn gpu<T>(r: Result<T, String>) -> R<T> {
    r.map_err(|e| {
        eprintln!("rustnv: {e}");
        FAILED
    })
}
impl Worker {
    fn new(codec: backend::Codec, pci: [u32; 4], w: u32, h: u32, surfaces: usize) -> R<Self> {
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let join = thread::Builder::new()
            .name("rustnv-nvdec".into())
            .spawn(move || {
                let mut decoder = match backend::Decoder::new(codec, pci, w, h, surfaces) {
                    Ok(d) => {
                        let _ = ready_tx.send(Ok(()));
                        d
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                while let Ok(cmd) = rx.recv() {
                    match cmd {
                        Command::Decode(picture, done) => {
                            let result = decoder.decode(&picture);
                            done.finish(result.map_err(|e| {
                                eprintln!("rustnv: {e}");
                                VA_STATUS_ERROR_DECODING_ERROR as VAStatus
                            }));
                        }
                        Command::Copy(id, output, done) => {
                            done.finish(gpu(decoder.copy_into(id, &output)));
                        }
                        Command::Readback(id, reply) => {
                            let _ = reply.send(decoder.readback(id));
                        }
                        Command::Release(id) => decoder.release_surface(id),
                        Command::Quit => break,
                    }
                }
            })
            .map_err(|_| FAILED)?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                tx,
                join: Some(join),
            }),
            result => {
                let _ = join.join();
                match result {
                    Ok(Err(e)) => gpu(Err(e)),
                    _ => Err(FAILED),
                }
            }
        }
    }
    /// Queues work whose outcome is reported through the returned Completion.
    fn queue(&self, command: impl FnOnce(Pending) -> Command) -> Arc<Completion> {
        let done = Completion::new();
        // A dead worker drops the command, and with it the Pending, failing the work.
        let _ = self.tx.send(command(Pending(done.clone())));
        done
    }
    fn readback(&self, id: u32) -> R<Vec<u8>> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Command::Readback(id, tx))
            .map_err(|_| FAILED)?;
        gpu(rx.recv().map_err(|_| FAILED)?)
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
enum OutputCommand {
    Export(u32, u32, u32, mpsc::Sender<Result<backend::Export, String>>),
    Release(u32),
    Quit,
}
// The pool and its RM objects never leave this thread. Only duplicated dma-buf
// handles cross threads; retaining the pool pins already exported allocations.
struct OutputWorker {
    tx: mpsc::Sender<OutputCommand>,
    join: Option<thread::JoinHandle<()>>,
}
impl OutputWorker {
    fn new(pci: [u32; 4]) -> R<Self> {
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let join = thread::Builder::new()
            .name("rustnv-output".into())
            .spawn(move || {
                let mut pool = match backend::OutputPool::new(pci) {
                    Ok(pool) => {
                        let _ = ready_tx.send(Ok(()));
                        pool
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                while let Ok(command) = rx.recv() {
                    match command {
                        OutputCommand::Export(id, w, h, reply) => {
                            let _ = reply.send(pool.export(id, w, h));
                        }
                        OutputCommand::Release(id) => pool.release(id),
                        OutputCommand::Quit => break,
                    }
                }
            })
            .map_err(|_| FAILED)?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                tx,
                join: Some(join),
            }),
            result => {
                let _ = join.join();
                match result {
                    Ok(Err(e)) => gpu(Err(e)),
                    _ => Err(FAILED),
                }
            }
        }
    }
    fn export(&self, id: u32, w: u32, h: u32) -> R<backend::Export> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(OutputCommand::Export(id, w, h, tx))
            .map_err(|_| FAILED)?;
        gpu(rx.recv().map_err(|_| FAILED)?)
    }
}
impl Drop for OutputWorker {
    fn drop(&mut self) {
        let _ = self.tx.send(OutputCommand::Quit);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}
struct Context {
    codec: backend::Codec,
    worker: Arc<Worker>,
    width: u32,
    height: u32,
    targets: HashSet<u32>,
    frame: Option<Frame>,
}
// A VA surface can outlive its decoding context (FFmpeg drains its last output afterward).
struct Surface {
    width: u32,
    height: u32,
    fourcc: u32,
    owner: Option<u32>,
    worker: Option<Arc<Worker>>,
    extent: (u32, u32),
    /// Latest queued decode (None until the first EndPicture), and the copy into `output` after it.
    decode: Option<Arc<Completion>>,
    copy: Option<Arc<Completion>>,
    output: Option<backend::Export>,
    output_dirty: bool,
    input: Option<encode::Input>,
}
impl Surface {
    fn pending(&self) -> Vec<Arc<Completion>> {
        self.decode.iter().chain(&self.copy).cloned().collect()
    }
}
struct Buffer {
    context: Option<u32>,
    kind: VABufferType,
    size: u32,
    count: u32,
    bytes: Vec<u8>,
    mapped: bool,
    coded: Option<Box<VACodedBufferSegment>>,
    derived: Option<u32>,
}
struct Driver {
    /// The NVIDIA GPU behind the DRM device libva opened for this display.
    pci: [u32; 4],
    next: u32,
    configs: HashMap<u32, VAProfile>,
    enc_configs: HashMap<u32, (VAProfile, u32)>,
    enc_contexts: HashMap<u32, encode::Session>,
    surfaces: HashMap<u32, Surface>,
    contexts: HashMap<u32, Context>,
    buffers: HashMap<u32, Buffer>,
    images: HashMap<u32, VAImage>,
    output_worker: Option<OutputWorker>,
}
impl Driver {
    fn new(pci: [u32; 4]) -> Self {
        Self {
            pci,
            next: 1,
            configs: HashMap::new(),
            surfaces: HashMap::new(),
            enc_configs: HashMap::new(),
            enc_contexts: HashMap::new(),
            contexts: HashMap::new(),
            buffers: HashMap::new(),
            images: HashMap::new(),
            output_worker: None,
        }
    }
    fn id(&mut self) -> R<u32> {
        let id = self.next;
        self.next = id
            .checked_add(1)
            .ok_or(VA_STATUS_ERROR_MAX_NUM_EXCEEDED as VAStatus)?;
        Ok(id)
    }
    fn surface(&self, id: u32) -> R<&Surface> {
        self.surfaces
            .get(&id)
            .ok_or(VA_STATUS_ERROR_INVALID_SURFACE as VAStatus)
    }
    fn context(&self, id: u32) -> R<&Context> {
        self.contexts
            .get(&id)
            .ok_or(VA_STATUS_ERROR_INVALID_CONTEXT as VAStatus)
    }
    fn buffer(&self, id: u32) -> R<&Buffer> {
        self.buffers
            .get(&id)
            .ok_or(VA_STATUS_ERROR_INVALID_BUFFER as VAStatus)
    }
    /// Queues the copy into the exported backing after the latest decode; vaSyncSurface waits for it.
    fn update_output(&mut self, id: u32) -> R {
        let s = self.surfaces.get_mut(&id).ok_or(VA_STATUS_ERROR_INVALID_SURFACE as VAStatus)?;
        if s.decode.is_none() || !s.output_dirty {
            return Ok(());
        }
        let Some(output) = s.output.as_ref() else {
            return Ok(());
        };
        let output = gpu(output.try_clone())?;
        let worker = s.worker.as_ref().ok_or(FAILED)?;
        s.copy = Some(worker.queue(|done| Command::Copy(id, output, done)));
        s.output_dirty = false;
        Ok(())
    }
    fn export_output(&mut self, id: u32) -> R<backend::Export> {
        let s = self.surface(id)?;
        if s.output.is_none() {
            let (w, h) = (s.width, s.height);
            if self.output_worker.is_none() {
                self.output_worker = Some(OutputWorker::new(self.pci)?);
            }
            let output = self.output_worker.as_ref().unwrap().export(id, w, h)?;
            let s = self.surfaces.get_mut(&id).unwrap();
            s.output = Some(output);
            s.output_dirty = s.decode.is_some();
        }
        self.update_output(id)?;
        gpu(self.surface(id)?.output.as_ref().ok_or(FAILED)?.try_clone())
    }
}
unsafe fn array<'a, T>(p: *const T, n: usize) -> R<&'a [T]> {
    if n == 0 {
        return Ok(&[]);
    }
    if p.is_null() || n > isize::MAX as usize / size_of::<T>().max(1) {
        return Err(INVALID);
    }
    Ok(slice::from_raw_parts(p, n))
}
unsafe fn out<T>(p: *mut T, value: T) -> R {
    if p.is_null() {
        Err(INVALID)
    } else {
        p.write(value);
        Ok(())
    }
}
fn count(n: i32) -> R<usize> {
    usize::try_from(n).map_err(|_| INVALID)
}
fn dimensions(w: u32, h: u32, max: u32) -> R {
    if w == 0 || h == 0 || w > max || h > max || w % 2 != 0 || h % 2 != 0 {
        Err(VA_STATUS_ERROR_RESOLUTION_NOT_SUPPORTED as VAStatus)
    } else {
        Ok(())
    }
}
fn supported(p: VAProfile, e: VAEntrypoint) -> R {
    if !PROFILES.contains(&p) {
        Err(VA_STATUS_ERROR_UNSUPPORTED_PROFILE as VAStatus)
    } else if e != VAEntrypointVLD {
        Err(VA_STATUS_ERROR_UNSUPPORTED_ENTRYPOINT as VAStatus)
    } else {
        Ok(())
    }
}
fn attribute(t: VAConfigAttribType) -> u32 {
    match t {
        va::VAConfigAttribRTFormat => VA_RT_FORMAT_YUV420,
        va::VAConfigAttribDecSliceMode => VA_DEC_SLICE_MODE_NORMAL,
        va::VAConfigAttribDecProcessing => VA_DEC_PROCESSING_NONE,
        va::VAConfigAttribMaxPictureWidth | va::VAConfigAttribMaxPictureHeight => MAX_DIM,
        _ => VA_ATTRIB_NOT_SUPPORTED,
    }
}
unsafe fn invoke(ctx: VADriverContextP, f: impl FnOnce(&mut Driver) -> R) -> VAStatus {
    invoke_unlocked(ctx, |m| f(&mut *lock(m)?))
}
/// For calls that wait on GPU work: the closure takes the driver lock itself and must not hold it
/// while waiting, so other threads keep decoding and exporting meanwhile.
unsafe fn invoke_unlocked(ctx: VADriverContextP, f: impl FnOnce(&Mutex<Driver>) -> R) -> VAStatus {
    if ctx.is_null() || (*ctx).pDriverData.is_null() {
        return VA_STATUS_ERROR_INVALID_DISPLAY as VAStatus;
    }
    match catch_unwind(AssertUnwindSafe(|| f(&*((*ctx).pDriverData as *const Mutex<Driver>)))) {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => e,
        Err(_) => FAILED,
    }
}
fn lock(m: &Mutex<Driver>) -> R<MutexGuard<'_, Driver>> {
    m.lock().map_err(|_| FAILED)
}
/// NV12 contents of a surface once its latest decode completed; waits without the driver lock.
fn readback(m: &Mutex<Driver>, id: u32) -> R<Vec<u8>> {
    let (worker, decode) = {
        let d = lock(m)?;
        let s = d.surface(id)?;
        let invalid = VA_STATUS_ERROR_INVALID_SURFACE as VAStatus;
        (s.worker.clone().ok_or(invalid)?, s.decode.clone().ok_or(invalid)?)
    };
    decode.wait()?;
    worker.readback(id)
}
macro_rules! callback {
    ($name:ident($ctx:ident $(, $arg:ident : $ty:ty)*) |$d:ident| $body:block) => {
        unsafe extern "C" fn $name($ctx: VADriverContextP, $($arg: $ty),*) -> VAStatus { invoke($ctx, |$d| $body) }
    };
}
macro_rules! callback_unlocked {
    ($name:ident($ctx:ident $(, $arg:ident : $ty:ty)*) |$m:ident| $body:block) => {
        unsafe extern "C" fn $name($ctx: VADriverContextP, $($arg: $ty),*) -> VAStatus { invoke_unlocked($ctx, |$m| $body) }
    };
}
callback!(query_profiles(ctx, list: *mut VAProfile, n: *mut i32) |_d| {
    encode::query_profiles(list, n)
});
callback!(query_entrypoints(ctx, p: VAProfile, list: *mut VAEntrypoint, n: *mut i32) |_d| {
    encode::query_entrypoints(p, list, n)
});
callback!(get_config_attributes(ctx, p: VAProfile, e: VAEntrypoint, attrs: *mut VAConfigAttrib, n: i32) |_d| {
    if e == VAEntrypointEncSlice { return encode::get_attributes(p, attrs, n); }
    supported(p, e)?; for i in 0..count(n)? { let a = array(attrs, count(n)?)?[i]; (*attrs.add(i)).value = attribute(a.type_); } Ok(())
});
callback!(create_config(ctx, p: VAProfile, e: VAEntrypoint, attrs: *mut VAConfigAttrib, n: i32, id: *mut u32) |d| {
    if e == VAEntrypointEncSlice { return encode::create_config(d, p, attrs, n, id); }
    supported(p, e)?; if id.is_null() { return Err(INVALID); }
    for a in array(attrs, count(n)?)? {
        let value = attribute(a.type_);
        if value == VA_ATTRIB_NOT_SUPPORTED || a.value != value { return Err(VA_STATUS_ERROR_ATTR_NOT_SUPPORTED as VAStatus); }
    }
    let key = d.id()?; d.configs.insert(key, p); out(id, key)
});
callback!(destroy_config(ctx, id: u32) |d| { if d.enc_configs.remove(&id).is_some() { return Ok(()); } d.configs.remove(&id).ok_or(VA_STATUS_ERROR_INVALID_CONFIG as VAStatus)?; Ok(()) });
callback!(query_config(ctx, id: u32, p: *mut VAProfile, e: *mut VAEntrypoint, attrs: *mut VAConfigAttrib, n: *mut i32) |d| {
    if let Some(&(profile, _)) = d.enc_configs.get(&id) { return encode::query_config(profile, p, e, attrs, n); }
    let profile = *d.configs.get(&id).ok_or(VA_STATUS_ERROR_INVALID_CONFIG as VAStatus)?;
    if p.is_null() || e.is_null() || attrs.is_null() || n.is_null() { return Err(INVALID); }
    out(p, profile)?; out(e, VAEntrypointVLD)?;
    out(attrs, VAConfigAttrib { type_: VAConfigAttribRTFormat, value: VA_RT_FORMAT_YUV420 })?; out(n, 1)
});
callback!(create_surfaces2(ctx, format: u32, w: u32, h: u32, surfaces: *mut u32, n: u32, attrs: *mut VASurfaceAttrib, na: u32) |d| {
    dimensions(w, h, ENC_MAX_DIM)?;
    if format & !(VA_RT_FORMAT_YUV420 | VA_RT_FORMAT_YUV420_10 | VA_RT_FORMAT_YUV444 | VA_RT_FORMAT_YUV444_10) != 0 || format == 0 {
        return Err(VA_STATUS_ERROR_UNSUPPORTED_RT_FORMAT as VAStatus);
    }
    if n == 0 || surfaces.is_null() { return Err(INVALID); }
    let mut fourcc = match format {
        VA_RT_FORMAT_YUV420_10 => P010,
        VA_RT_FORMAT_YUV444 => YUV444P,
        VA_RT_FORMAT_YUV444_10 => Q416,
        _ => NV12,
    };
    for a in array(attrs, na as usize)? {
        if a.flags & VA_SURFACE_ATTRIB_SETTABLE == 0 { continue; }
        if a.value.type_ != VAGenericValueTypeInteger { return Err(INVALID); }
        let v = a.value.value.i as u32;
        match a.type_ {
            va::VASurfaceAttribPixelFormat if encode::INPUT_FORMATS.contains(&v) => { fourcc = v; },
            va::VASurfaceAttribMemoryType if v == VA_SURFACE_ATTRIB_MEM_TYPE_VA => (),
            va::VASurfaceAttribUsageHint => (),
            va::VASurfaceAttribMemoryType => return Err(VA_STATUS_ERROR_UNSUPPORTED_MEMORY_TYPE as VAStatus),
            _ => return Err(VA_STATUS_ERROR_ATTR_NOT_SUPPORTED as VAStatus),
        }
    }
    for i in 0..n as usize {
        let id = d.id()?;
        d.surfaces.insert(id, Surface {
            width: w, height: h, fourcc, owner: None, worker: None, extent: (w, h),
            decode: None, copy: None, output: None, output_dirty: false, input: None,
        });
        surfaces.add(i).write(id);
    }
    Ok(())
});
unsafe extern "C" fn create_surfaces(
    ctx: VADriverContextP,
    w: i32,
    h: i32,
    format: i32,
    n: i32,
    surfaces: *mut u32,
) -> VAStatus {
    if w < 0 || h < 0 || n < 0 {
        return INVALID;
    }
    create_surfaces2(
        ctx,
        format as u32,
        w as u32,
        h as u32,
        surfaces,
        n as u32,
        ptr::null_mut(),
        0,
    )
}
callback!(destroy_surfaces(ctx, ids: *mut u32, n: i32) |d| {
    let ids = array(ids, count(n)?)?; let mut seen = HashSet::new();
    for id in ids { d.surface(*id)?; if !seen.insert(*id) { return Err(INVALID); }
        if busy(d, *id) { return Err(VA_STATUS_ERROR_SURFACE_BUSY as VAStatus); }
    }
    for id in ids { let s = d.surfaces.remove(id).unwrap(); if let Some(worker) = s.worker { worker.tx.send(Command::Release(*id)).map_err(|_| FAILED)?; } if s.output.is_some() { d.output_worker.as_ref().ok_or(FAILED)?.tx.send(OutputCommand::Release(*id)).map_err(|_| FAILED)?; } for c in d.contexts.values_mut() { c.targets.remove(id); } }
    Ok(())
});
callback!(create_context(ctx, config: u32, w: i32, h: i32, flags: i32, targets: *mut u32, n: i32, id: *mut u32) |d| {
    if let Some(&config) = d.enc_configs.get(&config) { return encode::create_context(d, config, w, h, targets, n, id); }
    if !d.configs.contains_key(&config) { return Err(VA_STATUS_ERROR_INVALID_CONFIG as VAStatus); }
    if id.is_null() || w <= 0 || h <= 0 { return Err(INVALID); } dimensions(w as u32, h as u32, MAX_DIM)?;
    if flags & !(VA_PROGRESSIVE as i32) != 0 { return Err(VA_STATUS_ERROR_FLAG_NOT_SUPPORTED as VAStatus); }
    let targets: HashSet<u32> = array(targets, count(n)?)?.iter().copied().collect();
    for target in &targets { let s = d.surface(*target)?; if s.width < w as u32 || s.height < h as u32 { return Err(INVALID); } }
    let codec = codec_of(d.configs[&config])?;
    let worker = Arc::new(Worker::new(codec, d.pci, w as u32, h as u32, targets.len())?); let key = d.id()?;
    d.contexts.insert(key, Context { codec, worker, width: w as u32, height: h as u32, targets, frame: None }); out(id, key)
});
callback!(destroy_context(ctx, id: u32) |d| {
    if d.enc_contexts.remove(&id).is_some() {
        d.buffers.retain(|_, b| b.context != Some(id));
        return Ok(());
    }
    d.contexts.remove(&id).ok_or(VA_STATUS_ERROR_INVALID_CONTEXT as VAStatus)?;
    d.buffers.retain(|_, b| b.context != Some(id));
    Ok(())
});
fn buffer_shape(codec: backend::Codec, kind: VABufferType, size: u32) -> bool {
    let (pic, iq, slice) = parameter_sizes(codec);
    let size = size as usize;
    match kind {
        va::VAPictureParameterBufferType => size == pic,
        va::VAIQMatrixBufferType => iq == Some(size),
        va::VASliceParameterBufferType => size == slice,
        va::VASliceDataBufferType => size != 0,
        _ => false,
    }
}
callback!(create_buffer(ctx, context: u32, kind: VABufferType, size: u32, n: u32, data: *mut c_void, id: *mut u32) |d| {
    if id.is_null() || n == 0 { return Err(INVALID); }
    if !d.enc_contexts.contains_key(&context) {
        let codec = d.context(context)?.codec;
        if !buffer_shape(codec, kind, size) { return Err(VA_STATUS_ERROR_UNSUPPORTED_BUFFERTYPE as VAStatus); }
    }
    let len = (size as usize).checked_mul(n as usize).filter(|v| *v <= isize::MAX as usize).ok_or(INVALID)?;
    let mut bytes = Vec::new(); bytes.try_reserve_exact(len).map_err(|_| VA_STATUS_ERROR_ALLOCATION_FAILED as VAStatus)?; bytes.resize(len, 0);
    if !data.is_null() { ptr::copy_nonoverlapping(data as *const u8, bytes.as_mut_ptr(), len); }
    let key = d.id()?;
    d.buffers.insert(key, Buffer {
        context: Some(context), kind, size, count: n, bytes, mapped: false,
        coded: if kind == VAEncCodedBufferType { Some(Box::default()) } else { None },
        derived: None,
    });
    out(id, key)
});
callback!(set_buffer_elements(ctx, id: u32, n: u32) |d| {
    let b = d.buffers.get_mut(&id).ok_or(VA_STATUS_ERROR_INVALID_BUFFER as VAStatus)?;
    if b.mapped || b.context.is_none() || n == 0 || (b.size as usize).checked_mul(n as usize).is_none_or(|v| v > b.bytes.len()) { return Err(INVALID); } b.count = n; Ok(())
});
callback!(map_buffer(ctx, id: u32, p: *mut *mut c_void) |d| {
    encode::map_buffer(d, id, p)
});
callback!(unmap_buffer(ctx, id: u32) |d| { encode::unmap_buffer(d, id) });
callback!(destroy_buffer(ctx, id: u32) |d| { let b = d.buffer(id)?; if b.mapped || b.context.is_none() { return Err(INVALID); } d.buffers.remove(&id); Ok(()) });
callback!(buffer_info(ctx, id: u32, kind: *mut VABufferType, size: *mut u32, n: *mut u32) |d| { let b = d.buffer(id)?; out(kind, b.kind)?; out(size, b.size)?; out(n, b.count) });
callback!(begin_picture(ctx, id: u32, target: u32) |d| {
    if d.enc_contexts.contains_key(&id) {
        d.surface(target)?;
        if busy(d, target) { return Err(VA_STATUS_ERROR_SURFACE_BUSY as VAStatus); }
        return d.enc_contexts.get_mut(&id).unwrap().begin(target);
    }
    let s = d.surface(target)?; let c = d.context(id)?;
    if c.frame.is_some() { return Err(INVALID); }
    if (!c.targets.is_empty() && !c.targets.contains(&target)) || s.width < c.width || s.height < c.height { return Err(VA_STATUS_ERROR_INVALID_SURFACE as VAStatus); }
    if s.owner.is_some_and(|owner| owner != id) { return Err(VA_STATUS_ERROR_SURFACE_BUSY as VAStatus); }
    let worker = c.worker.clone(); let extent = (c.width,c.height);
    d.contexts.get_mut(&id).unwrap().frame = Some(Frame { target, pic: None, iq: None, groups: Vec::new(), data: Vec::new(), events: Vec::new() });
    let s = d.surfaces.get_mut(&target).unwrap();
    // A surface moving to another decoder must not keep its picture alive in the previous one.
    if let Some(previous) = s.worker.replace(worker.clone()).filter(|p| !Arc::ptr_eq(p, &worker)) { let _ = previous.tx.send(Command::Release(target)); }
    s.owner = Some(id); s.extent = extent; s.decode = None; s.copy = None; s.output_dirty = true; Ok(())
});
callback!(render_picture(ctx, id: u32, ids: *mut u32, n: i32) |d| {
    if d.enc_contexts.contains_key(&id) { return encode::render_picture(d, id, ids, n); }
    if d.context(id)?.frame.is_none() { return Err(INVALID); }
    let ids = array(ids, count(n)?)?;
    for key in ids { let b = d.buffer(*key)?; if b.context != Some(id) || b.mapped { return Err(VA_STATUS_ERROR_INVALID_BUFFER as VAStatus); } }
    for key in ids {
        let b = d.buffers.get(key).unwrap(); let f = d.contexts.get_mut(&id).unwrap().frame.as_mut().unwrap();
        match b.kind {
            va::VAPictureParameterBufferType => { if f.pic.is_some() || b.count != 1 { return Err(INVALID); } f.pic = Some(b.bytes[..b.size as usize].to_vec()); }
            va::VAIQMatrixBufferType => { if f.iq.is_some() || b.count != 1 { return Err(INVALID); } f.iq = Some(b.bytes[..b.size as usize].to_vec()); }
            va::VASliceParameterBufferType => { let group = b.bytes[..b.size as usize * b.count as usize].chunks_exact(b.size as usize).map(<[u8]>::to_vec).collect(); f.events.push((true,f.groups.len())); f.groups.push(group); }
            va::VASliceDataBufferType => { f.events.push((false,f.data.len())); f.data.push(b.bytes[..b.size as usize * b.count as usize].to_vec()); }
            _ => return Err(VA_STATUS_ERROR_UNSUPPORTED_BUFFERTYPE as VAStatus),
        }
    } Ok(())
});
/// Pairs every slice parameter element with its bytes, gathered into one contiguous data buffer.
/// Every libva slice parameter struct starts with slice_data_size, slice_data_offset, slice_data_flag.
fn prepare(f: &mut Frame) -> R<backend::Picture> {
    let params = f.pic.take().ok_or(INVALID)?;
    if f.groups.is_empty() || f.data.is_empty() { return Err(INVALID); }
    // Offsets are relative to the associated data buffer. Preserve submission
    // boundaries even across RenderPicture calls and multiple parameter groups.
    let mut association = vec![usize::MAX; f.groups.len()];
    if f.data.len() == 1 { association.fill(0); }
    else if f.events.first().is_some_and(|e| e.0) {
        let mut pending = Vec::new();
        for &(parameter,index) in &f.events {
            if parameter { pending.push(index); }
            else { if pending.is_empty() { return Err(INVALID); } for group in pending.drain(..) { association[group] = index; } }
        }
        if !pending.is_empty() { return Err(INVALID); }
    } else {
        let mut current = None; let mut used = HashSet::new();
        for &(parameter,index) in &f.events {
            if parameter { let data = current.ok_or(INVALID)?; association[index] = data; used.insert(data); }
            else { current = Some(index); }
        }
        if used.len() != f.data.len() { return Err(INVALID); }
    }
    let field = |p: &[u8], i: usize| u32::from_ne_bytes(p[i * 4..i * 4 + 4].try_into().unwrap()) as usize;
    let mut slices = Vec::new(); let mut data = Vec::new();
    for (i, group) in f.groups.drain(..).enumerate() {
        let source = &f.data[association[i]];
        for param in group {
            let (size, offset, flag) = (field(&param, 0), field(&param, 1), field(&param, 2));
            if flag != VA_SLICE_DATA_FLAG_ALL as usize || size == 0 { return Err(INVALID); }
            let bytes = source.get(offset..offset.checked_add(size).ok_or(INVALID)?).ok_or(INVALID)?;
            let start = data.len(); data.extend_from_slice(bytes);
            slices.push(backend::Slice { param, data: start..data.len() });
        }
    }
    f.data.clear();
    Ok(backend::Picture { target: f.target, params, iq: f.iq.take(), slices, data })
}
// Queues the decode and returns; its outcome is reported by vaSyncSurface.
callback!(end_picture(ctx, id: u32) |d| {
    if d.enc_contexts.contains_key(&id) {
        let c = &d.enc_contexts[&id];
        let target = c.target.ok_or(INVALID)?;
        let coded = c.coded.ok_or(INVALID)?;
        let s = d.surface(target)?;
        let (w, h) = s.extent;
        // Decode surfaces currently cross the NVDEC/NVENC layouts through CPU readback.
        if let Some(done) = s.decode.as_ref() {
            done.wait()?;
            let bytes = s.worker.as_ref().ok_or(INVALID)?.readback(target)?;
            d.surfaces.get_mut(&target).unwrap().input = Some(encode::Input {
                bytes, fourcc: NV12, pitch: w as usize,
                offsets: [0, (w * h) as usize, 0],
            });
        }
        let c = d.enc_contexts.get_mut(&id).unwrap();
        // VA sequence dimensions may be padded to coding-block boundaries; uploads
        // retain the visible extent. Never ask RM to read beyond an input plane.
        c.cfg.width = c.cfg.width.min(w as i32);
        c.cfg.height = c.cfg.height.min(h as i32);
        let input = d.surfaces.get_mut(&target).unwrap().input.take().ok_or(INVALID)?;
        let output = std::mem::take(&mut d.buffers.get_mut(&coded).ok_or(INVALID)?.bytes);
        let (input, output) = c.encode(input, output)?;
        d.surfaces.get_mut(&target).unwrap().input = Some(input);
        d.buffers.get_mut(&coded).unwrap().bytes = output;
        return Ok(());
    }
    let c = d.contexts.get_mut(&id).ok_or(VA_STATUS_ERROR_INVALID_CONTEXT as VAStatus)?;
    let mut f = c.frame.take().ok_or(INVALID)?;
    let (worker, target) = (c.worker.clone(), f.target);
    let (decode, status) = match prepare(&mut f) {
        Ok(picture) => (worker.queue(move |done| Command::Decode(picture, done)), Ok(())),
        Err(e) => (Completion::done(Err(e)), Err(e)),
    };
    let s = d.surfaces.get_mut(&target).ok_or(VA_STATUS_ERROR_INVALID_SURFACE as VAStatus)?;
    s.decode = Some(decode); s.copy = None;
    status?;
    d.update_output(target)
});
fn busy(d: &Driver, id: u32) -> bool {
    d.contexts.values().any(|c| c.frame.as_ref().is_some_and(|f| f.target == id))
        || d.enc_contexts.values().any(|c| c.target == Some(id))
}
callback_unlocked!(sync_surface(ctx, id: u32) |m| {
    let pending = { let d = lock(m)?; let s = d.surface(id)?; if busy(&d, id) { return Err(VA_STATUS_ERROR_SURFACE_BUSY as VAStatus); } s.pending() };
    for work in pending { work.wait()?; }
    Ok(())
});
unsafe extern "C" fn sync_surface2(ctx: VADriverContextP, id: u32, _timeout: u64) -> VAStatus {
    sync_surface(ctx, id)
}
callback!(query_status(ctx, id: u32, status: *mut VASurfaceStatus) |d| {
    let s = d.surface(id)?;
    let rendering = busy(d, id) || s.pending().iter().any(|work| work.poll().is_none());
    out(status, if rendering { VASurfaceRendering } else { VASurfaceReady })
});
callback!(query_surface_error(ctx, id: u32, _status: VAStatus, _info: *mut *mut c_void) |d| { d.surface(id)?; Err(UNIMPL) });
fn image_format() -> VAImageFormat {
    VAImageFormat {
        fourcc: NV12,
        byte_order: VA_LSB_FIRST,
        bits_per_pixel: 12,
        ..Default::default()
    }
}
fn new_input(fourcc: u32, w: u32, h: u32) -> encode::Input {
    let wide = matches!(fourcc, P010 | Q416);
    let full = matches!(fourcc, YUV444P | Q416);
    let pitch = w as usize * if wide { 2 } else { 1 };
    let l = pitch * h as usize;
    let size = if full { l * 3 } else { l * 3 / 2 };
    encode::Input {
        bytes: vec![0; size],
        fourcc,
        pitch,
        offsets: [0, l, if full { l * 2 } else { 0 }],
    }
}
fn input_image(d: &mut Driver, input: &encode::Input, w: u32, h: u32) -> R<VAImage> {
    let mut image = allocate_image(d, w, h)?;
    image.format.fourcc = input.fourcc;
    image.format.bits_per_pixel = match input.fourcc {
        P010 | YUV444P => 24,
        Q416 => 48,
        _ => 12,
    };
    image.num_planes = if matches!(input.fourcc, YUV444P | Q416) {
        3
    } else {
        2
    };
    image.pitches = [
        input.pitch as u32,
        input.pitch as u32,
        if image.num_planes == 3 {
            input.pitch as u32
        } else {
            0
        },
    ];
    image.offsets = input.offsets.map(|v| v as u32);
    image.data_size = input.bytes.len() as u32;
    d.images.insert(image.image_id, image);
    Ok(image)
}
fn allocate_image(d: &mut Driver, w: u32, h: u32) -> R<VAImage> {
    dimensions(w, h, ENC_MAX_DIM)?;
    let size = w * h * 3 / 2;
    let id = d.id()?;
    let buffer = d.id()?;
    let image = VAImage {
        image_id: id,
        format: image_format(),
        buf: buffer,
        width: w as u16,
        height: h as u16,
        data_size: size,
        num_planes: 2,
        pitches: [w, w, 0],
        offsets: [0, w * h, 0],
        ..Default::default()
    };
    d.buffers.insert(
        buffer,
        Buffer {
            context: None,
            kind: VAImageBufferType,
            size,
            count: 1,
            bytes: Vec::new(),
            mapped: false,
            coded: None,
            derived: None,
        },
    );
    d.images.insert(id, image);
    Ok(image)
}
callback!(query_images(ctx, formats: *mut VAImageFormat, n: *mut i32) |_d| { encode::query_images(formats, n) });
callback!(create_image(ctx, format: *mut VAImageFormat, w: i32, h: i32, image: *mut VAImage) |d| {
    encode::create_image(d, format, w, h, image)
});
callback_unlocked!(derive_image(ctx, surface: u32, image: *mut VAImage) |m| {
    if image.is_null() { return Err(INVALID); }
    if let Some(value) = encode::derive_input_image(&mut *lock(m)?, surface)? { return out(image, value); }
    let (w,h) = lock(m)?.surface(surface)?.extent; let bytes = readback(m, surface)?;
    if bytes.len() != (w*h*3/2) as usize { return Err(FAILED); }
    let mut d = lock(m)?; let value = allocate_image(&mut d,w,h)?;
    d.buffers.get_mut(&value.buf).unwrap().bytes = bytes; out(image,value)
});
callback!(destroy_image(ctx, id: u32) |d| {
    let image = d.images.get(&id).ok_or(VA_STATUS_ERROR_INVALID_IMAGE as VAStatus)?; if d.buffer(image.buf)?.mapped { return Err(INVALID); }
    let image = d.images.remove(&id).unwrap(); d.buffers.remove(&image.buf); Ok(())
});
callback_unlocked!(get_image(ctx, surface: u32, x: i32, y: i32, w: u32, h: u32, id: u32) |m| {
    let (image, (cw,ch)) = { let d = lock(m)?; (*d.images.get(&id).ok_or(VA_STATUS_ERROR_INVALID_IMAGE as VAStatus)?, d.surface(surface)?.extent) };
    if x < 0 || y < 0 || x % 2 != 0 || y % 2 != 0 || w % 2 != 0 || h % 2 != 0 || w == 0 || h == 0 || image.width as u32 != w || image.height as u32 != h || (x as u32).checked_add(w).is_none_or(|v| v > cw) || (y as u32).checked_add(h).is_none_or(|v| v > ch) { return Err(INVALID); }
    let (sw,sh) = (cw as usize,ch as usize);
    let bytes = readback(m, surface)?; if bytes.len() != sw*sh*3/2 { return Err(FAILED); }
    let mut d = lock(m)?;
    let b = d.buffers.get_mut(&image.buf).ok_or(VA_STATUS_ERROR_INVALID_IMAGE as VAStatus)?; if b.mapped { return Err(INVALID); }
    for row in 0..h as usize { let start = (y as usize+row)*sw+x as usize; let dest = row*w as usize; b.bytes[dest..dest+w as usize].copy_from_slice(&bytes[start..start+w as usize]); }
    for row in 0..h as usize/2 { let start = sw*sh+(y as usize/2+row)*sw+x as usize; let dest = (w*h) as usize+row*w as usize; b.bytes[dest..dest+w as usize].copy_from_slice(&bytes[start..start+w as usize]); } Ok(())
});
fn surface_attr(kind: VASurfaceAttribType, value: u32, settable: bool) -> VASurfaceAttrib {
    let mut attr: VASurfaceAttrib = Default::default();
    attr.type_ = kind;
    attr.flags = VA_SURFACE_ATTRIB_GETTABLE
        | if settable {
            VA_SURFACE_ATTRIB_SETTABLE
        } else {
            0
        };
    attr.value.type_ = VAGenericValueTypeInteger;
    attr.value.value.i = value as i32;
    attr
}
callback!(query_surface_attrs(ctx, config: u32, attrs: *mut VASurfaceAttrib, n: *mut u32) |d| {
    encode::query_surface_attributes(d, config, attrs, n)
});
callback!(export_surface(ctx, id: u32, mem_type: u32, flags: u32, descriptor: *mut c_void) |d| {
    if mem_type != VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2 { return Err(VA_STATUS_ERROR_UNSUPPORTED_MEMORY_TYPE as VAStatus); }
    if flags & !(VA_EXPORT_SURFACE_READ_ONLY | VA_EXPORT_SURFACE_SEPARATE_LAYERS | VA_EXPORT_SURFACE_COMPOSED_LAYERS) != 0 || flags & VA_EXPORT_SURFACE_COMPOSED_LAYERS != 0 && flags & VA_EXPORT_SURFACE_SEPARATE_LAYERS != 0 { return Err(VA_STATUS_ERROR_FLAG_NOT_SUPPORTED as VAStatus); }
    if descriptor.is_null() { return Err(INVALID); }
    let export = d.export_output(id)?;
    let s = d.surface(id)?;
    let mut desc: VADRMPRIMESurfaceDescriptor = zeroed(); desc.fourcc = NV12; desc.width = s.extent.0; desc.height = s.extent.1; desc.num_objects = 2;
    let [y,uv] = export.fds; let fds = [y.into_raw_fd(),uv.into_raw_fd()];
    for i in 0..2 { desc.objects[i].fd = fds[i]; desc.objects[i].size = export.sizes[i]; desc.objects[i].drm_format_modifier = export.modifier; }
    if flags & VA_EXPORT_SURFACE_COMPOSED_LAYERS != 0 {
        desc.num_layers = 1; desc.layers[0].drm_format = NV12; desc.layers[0].num_planes = 2;
        for i in 0..2 { desc.layers[0].object_index[i] = i as u32; desc.layers[0].offset[i] = export.offsets[i]; desc.layers[0].pitch[i] = export.pitches[i]; }
    } else {
        desc.num_layers = 2;
        for i in 0..2 { desc.layers[i].drm_format = if i == 0 { u32::from_le_bytes(*b"R8  ") } else { u32::from_le_bytes(*b"RG88") }; desc.layers[i].num_planes = 1; desc.layers[i].object_index[0] = i as u32; desc.layers[i].offset[0] = export.offsets[i]; desc.layers[i].pitch[0] = export.pitches[i]; }
    } out(descriptor.cast(),desc)
});
// libva requires the legacy dispatch slots even when a feature is unsupported.
// Keep these typed: incompatible function-pointer casts are undefined behavior.
macro_rules! unsupported {
    ($name:ident($($arg:ident : $ty:ty),* $(,)?)) => {
        unsafe extern "C" fn $name(_ctx: VADriverContextP, $($arg: $ty),*) -> VAStatus { $(let _ = $arg;)* UNIMPL }
    };
}
unsupported!(
    put_surface(
        surface: u32,
        draw: *mut c_void,
        sx: i16,
        sy: i16,
        sw: u16,
        sh: u16,
        dx: i16,
        dy: i16,
        dw: u16,
        dh: u16,
        clips: *mut VARectangle,
        n: u32,
        flags: u32,
    )
);
unsupported!(set_palette(image: u32, palette: *mut u8));
callback!(put_image(ctx, surface: u32, image: u32, sx: i32, sy: i32, sw: u32, sh: u32, dx: i32, dy: i32, dw: u32, dh: u32) |d| {
    encode::put_image(d, surface, image, (sx, sy, sw, sh), (dx, dy, dw, dh))
});
unsupported!(create_subpicture(image: u32, sub: *mut u32));
unsupported!(destroy_subpicture(sub: u32));
unsupported!(set_subpicture_image(sub: u32, image: u32));
unsupported!(set_subpicture_chroma(sub: u32, min: u32, max: u32, mask: u32));
unsupported!(set_subpicture_alpha(sub: u32, alpha: f32));
unsupported!(
    associate_subpicture(
        sub: u32,
        surfaces: *mut u32,
        n: i32,
        sx: i16,
        sy: i16,
        sw: u16,
        sh: u16,
        dx: i16,
        dy: i16,
        dw: u16,
        dh: u16,
        flags: u32,
    )
);
unsupported!(deassociate_subpicture(sub: u32, surfaces: *mut u32, n: i32));
unsupported!(get_display_attributes(attrs: *mut VADisplayAttribute, n: i32));
unsupported!(set_display_attributes(attrs: *mut VADisplayAttribute, n: i32));
unsupported!(
    lock_surface(
        surface: u32,
        fourcc: *mut u32,
        ys: *mut u32,
        us: *mut u32,
        vs: *mut u32,
        yo: *mut u32,
        uo: *mut u32,
        vo: *mut u32,
        name: *mut u32,
        data: *mut *mut c_void,
    )
);
unsupported!(unlock_surface(surface: u32));
unsupported!(get_surface_attributes(config: u32, attrs: *mut VASurfaceAttrib, n: u32));
unsupported!(acquire_buffer(id: u32, info: *mut VABufferInfo));
unsupported!(release_buffer(id: u32));
unsupported!(create_mf(id: *mut u32));
unsupported!(mf_add(mf: u32, context: u32));
unsupported!(mf_release(mf: u32, context: u32));
unsupported!(mf_submit(mf: u32, contexts: *mut u32, n: i32));
unsupported!(
    create_buffer2(
        context: u32,
        kind: VABufferType,
        w: u32,
        h: u32,
        size: *mut u32,
        pitch: *mut u32,
        id: *mut u32,
    )
);
unsupported!(query_processing(config: u32, param: *mut VAProcessingRateParameter, rate: *mut u32));
callback!(sync_buffer(ctx, id: u32, _timeout: u64) |d| { d.buffer(id)?; Ok(()) });
unsupported!(copy_object(dst: *mut VACopyObject, src: *mut VACopyObject, option: VACopyOption));
callback!(query_subpictures(ctx, _formats:*mut VAImageFormat, _flags:*mut u32, n:*mut u32) |_d| { out(n,0) });
callback!(query_display_attributes(ctx, _attrs:*mut VADisplayAttribute, n:*mut i32) |_d| { out(n,0) });
unsafe extern "C" fn map_buffer2(
    ctx: VADriverContextP,
    id: u32,
    data: *mut *mut c_void,
    flags: u32,
) -> VAStatus {
    if flags & !(VA_MAPBUFFER_FLAG_READ | VA_MAPBUFFER_FLAG_WRITE) != 0 {
        return INVALID;
    }
    map_buffer(ctx, id, data)
}

unsafe extern "C" fn terminate(ctx: VADriverContextP) -> VAStatus {
    if ctx.is_null() || (*ctx).pDriverData.is_null() {
        return VA_STATUS_ERROR_INVALID_DISPLAY as VAStatus;
    }
    let data = (*ctx).pDriverData;
    (*ctx).pDriverData = ptr::null_mut();
    match catch_unwind(AssertUnwindSafe(|| {
        drop(Box::from_raw(data as *mut Mutex<Driver>))
    })) {
        Ok(()) => 0,
        Err(_) => FAILED,
    }
}

/// PCI address of the NVIDIA GPU behind the DRM device libva opened for this display.
/// Decoding on any other GPU would hand the application buffers its device cannot import.
/// Uses only fstat on the fd and one sysfs file read, which sandboxed GPU processes (Chrome's
/// broker allows /sys/dev/char/<major>:<minor>/device/...) still permit.
unsafe fn drm_pci(ctx: VADriverContextP) -> Result<[u32; 4], String> {
    use std::os::{fd::BorrowedFd, unix::fs::MetadataExt};
    let state = (*ctx).drm_state as *const drm_state;
    if state.is_null() || (*state).fd < 0 {
        return Err("the VA display has no DRM device".into());
    }
    let fd = BorrowedFd::borrow_raw((*state).fd);
    let rdev = std::fs::File::from(fd.try_clone_to_owned().map_err(|e| format!("DRM fd: {e}"))?)
        .metadata()
        .map_err(|e| format!("DRM fd: {e}"))?
        .rdev();
    let major = ((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfff);
    let minor = (rdev & 0xff) | ((rdev >> 12) & !0xff);
    let path = format!("/sys/dev/char/{major}:{minor}/device/uevent");
    let uevent = std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?;
    let value = |key: &str| uevent.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix('='));
    let driver = value("DRIVER").unwrap_or_default();
    if driver != "nvidia" {
        return Err(format!("DRM device {major}:{minor} is driven by {driver:?}, not nvidia"));
    }
    let bdf = value("PCI_SLOT_NAME").ok_or_else(|| format!("{path}: no PCI_SLOT_NAME"))?;
    let fields: Vec<u32> = bdf
        .split([':', '.'])
        .map(|f| u32::from_str_radix(f, 16))
        .collect::<Result<_, _>>()
        .map_err(|_| format!("unexpected PCI address {bdf:?}"))?;
    fields.try_into().map_err(|_| format!("unexpected PCI address {bdf:?}"))
}

#[no_mangle]
pub unsafe extern "C" fn __vaDriverInit_1_0(ctx: VADriverContextP) -> VAStatus {
    if ctx.is_null() || (*ctx).vtable.is_null() || !(*ctx).pDriverData.is_null() {
        return INVALID;
    }
    let pci = match drm_pci(ctx) {
        Ok(pci) => pci,
        Err(e) => {
            eprintln!("rustnv: {e}");
            return VA_STATUS_ERROR_INVALID_DISPLAY as VAStatus;
        }
    };
    match catch_unwind(AssertUnwindSafe(|| {
        let mut table: VADriverVTable = zeroed();
        table.vaTerminate = Some(terminate);
        table.vaQueryConfigProfiles = Some(query_profiles);
        table.vaQueryConfigEntrypoints = Some(query_entrypoints);
        table.vaGetConfigAttributes = Some(get_config_attributes);
        table.vaCreateConfig = Some(create_config);
        table.vaDestroyConfig = Some(destroy_config);
        table.vaQueryConfigAttributes = Some(query_config);
        table.vaCreateSurfaces = Some(create_surfaces);
        table.vaCreateSurfaces2 = Some(create_surfaces2);
        table.vaDestroySurfaces = Some(destroy_surfaces);
        table.vaCreateContext = Some(create_context);
        table.vaDestroyContext = Some(destroy_context);
        table.vaCreateBuffer = Some(create_buffer);
        table.vaBufferSetNumElements = Some(set_buffer_elements);
        table.vaMapBuffer = Some(map_buffer);
        table.vaUnmapBuffer = Some(unmap_buffer);
        table.vaDestroyBuffer = Some(destroy_buffer);
        table.vaBufferInfo = Some(buffer_info);
        table.vaBeginPicture = Some(begin_picture);
        table.vaRenderPicture = Some(render_picture);
        table.vaEndPicture = Some(end_picture);
        table.vaSyncSurface = Some(sync_surface);
        table.vaSyncSurface2 = Some(sync_surface2);
        table.vaQuerySurfaceStatus = Some(query_status);
        table.vaQuerySurfaceError = Some(query_surface_error);
        table.vaQueryImageFormats = Some(query_images);
        table.vaCreateImage = Some(create_image);
        table.vaDeriveImage = Some(derive_image);
        table.vaDestroyImage = Some(destroy_image);
        table.vaGetImage = Some(get_image);
        table.vaQuerySurfaceAttributes = Some(query_surface_attrs);
        table.vaExportSurfaceHandle = Some(export_surface);
        table.vaPutSurface = Some(put_surface);
        table.vaSetImagePalette = Some(set_palette);
        table.vaPutImage = Some(put_image);
        table.vaQuerySubpictureFormats = Some(query_subpictures);
        table.vaCreateSubpicture = Some(create_subpicture);
        table.vaDestroySubpicture = Some(destroy_subpicture);
        table.vaSetSubpictureImage = Some(set_subpicture_image);
        table.vaSetSubpictureChromakey = Some(set_subpicture_chroma);
        table.vaSetSubpictureGlobalAlpha = Some(set_subpicture_alpha);
        table.vaAssociateSubpicture = Some(associate_subpicture);
        table.vaDeassociateSubpicture = Some(deassociate_subpicture);
        table.vaQueryDisplayAttributes = Some(query_display_attributes);
        table.vaGetDisplayAttributes = Some(get_display_attributes);
        table.vaSetDisplayAttributes = Some(set_display_attributes);
        table.vaLockSurface = Some(lock_surface);
        table.vaUnlockSurface = Some(unlock_surface);
        table.vaGetSurfaceAttributes = Some(get_surface_attributes);
        table.vaAcquireBufferHandle = Some(acquire_buffer);
        table.vaReleaseBufferHandle = Some(release_buffer);
        table.vaCreateMFContext = Some(create_mf);
        table.vaMFAddContext = Some(mf_add);
        table.vaMFReleaseContext = Some(mf_release);
        table.vaMFSubmit = Some(mf_submit);
        table.vaCreateBuffer2 = Some(create_buffer2);
        table.vaQueryProcessingRate = Some(query_processing);
        table.vaSyncBuffer = Some(sync_buffer);
        table.vaCopy = Some(copy_object);
        table.vaMapBuffer2 = Some(map_buffer2);
        (*ctx).version_major = VA_MAJOR_VERSION as i32;
        (*ctx).version_minor = VA_MINOR_VERSION as i32;
        (*ctx).max_profiles = (PROFILES.len() + encode::PROFILES.len()) as i32;
        (*ctx).max_entrypoints = 2;
        (*ctx).max_attributes = 64;
        (*ctx).max_image_formats = 4;
        (*ctx).max_subpic_formats = 1;
        (*ctx).max_display_attributes = 1;
        (*ctx).str_vendor = b"Rust NVIDIA RM NVDEC/NVENC driver\0".as_ptr().cast();
        (*ctx).pDriverData = Box::into_raw(Box::new(Mutex::new(Driver::new(pci)))).cast();
        (*ctx).vtable.write(table);
    })) {
        Ok(()) => 0,
        Err(_) => FAILED,
    }
}
