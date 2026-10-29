//! RM sessions behind the VA frontend. Each codec module turns a VA picture's parameter buffers
//! into input for the session's CodecDecoder; this module owns the session, the stable exported
//! surface storage and CPU read-back.
mod av1;
mod h264;
mod hevc;
mod mpeg2;
mod mpeg4;
mod vc1;
mod vp8;
mod vp9;

use crate::Format;
use nvrm::video::{
    self,
    session::{OutputAllocation, OutputAllocator, OutputMapping, Session, SessionInfo, Target},
    SurfaceLayout,
};
use std::{
    collections::HashMap,
    mem::size_of,
    ops::Range,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
};

/// Pitch granularity of a LINEAR copy-engine target.
const STAGING_PITCH_ALIGN: usize = 4;
/// Allocation granularity of the read-back staging buffer.
const STAGING_SIZE_ALIGN: usize = 65536;
const BLOCK_LINEAR: u64 = 0x0300_0000_0060_6010;

/// Codec family of a VA decoding context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
    Vp9,
    Av1,
    Mpeg2,
    Mpeg4,
    Vc1,
    Vp8,
}

/// The luma-sample rectangle of a picture that a read-back copies.
#[derive(Clone, Copy)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// One VA picture as submitted: raw parameter buffers, and complete slices (or tiles) whose data
/// ranges index `data`.
pub struct Picture {
    pub target: u32,
    pub params: Vec<u8>,
    pub iq: Option<Vec<u8>>,
    pub slices: Vec<Slice>,
    pub data: Vec<u8>,
}
pub struct Slice {
    pub param: Vec<u8>,
    pub data: Range<usize>,
}

/// Reads a libva parameter struct from a buffer the frontend validated to be exactly its size.
fn read<T: Copy>(bytes: &[u8], what: &str) -> Result<T, String> {
    if bytes.len() != size_of::<T>() {
        return Err(format!("{what}: {} bytes, expected {}", bytes.len(), size_of::<T>()));
    }
    // SAFETY: libva parameter structs are plain integer data; any bit pattern is valid.
    Ok(unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast()) })
}

/// Reads the leading libva parameter struct `T` from a buffer the frontend validated to be one of
/// `sizes` (the plain struct, or an extension struct that starts with it).
fn read_prefix<T: Copy>(bytes: &[u8], sizes: &[usize], what: &str) -> Result<T, String> {
    if !sizes.contains(&bytes.len()) || bytes.len() < size_of::<T>() {
        return Err(format!("{what}: {} bytes, expected one of {sizes:?}", bytes.len()));
    }
    // SAFETY: libva parameter structs are plain integer data; any bit pattern is valid.
    Ok(unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast()) })
}

/// The codec half of a VA decoder: validates a picture and stages its parameters and references
/// for the session's CodecDecoder.
trait Frontend {
    /// Returns the bitstream unit the session submits for this picture.
    fn stage(&mut self, picture: &Picture) -> Result<Vec<u8>, String>;
    /// The client released `target`; forget its reference mapping.
    fn release(&mut self, target: u32);
}

/// A surface's exported backing: one dma-buf holding every plane, named once per plane (the VA
/// descriptor lists one object per plane, each a duplicate of the same buffer).
pub struct Export {
    pub fds: Vec<OwnedFd>,
    pub pitches: [u32; 3],
    pub offsets: [u32; 3],
    pub size: u32,
    pub modifier: u64,
}
impl Export {
    pub fn try_clone(&self) -> Result<Self, String> {
        Ok(Self {
            fds: self
                .fds
                .iter()
                .map(|fd| fd.try_clone().map_err(|e| e.to_string()))
                .collect::<Result<_, _>>()?,
            pitches: self.pitches,
            offsets: self.offsets,
            size: self.size,
            modifier: self.modifier,
        })
    }
}

/// Plane geometry of the block-linear backing for a `format` surface of `width` x `height`:
/// (pitch, plane offsets, size). Luma first; 4:2:0 formats continue with one CbCr plane, 4:4:4
/// formats with separate Cb and Cr planes; RGB has its one plane. Planes start at multiples of
/// 64 rows.
fn backing_layout(width: u32, height: u32, format: Format) -> (u32, [u32; 3], usize) {
    let layout = SurfaceLayout::new(width, height, format.bytes_per_sample());
    let plane = layout.chroma_offset as u32;
    let offsets = [0, plane, if format.chroma_444 { 2 * plane } else { 0 }];
    let size = if format.chroma_444 {
        3 * plane as usize
    } else if format.rgb() {
        plane as usize
    } else {
        layout.size
    };
    (layout.pitch, offsets, size.next_multiple_of(1 << 16))
}

struct OutputBacking {
    allocation: OutputAllocation,
    pitch: u32,
    offsets: [u32; 3],
    size: u32,
    width: u32,
    height: u32,
    format: Format,
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

    pub fn export(&mut self, id: u32, width: u32, height: u32, format: Format) -> Result<Export, String> {
        if width == 0 || height == 0 || width > 8192 || height > 8192 {
            return Err("output dimensions must be within 1..8192".into());
        }
        if let Some(backing) = self.surfaces.get(&id) {
            if (backing.width, backing.height, backing.format) != (width, height, format) {
                return Err("cannot change the dimensions or format of an existing surface backing".into());
            }
        } else {
            let (pitch, offsets, size) = backing_layout(width, height, format);
            let allocation = self.allocator.allocate_zeroed(size).map_err(|e| e.to_string())?;
            self.surfaces.insert(
                id,
                OutputBacking { allocation, pitch, offsets, size: size as u32, width, height, format },
            );
        }
        let backing = &self.surfaces[&id];
        let fds = (0..format.planes())
            .map(|_| backing.allocation.fd.try_clone().map_err(|e| e.to_string()))
            .collect::<Result<_, _>>()?;
        Ok(Export {
            fds,
            pitches: [backing.pitch; 3],
            offsets: backing.offsets,
            size: backing.size,
            modifier: BLOCK_LINEAR,
        })
    }

    pub fn release(&mut self, id: u32) {
        self.surfaces.remove(&id);
    }
}

pub struct Decoder {
    session: Session,
    front: Box<dyn Frontend>,
    pictures: HashMap<u32, u64>,
    /// Linear CPU-readable copy target and its mapping, created on the first download and reused.
    staging: Option<(OutputAllocation, OutputMapping)>,
    width: u32,
    height: u32,
    format: Format,
}
impl Decoder {
    /// `surfaces` is the number of VA render targets bound to the context (0 when the client binds
    /// none up front). Each target retains at most one decoded picture and one more is being decoded.
    pub fn new(
        codec: Codec,
        pci: [u32; 4],
        width: u32,
        height: u32,
        surfaces: usize,
        format: Format,
    ) -> Result<Self, String> {
        let dev = video::VideoDevice::open_on(Some(pci)).map_err(|e| e.to_string())?;
        // This exact DRM modifier describes the verified pre-Blackwell block-linear layout.
        if !matches!(dev.info.nvdec_class, 0xc4b0 | 0xc7b0 | 0xc9b0) {
            return Err(
                "VA export requires verified Turing/Ampere/Ada block-linear layout".into(),
            );
        }
        let class = dev.info.nvdec_class;
        let (front, codec) = match codec {
            Codec::H264 => h264::create(class, width, height)?,
            Codec::Hevc => hevc::create(class, width, height, format)?,
            Codec::Vp9 => vp9::create(class, width, height, format)?,
            Codec::Av1 => av1::create(class, width, height, format)?,
            Codec::Mpeg2 => mpeg2::create(width, height)?,
            Codec::Mpeg4 => mpeg4::create(width, height)?,
            Codec::Vc1 => vc1::create(width, height)?,
            Codec::Vp8 => vp8::create(width, height)?,
        };
        // The session pool holds dpb_size() + 2 surfaces before the extra slots.
        let base = codec.dpb_size() + 2;
        let session = Session::with_codec(
            &dev,
            SessionInfo {
                codec: codec.codec(),
                max_width: width,
                max_height: height,
                bit_depth: format.bit_depth,
                extra_output_slots: if surfaces == 0 {
                    32
                } else {
                    u32::try_from((surfaces + 1).saturating_sub(base))
                        .map_err(|_| "too many surfaces")?
                },
            },
            codec,
        )
        .map_err(|e| e.to_string())?;
        Ok(Self {
            session,
            front,
            pictures: HashMap::new(),
            staging: None,
            width,
            height,
            format,
        })
    }
    /// Submits one picture into `picture.target`; runs on the worker. The engine status is
    /// confirmed later by [`Decoder::finish`] or by a copy of the picture.
    pub fn decode(&mut self, picture: &Picture) -> Result<(), String> {
        let bits = self.front.stage(picture)?;
        let target = picture.target;
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
        Ok(())
    }
    /// Waits for the decode of `target` and checks its engine status.
    pub fn finish(&mut self, target: u32) -> Result<(), String> {
        let id = *self.pictures.get(&target).ok_or("surface has not been decoded")?;
        self.session.wait_picture(id).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Queues the copy of the decoded pixels into the surface's stable externally shared storage
    /// and returns its fence (None: nothing left to wait for). The copy engine preserves the
    /// block-linear layout; no pixels cross the CPU. The copy first waits for the readers of the
    /// previous content (their fences on the dma-buf). The picture's engine status is not checked
    /// here; see [`Decoder::finish`].
    pub fn copy_into(&mut self, target: u32, output: &Export) -> Result<Option<OwnedFd>, String> {
        let id = *self
            .pictures
            .get(&target)
            .ok_or("surface has not been decoded")?;
        let readers = dmabuf_fence(&output.fds[0])?;
        let raw = |i: usize| output.fds.get(i).map_or(-1, |fd| fd.as_raw_fd());
        let target = Target {
            dmabuf_fd: [raw(0), raw(1), raw(2)],
            offset: output.offsets.map(u64::from),
            pitch: output.pitches,
            modifier: output.modifier,
            format: self.format.target(),
            width: self.width,
            height: self.height,
            acquire_fence_fd: readers.as_ref().map_or(-1, |f| f.as_raw_fd()),
        };
        self.session.copy_out_deferred(id, &target).map_err(|e| e.to_string())
    }
    /// Copies the `rect` of the picture into `dest`, laid out as [`Format::packed`] for the rect's
    /// size: the luma plane, then the interleaved CbCr plane (4:2:0) or the Cb and Cr planes
    /// (4:4:4). The copy engine writes a persistent CPU-cached system-memory staging buffer, so the
    /// CPU reads it at memory speed; each row goes straight into `dest`.
    pub fn readback(&mut self, target: u32, rect: Rect, dest: &mut [u8]) -> Result<(), String> {
        let id = *self
            .pictures
            .get(&target)
            .ok_or("surface has not been decoded")?;
        let (w, rows) = (self.width as usize, self.height as usize);
        let bps = self.format.bytes_per_sample() as usize;
        let (rx, ry, rw, rh) = (rect.x as usize, rect.y as usize, rect.w as usize, rect.h as usize);
        if rx + rw > w || ry + rh > rows || rx % 2 != 0 || ry % 2 != 0 || rw % 2 != 0 || rh % 2 != 0 {
            return Err("read-back rectangle outside the surface".into());
        }
        let (dpitch, doffsets, dsize) = self.format.packed(rect.w, rect.h);
        let dest = dest.get_mut(..dsize).ok_or("read-back destination is too small")?;
        // LINEAR copy targets need a 4-byte-aligned pitch.
        let pitch = (w * bps).next_multiple_of(STAGING_PITCH_ALIGN);
        let plane = pitch * rows;
        let chroma_rows = self.format.chroma_rows(self.height) as usize;
        let chroma_plane = pitch * chroma_rows;
        let planes = self.format.planes();
        if self.staging.is_none() {
            let size = plane + chroma_plane * (planes - 1);
            let alloc = self
                .session
                .allocate_readback(size.next_multiple_of(STAGING_SIZE_ALIGN))
                .map_err(|e| e.to_string())?;
            let map = alloc.map().map_err(|e| e.to_string())?;
            self.staging = Some((alloc, map));
        }
        let (alloc, map) = self.staging.as_ref().unwrap();
        let fd = alloc.fd.as_raw_fd();
        let t = Target {
            dmabuf_fd: [fd, fd, if planes == 3 { fd } else { -1 }],
            offset: [0, plane as u64, (plane + chroma_plane) as u64],
            pitch: [pitch as u32; 3],
            modifier: 0,
            format: self.format.target(),
            width: self.width,
            height: self.height,
            acquire_fence_fd: -1,
        };
        self.session
            .copy_out_wait(id, &t)
            .map_err(|e| e.to_string())?;
        let src = map.bytes().map_err(|e| e.to_string())?;
        let row_bytes = rw * bps;
        // (staging plane start, destination plane start, rows, first source row)
        let spans = [
            (0, doffsets[0] as usize, rh, ry),
            (plane, doffsets[1] as usize, self.format.chroma_rows(rect.h) as usize, if self.format.chroma_444 { ry } else { ry / 2 }),
            (plane + chroma_plane, doffsets[2] as usize, rh, ry),
        ];
        for &(start, base, count, first) in &spans[..planes] {
            for row in 0..count {
                let from = start + (first + row) * pitch + rx * bps;
                let to = base + row * dpitch as usize;
                dest[to..to + row_bytes].copy_from_slice(&src[from..from + row_bytes]);
            }
        }
        Ok(())
    }
    pub fn release_surface(&mut self, target: u32) {
        if let Some(id) = self.pictures.remove(&target) {
            let _ = self.session.release(id);
        }
        self.front.release(target);
    }
}

extern "C" {
    #[link_name = "ioctl"]
    fn libc_ioctl(fd: i32, request: u64, ...) -> i32;
}
/// `struct dma_buf_export_sync_file` (linux/dma-buf.h).
#[repr(C)]
struct DmaBufSyncFile {
    flags: u32,
    fd: i32,
}
const DMA_BUF_SYNC_READ: u32 = 1;
const DMA_BUF_SYNC_WRITE: u32 = 2;
/// _IOWR('b', 2, struct dma_buf_export_sync_file)
const DMA_BUF_IOCTL_EXPORT_SYNC_FILE: u64 = 0xc008_6202;

/// The implicit fences a user of `buf` with `access` (DMA_BUF_SYNC_READ / _WRITE) has to wait
/// for, or None when there are none.
fn dmabuf_sync_file(buf: &OwnedFd, access: u32) -> Result<Option<OwnedFd>, String> {
    let mut arg = DmaBufSyncFile { flags: access, fd: -1 };
    // SAFETY: valid dma-buf fd and argument struct of the ioctl's layout.
    if unsafe { libc_ioctl(buf.as_raw_fd(), DMA_BUF_IOCTL_EXPORT_SYNC_FILE, &mut arg) } != 0 {
        return Err(format!("DMA_BUF_IOCTL_EXPORT_SYNC_FILE: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: the kernel returned a new sync_file fd that we now own.
    Ok((arg.fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(arg.fd) }))
}
/// The fences a writer of `buf` has to wait for (every reader and writer).
fn dmabuf_fence(buf: &OwnedFd) -> Result<Option<OwnedFd>, String> {
    dmabuf_sync_file(buf, DMA_BUF_SYNC_WRITE)
}
/// Waits for the writers fenced on `buf` (another device or API that rendered into it).
pub fn wait_writers(buf: &OwnedFd) -> Result<(), String> {
    dmabuf_sync_file(buf, DMA_BUF_SYNC_READ)?.map_or(Ok(()), |f| wait_fence(&f))
}
/// Waits for every reader and writer fenced on `buf`, before writing into it.
pub fn wait_users(buf: &OwnedFd) -> Result<(), String> {
    dmabuf_fence(buf)?.map_or(Ok(()), |f| wait_fence(&f))
}

extern "C" {
    #[link_name = "poll"]
    fn libc_poll(fds: *mut PollFd, n: u64, timeout_ms: i32) -> i32;
}
#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

/// Waits until the sync_file `fence` signals, at most 5 s.
pub fn wait_fence(fence: &OwnedFd) -> Result<(), String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        let mut p = PollFd { fd: fence.as_raw_fd(), events: 1, revents: 0 }; // POLLIN
        // SAFETY: one valid pollfd.
        match unsafe { libc_poll(&mut p, 1, left.as_millis() as i32) } {
            1 => return Ok(()),
            0 => return Err("copy fence timed out".into()),
            _ if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => continue,
            _ => return Err(format!("poll on copy fence: {}", std::io::Error::last_os_error())),
        }
    }
}
