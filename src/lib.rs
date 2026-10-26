mod backend;
pub mod va;

use std::{
    collections::{HashMap, HashSet},
    ffi::c_void,
    mem::{size_of, zeroed},
    os::fd::IntoRawFd,
    panic::{catch_unwind, AssertUnwindSafe},
    ptr, slice,
    sync::{mpsc, Arc, Mutex},
    thread,
};
use va::*;
type R<T = ()> = Result<T, VAStatus>;
const INVALID: VAStatus = VA_STATUS_ERROR_INVALID_PARAMETER as VAStatus;
const FAILED: VAStatus = VA_STATUS_ERROR_OPERATION_FAILED as VAStatus;
const UNIMPL: VAStatus = VA_STATUS_ERROR_UNIMPLEMENTED as VAStatus;
const NV12: u32 = u32::from_le_bytes(*b"NV12");
const MAX_DIM: u32 = 4096;
const PROFILES: [VAProfile; 4] = [
    VAProfileH264Baseline,
    VAProfileH264ConstrainedBaseline,
    VAProfileH264Main,
    VAProfileH264High,
];

struct Frame {
    target: u32,
    pic: Option<VAPictureParameterBufferH264>,
    iq: Option<VAIQMatrixBufferH264>,
    groups: Vec<Vec<VASliceParameterBufferH264>>,
    data: Vec<Vec<u8>>,
    events: Vec<(bool, usize)>, // true: parameter group, false: data buffer
}
enum Command {
    Decode(
        Frame,
        Vec<VASliceParameterBufferH264>,
        Vec<u8>,
        mpsc::Sender<Result<(), String>>,
    ),
    Copy(u32, backend::Export, mpsc::Sender<Result<(), String>>),
    Readback(u32, mpsc::Sender<Result<Vec<u8>, String>>),
    Release(u32),
    Quit,
}
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
    fn new(w: u32, h: u32) -> R<Self> {
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let join = thread::Builder::new()
            .name("rustnv-nvdec".into())
            .spawn(move || {
                let mut decoder = match backend::Decoder::new(w, h) {
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
                        Command::Decode(f, slices, data, reply) => {
                            let _ = reply.send(decoder.decode(
                                f.target,
                                f.pic.as_ref().unwrap(),
                                f.iq.as_ref().unwrap(),
                                &slices,
                                &data,
                            ));
                        }
                        Command::Copy(id, output, reply) => {
                            let _ = reply.send(decoder.copy_into(id, &output));
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
    fn readback(&self, id: u32) -> R<Vec<u8>> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Command::Readback(id, tx))
            .map_err(|_| FAILED)?;
        gpu(rx.recv().map_err(|_| FAILED)?)
    }
    fn copy_into(&self, id: u32, output: backend::Export) -> R {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Command::Copy(id, output, tx))
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
    fn new() -> R<Self> {
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let join = thread::Builder::new()
            .name("rustnv-output".into())
            .spawn(move || {
                let mut pool = match backend::OutputPool::new() {
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
    owner: Option<u32>,
    worker: Option<Arc<Worker>>,
    extent: (u32, u32),
    decoded: bool,
    error: VAStatus,
    output: Option<backend::Export>,
    output_dirty: bool,
}
struct Buffer {
    context: Option<u32>,
    kind: VABufferType,
    size: u32,
    count: u32,
    bytes: Vec<u8>,
    mapped: bool,
}
struct Driver {
    next: u32,
    configs: HashMap<u32, VAProfile>,
    surfaces: HashMap<u32, Surface>,
    contexts: HashMap<u32, Context>,
    buffers: HashMap<u32, Buffer>,
    images: HashMap<u32, VAImage>,
    output_worker: Option<OutputWorker>,
}
impl Driver {
    fn new() -> Self {
        Self {
            next: 1,
            configs: HashMap::new(),
            surfaces: HashMap::new(),
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
    fn readback(&self, id: u32) -> R<Vec<u8>> {
        let s = self.surface(id)?;
        if !s.decoded {
            return Err(if s.error != 0 {
                s.error
            } else {
                VA_STATUS_ERROR_INVALID_SURFACE as VAStatus
            });
        }
        s.worker.as_ref().ok_or(FAILED)?.readback(id)
    }
    fn update_output(&mut self, id: u32) -> R {
        let s = self.surface(id)?;
        if !s.decoded || !s.output_dirty {
            return Ok(());
        }
        let Some(output) = s.output.as_ref() else {
            return Ok(());
        };
        let output = gpu(output.try_clone())?;
        let worker = s.worker.as_ref().ok_or(FAILED)?.clone();
        worker.copy_into(id, output)?;
        self.surfaces.get_mut(&id).unwrap().output_dirty = false;
        Ok(())
    }
    fn export_output(&mut self, id: u32) -> R<backend::Export> {
        let s = self.surface(id)?;
        if s.error != 0 {
            return Err(s.error);
        }
        if s.output.is_none() {
            let (w, h) = (s.width, s.height);
            if self.output_worker.is_none() {
                self.output_worker = Some(OutputWorker::new()?);
            }
            let output = self.output_worker.as_ref().unwrap().export(id, w, h)?;
            let s = self.surfaces.get_mut(&id).unwrap();
            s.output = Some(output);
            s.output_dirty = s.decoded;
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
fn dimensions(w: u32, h: u32) -> R {
    if w == 0 || h == 0 || w > MAX_DIM || h > MAX_DIM || w % 2 != 0 || h % 2 != 0 {
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
    if ctx.is_null() || (*ctx).pDriverData.is_null() {
        return VA_STATUS_ERROR_INVALID_DISPLAY as VAStatus;
    }
    match catch_unwind(AssertUnwindSafe(|| {
        let m = &*((*ctx).pDriverData as *const Mutex<Driver>);
        let mut d = m.lock().map_err(|_| FAILED)?;
        f(&mut d)
    })) {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => e,
        Err(_) => FAILED,
    }
}
macro_rules! callback {
    ($name:ident($ctx:ident $(, $arg:ident : $ty:ty)*) |$d:ident| $body:block) => {
        unsafe extern "C" fn $name($ctx: VADriverContextP, $($arg: $ty),*) -> VAStatus { invoke($ctx, |$d| $body) }
    };
}
callback!(query_profiles(ctx, list: *mut VAProfile, n: *mut i32) |_d| {
    if list.is_null() { return Err(INVALID); } ptr::copy_nonoverlapping(PROFILES.as_ptr(), list, PROFILES.len()); out(n, PROFILES.len() as i32)
});
callback!(query_entrypoints(ctx, p: VAProfile, list: *mut VAEntrypoint, n: *mut i32) |_d| {
    supported(p, VAEntrypointVLD)?; out(list, VAEntrypointVLD)?; out(n, 1)
});
callback!(get_config_attributes(ctx, p: VAProfile, e: VAEntrypoint, attrs: *mut VAConfigAttrib, n: i32) |_d| {
    supported(p, e)?; for i in 0..count(n)? { let a = array(attrs, count(n)?)?[i]; (*attrs.add(i)).value = attribute(a.type_); } Ok(())
});
callback!(create_config(ctx, p: VAProfile, e: VAEntrypoint, attrs: *mut VAConfigAttrib, n: i32, id: *mut u32) |d| {
    supported(p, e)?; if id.is_null() { return Err(INVALID); }
    for a in array(attrs, count(n)?)? {
        let value = attribute(a.type_);
        if value == VA_ATTRIB_NOT_SUPPORTED || a.value != value { return Err(VA_STATUS_ERROR_ATTR_NOT_SUPPORTED as VAStatus); }
    }
    let key = d.id()?; d.configs.insert(key, p); out(id, key)
});
callback!(destroy_config(ctx, id: u32) |d| { d.configs.remove(&id).ok_or(VA_STATUS_ERROR_INVALID_CONFIG as VAStatus)?; Ok(()) });
callback!(query_config(ctx, id: u32, p: *mut VAProfile, e: *mut VAEntrypoint, attrs: *mut VAConfigAttrib, n: *mut i32) |d| {
    let profile = *d.configs.get(&id).ok_or(VA_STATUS_ERROR_INVALID_CONFIG as VAStatus)?;
    if p.is_null() || e.is_null() || attrs.is_null() || n.is_null() { return Err(INVALID); }
    out(p, profile)?; out(e, VAEntrypointVLD)?;
    out(attrs, VAConfigAttrib { type_: VAConfigAttribRTFormat, value: VA_RT_FORMAT_YUV420 })?; out(n, 1)
});
callback!(create_surfaces2(ctx, format: u32, w: u32, h: u32, surfaces: *mut u32, n: u32, attrs: *mut VASurfaceAttrib, na: u32) |d| {
    dimensions(w, h)?;
    if format != VA_RT_FORMAT_YUV420 { return Err(VA_STATUS_ERROR_UNSUPPORTED_RT_FORMAT as VAStatus); }
    if n == 0 || surfaces.is_null() { return Err(INVALID); }
    for a in array(attrs, na as usize)? {
        if a.flags & VA_SURFACE_ATTRIB_SETTABLE == 0 { continue; }
        if a.value.type_ != VAGenericValueTypeInteger { return Err(INVALID); }
        let v = a.value.value.i as u32;
        match a.type_ {
            va::VASurfaceAttribPixelFormat if v == NV12 => (),
            va::VASurfaceAttribMemoryType if v == VA_SURFACE_ATTRIB_MEM_TYPE_VA => (),
            va::VASurfaceAttribUsageHint => (),
            va::VASurfaceAttribMemoryType => return Err(VA_STATUS_ERROR_UNSUPPORTED_MEMORY_TYPE as VAStatus),
            _ => return Err(VA_STATUS_ERROR_ATTR_NOT_SUPPORTED as VAStatus),
        }
    }
    for i in 0..n as usize { let id = d.id()?; d.surfaces.insert(id, Surface { width: w, height: h, owner: None, worker: None, extent: (w,h), decoded: false, error: 0, output: None, output_dirty: false }); surfaces.add(i).write(id); } Ok(())
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
        if d.contexts.values().any(|c| c.frame.as_ref().is_some_and(|f| f.target == *id)) { return Err(VA_STATUS_ERROR_SURFACE_BUSY as VAStatus); }
    }
    for id in ids { let s = d.surfaces.remove(id).unwrap(); if let Some(worker) = s.worker { worker.tx.send(Command::Release(*id)).map_err(|_| FAILED)?; } if s.output.is_some() { d.output_worker.as_ref().ok_or(FAILED)?.tx.send(OutputCommand::Release(*id)).map_err(|_| FAILED)?; } for c in d.contexts.values_mut() { c.targets.remove(id); } }
    Ok(())
});
callback!(create_context(ctx, config: u32, w: i32, h: i32, flags: i32, targets: *mut u32, n: i32, id: *mut u32) |d| {
    if !d.configs.contains_key(&config) { return Err(VA_STATUS_ERROR_INVALID_CONFIG as VAStatus); }
    if id.is_null() || w <= 0 || h <= 0 { return Err(INVALID); } dimensions(w as u32, h as u32)?;
    if flags & !(VA_PROGRESSIVE as i32) != 0 { return Err(VA_STATUS_ERROR_FLAG_NOT_SUPPORTED as VAStatus); }
    let targets: HashSet<u32> = array(targets, count(n)?)?.iter().copied().collect();
    for target in &targets { let s = d.surface(*target)?; if s.width < w as u32 || s.height < h as u32 { return Err(INVALID); } }
    let worker = Arc::new(Worker::new(w as u32, h as u32)?); let key = d.id()?;
    d.contexts.insert(key, Context { worker, width: w as u32, height: h as u32, targets, frame: None }); out(id, key)
});
callback!(destroy_context(ctx, id: u32) |d| {
    d.contexts.remove(&id).ok_or(VA_STATUS_ERROR_INVALID_CONTEXT as VAStatus)?;
    d.buffers.retain(|_, b| b.context != Some(id));
    Ok(())
});
fn buffer_shape(kind: VABufferType, size: u32) -> bool {
    match kind {
        va::VAPictureParameterBufferType => {
            size as usize == size_of::<VAPictureParameterBufferH264>()
        }
        va::VAIQMatrixBufferType => size as usize == size_of::<VAIQMatrixBufferH264>(),
        va::VASliceParameterBufferType => size as usize == size_of::<VASliceParameterBufferH264>(),
        va::VASliceDataBufferType => size != 0,
        _ => false,
    }
}
callback!(create_buffer(ctx, context: u32, kind: VABufferType, size: u32, n: u32, data: *mut c_void, id: *mut u32) |d| {
    d.context(context)?; if id.is_null() || n == 0 { return Err(INVALID); }
    if !buffer_shape(kind, size) { return Err(VA_STATUS_ERROR_UNSUPPORTED_BUFFERTYPE as VAStatus); }
    let len = (size as usize).checked_mul(n as usize).filter(|v| *v <= isize::MAX as usize).ok_or(INVALID)?;
    let mut bytes = Vec::new(); bytes.try_reserve_exact(len).map_err(|_| VA_STATUS_ERROR_ALLOCATION_FAILED as VAStatus)?; bytes.resize(len, 0);
    if !data.is_null() { ptr::copy_nonoverlapping(data as *const u8, bytes.as_mut_ptr(), len); }
    let key = d.id()?; d.buffers.insert(key, Buffer { context: Some(context), kind, size, count: n, bytes, mapped: false }); out(id, key)
});
callback!(set_buffer_elements(ctx, id: u32, n: u32) |d| {
    let b = d.buffers.get_mut(&id).ok_or(VA_STATUS_ERROR_INVALID_BUFFER as VAStatus)?;
    if b.mapped || b.context.is_none() || n == 0 || (b.size as usize).checked_mul(n as usize).is_none_or(|v| v > b.bytes.len()) { return Err(INVALID); } b.count = n; Ok(())
});
callback!(map_buffer(ctx, id: u32, p: *mut *mut c_void) |d| {
    let b = d.buffers.get_mut(&id).ok_or(VA_STATUS_ERROR_INVALID_BUFFER as VAStatus)?;
    if b.mapped { return Err(INVALID); } out(p, b.bytes.as_mut_ptr().cast())?; b.mapped = true; Ok(())
});
callback!(unmap_buffer(ctx, id: u32) |d| { let b = d.buffers.get_mut(&id).ok_or(VA_STATUS_ERROR_INVALID_BUFFER as VAStatus)?; if !b.mapped { return Err(INVALID); } b.mapped = false; Ok(()) });
callback!(destroy_buffer(ctx, id: u32) |d| { let b = d.buffer(id)?; if b.mapped || b.context.is_none() { return Err(INVALID); } d.buffers.remove(&id); Ok(()) });
callback!(buffer_info(ctx, id: u32, kind: *mut VABufferType, size: *mut u32, n: *mut u32) |d| { let b = d.buffer(id)?; out(kind, b.kind)?; out(size, b.size)?; out(n, b.count) });
callback!(begin_picture(ctx, id: u32, target: u32) |d| {
    let s = d.surface(target)?; let c = d.context(id)?;
    if c.frame.is_some() { return Err(INVALID); }
    if (!c.targets.is_empty() && !c.targets.contains(&target)) || s.width < c.width || s.height < c.height { return Err(VA_STATUS_ERROR_INVALID_SURFACE as VAStatus); }
    if s.owner.is_some_and(|owner| owner != id) { return Err(VA_STATUS_ERROR_SURFACE_BUSY as VAStatus); }
    let worker = c.worker.clone(); let extent = (c.width,c.height);
    d.contexts.get_mut(&id).unwrap().frame = Some(Frame { target, pic: None, iq: None, groups: Vec::new(), data: Vec::new(), events: Vec::new() });
    let s = d.surfaces.get_mut(&target).unwrap(); s.owner = Some(id); s.worker = Some(worker); s.extent = extent; s.decoded = false; s.error = 0; s.output_dirty = true; Ok(())
});
callback!(render_picture(ctx, id: u32, ids: *mut u32, n: i32) |d| {
    if d.context(id)?.frame.is_none() { return Err(INVALID); }
    let ids = array(ids, count(n)?)?;
    for key in ids { let b = d.buffer(*key)?; if b.context != Some(id) || b.mapped { return Err(VA_STATUS_ERROR_INVALID_BUFFER as VAStatus); } }
    for key in ids {
        let b = d.buffers.get(key).unwrap(); let f = d.contexts.get_mut(&id).unwrap().frame.as_mut().unwrap();
        match b.kind {
            va::VAPictureParameterBufferType => { if f.pic.is_some() || b.count != 1 { return Err(INVALID); } f.pic = Some(ptr::read_unaligned(b.bytes.as_ptr().cast())); }
            va::VAIQMatrixBufferType => { if f.iq.is_some() || b.count != 1 { return Err(INVALID); } f.iq = Some(ptr::read_unaligned(b.bytes.as_ptr().cast())); }
            va::VASliceParameterBufferType => { let mut group = Vec::with_capacity(b.count as usize); for i in 0..b.count as usize { group.push(ptr::read_unaligned(b.bytes.as_ptr().add(i * b.size as usize).cast())); } f.events.push((true,f.groups.len())); f.groups.push(group); }
            va::VASliceDataBufferType => { f.events.push((false,f.data.len())); f.data.push(b.bytes[..b.size as usize * b.count as usize].to_vec()); }
            _ => return Err(VA_STATUS_ERROR_UNSUPPORTED_BUFFERTYPE as VAStatus),
        }
    } Ok(())
});
callback!(end_picture(ctx, id: u32) |d| {
    let c = d.contexts.get_mut(&id).ok_or(VA_STATUS_ERROR_INVALID_CONTEXT as VAStatus)?;
    let mut f = c.frame.take().ok_or(INVALID)?;
    let pic = f.pic.as_ref().ok_or(INVALID)?;
    if pic.CurrPic.picture_id != f.target || f.groups.is_empty() || f.data.is_empty() { return Err(INVALID); }
    if pic.bit_depth_luma_minus8 != 0 || pic.bit_depth_chroma_minus8 != 0 || pic.seq_fields.bits.chroma_format_idc() != 1 || pic.seq_fields.bits.frame_mbs_only_flag() == 0 || pic.pic_fields.bits.field_pic_flag() != 0 || pic.num_slice_groups_minus1 != 0 { return Err(VA_STATUS_ERROR_UNSUPPORTED_PROFILE as VAStatus); }
    if f.iq.is_none() { let mut iq: VAIQMatrixBufferH264 = zeroed(); iq.ScalingList4x4 = [[16; 16]; 6]; iq.ScalingList8x8 = [[16; 64]; 2]; f.iq = Some(iq); }
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
    let mut slices = Vec::new(); let mut data = Vec::new();
    for (i, group) in f.groups.drain(..).enumerate() {
        let source = &f.data[association[i]];
        for mut s in group {
            if s.slice_data_flag != VA_SLICE_DATA_FLAG_ALL || s.slice_data_size == 0 { return Err(INVALID); }
            let start = s.slice_data_offset as usize; let end = start.checked_add(s.slice_data_size as usize).ok_or(INVALID)?;
            let bytes = source.get(start..end).ok_or(INVALID)?;
            s.slice_data_offset = u32::try_from(data.len()).map_err(|_| INVALID)?;
            data.extend_from_slice(bytes); slices.push(s);
        }
    }
    f.data.clear(); let target = f.target;
    let (tx, rx) = mpsc::channel(); c.worker.tx.send(Command::Decode(f, slices, data, tx)).map_err(|_| FAILED)?;
    let result = gpu(rx.recv().map_err(|_| FAILED)?);
    let s = d.surfaces.get_mut(&target).ok_or(VA_STATUS_ERROR_INVALID_SURFACE as VAStatus)?;
    s.decoded = result.is_ok(); s.error = if result.is_ok() { 0 } else { VA_STATUS_ERROR_DECODING_ERROR as VAStatus };
    result.map_err(|_| VA_STATUS_ERROR_DECODING_ERROR as VAStatus)?;
    if let Err(error) = d.update_output(target) { d.surfaces.get_mut(&target).unwrap().error = error; return Err(error); }
    Ok(())
});
callback!(sync_surface(ctx, id: u32) |d| {
    let s = d.surface(id)?; if d.contexts.values().any(|c| c.frame.as_ref().is_some_and(|f| f.target == id)) { return Err(VA_STATUS_ERROR_SURFACE_BUSY as VAStatus); } if s.error != 0 { return Err(s.error); } Ok(())
});
unsafe extern "C" fn sync_surface2(ctx: VADriverContextP, id: u32, _timeout: u64) -> VAStatus {
    sync_surface(ctx, id)
}
callback!(query_status(ctx, id: u32, status: *mut VASurfaceStatus) |d| { let _s = d.surface(id)?; let busy = d.contexts.values().any(|c| c.frame.as_ref().is_some_and(|f| f.target == id)); out(status, if busy { VASurfaceRendering } else { VASurfaceReady }) });
callback!(query_surface_error(ctx, id: u32, _status: VAStatus, _info: *mut *mut c_void) |d| { d.surface(id)?; Err(UNIMPL) });
fn image_format() -> VAImageFormat {
    VAImageFormat {
        fourcc: NV12,
        byte_order: VA_LSB_FIRST,
        bits_per_pixel: 12,
        ..Default::default()
    }
}
fn allocate_image(d: &mut Driver, w: u32, h: u32) -> R<VAImage> {
    dimensions(w, h)?;
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
            bytes: vec![0; size as usize],
            mapped: false,
        },
    );
    d.images.insert(id, image);
    Ok(image)
}
callback!(query_images(ctx, formats: *mut VAImageFormat, n: *mut i32) |_d| { out(formats, image_format())?; out(n, 1) });
callback!(create_image(ctx, format: *mut VAImageFormat, w: i32, h: i32, image: *mut VAImage) |d| {
    if format.is_null() || image.is_null() || w <= 0 || h <= 0 { return Err(INVALID); } if (*format).fourcc != NV12 { return Err(VA_STATUS_ERROR_INVALID_IMAGE_FORMAT as VAStatus); } let value = allocate_image(d, w as u32, h as u32)?; out(image, value)
});
callback!(derive_image(ctx, surface: u32, image: *mut VAImage) |d| {
    if image.is_null() { return Err(INVALID); } let s = d.surface(surface)?; let (w,h) = s.extent; let bytes = d.readback(surface)?;
    if bytes.len() != (w*h*3/2) as usize { return Err(FAILED); } let value = allocate_image(d,w,h)?;
    d.buffers.get_mut(&value.buf).unwrap().bytes = bytes; out(image,value)
});
callback!(destroy_image(ctx, id: u32) |d| {
    let image = d.images.get(&id).ok_or(VA_STATUS_ERROR_INVALID_IMAGE as VAStatus)?; if d.buffer(image.buf)?.mapped { return Err(INVALID); }
    let image = d.images.remove(&id).unwrap(); d.buffers.remove(&image.buf); Ok(())
});
callback!(get_image(ctx, surface: u32, x: i32, y: i32, w: u32, h: u32, id: u32) |d| {
    let image = *d.images.get(&id).ok_or(VA_STATUS_ERROR_INVALID_IMAGE as VAStatus)?; let s = d.surface(surface)?;
    let (cw,ch) = s.extent;
    if x < 0 || y < 0 || x % 2 != 0 || y % 2 != 0 || w % 2 != 0 || h % 2 != 0 || w == 0 || h == 0 || image.width as u32 != w || image.height as u32 != h || (x as u32).checked_add(w).is_none_or(|v| v > cw) || (y as u32).checked_add(h).is_none_or(|v| v > ch) { return Err(INVALID); }
    let (sw,sh) = (cw as usize,ch as usize); let b = d.buffer(image.buf)?; if b.mapped { return Err(INVALID); }
    let bytes = d.readback(surface)?; if bytes.len() != sw*sh*3/2 { return Err(FAILED); }
    let b = d.buffers.get_mut(&image.buf).unwrap();
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
    if !d.configs.contains_key(&config) { return Err(VA_STATUS_ERROR_INVALID_CONFIG as VAStatus); } if n.is_null() { return Err(INVALID); }
    let values = [surface_attr(VASurfaceAttribPixelFormat,NV12,true),surface_attr(VASurfaceAttribMinWidth,2,false),surface_attr(VASurfaceAttribMinHeight,2,false),surface_attr(VASurfaceAttribMaxWidth,MAX_DIM,false),surface_attr(VASurfaceAttribMaxHeight,MAX_DIM,false),surface_attr(VASurfaceAttribMemoryType,VA_SURFACE_ATTRIB_MEM_TYPE_VA,true),surface_attr(VASurfaceAttribUsageHint,VA_SURFACE_ATTRIB_USAGE_HINT_DECODER,true)];
    if attrs.is_null() { return out(n,values.len() as u32); } let available = *n; out(n,values.len() as u32)?; if available < values.len() as u32 { return Err(VA_STATUS_ERROR_MAX_NUM_EXCEEDED as VAStatus); } ptr::copy_nonoverlapping(values.as_ptr(),attrs,values.len()); Ok(())
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
unsupported!(
    put_image(
        surface: u32,
        image: u32,
        sx: i32,
        sy: i32,
        sw: u32,
        sh: u32,
        dx: i32,
        dy: i32,
        dw: u32,
        dh: u32,
    )
);
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
unsupported!(sync_buffer(id: u32, timeout: u64));
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

#[no_mangle]
pub unsafe extern "C" fn __vaDriverInit_1_0(ctx: VADriverContextP) -> VAStatus {
    if ctx.is_null() || (*ctx).vtable.is_null() || !(*ctx).pDriverData.is_null() {
        return INVALID;
    }
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
        (*ctx).max_profiles = PROFILES.len() as i32;
        (*ctx).max_entrypoints = 1;
        (*ctx).max_attributes = 5;
        (*ctx).max_image_formats = 1;
        (*ctx).max_subpic_formats = 1;
        (*ctx).max_display_attributes = 1;
        (*ctx).str_vendor = b"Rust NVIDIA RM NVDEC H.264 driver\0".as_ptr().cast();
        (*ctx).pDriverData = Box::into_raw(Box::new(Mutex::new(Driver::new()))).cast();
        (*ctx).vtable.write(table);
    })) {
        Ok(()) => 0,
        Err(_) => FAILED,
    }
}
