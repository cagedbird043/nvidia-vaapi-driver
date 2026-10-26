//! RM sessions behind the VA frontend. Each codec module turns a VA picture's parameter buffers
//! into input for the session's CodecDecoder; this module owns the session, the stable exported
//! surface storage and CPU read-back.
mod av1;
mod h264;
mod hevc;
mod vp9;

use crate::Format;
use nvrm::video::{
    self,
    session::{OutputAllocation, OutputAllocator, Session, SessionInfo, Target},
    SurfaceLayout,
};
use std::{
    collections::HashMap,
    mem::size_of,
    ops::Range,
    os::fd::{AsRawFd, OwnedFd},
};

const BLOCK_LINEAR: u64 = 0x0300_0000_0060_6010;

/// Codec family of a VA decoding context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
    Vp9,
    Av1,
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
/// formats with separate Cb and Cr planes. Planes start at multiples of 64 rows.
fn backing_layout(width: u32, height: u32, format: Format) -> (u32, [u32; 3], usize) {
    let layout = SurfaceLayout::new(width, height, format.bytes_per_sample());
    let plane = layout.chroma_offset as u32;
    let offsets = [0, plane, if format.chroma_444 { 2 * plane } else { 0 }];
    let size = if format.chroma_444 {
        3 * plane as usize
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
        if width == 0 || height == 0 || width > 4096 || height > 4096 {
            return Err("output dimensions must be within 1..4096".into());
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
    /// Linear CPU-readable copy target, allocated on the first download and reused.
    staging: Option<OutputAllocation>,
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
    /// Decodes one picture into `picture.target` and waits for the hardware; runs on the worker.
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
        let raw = |i: usize| output.fds.get(i).map_or(-1, |fd| fd.as_raw_fd());
        let target = Target {
            dmabuf_fd: [raw(0), raw(1), raw(2)],
            offset: output.offsets.map(u64::from),
            pitch: output.pitches,
            modifier: output.modifier,
            format: self.format.target(),
            width: self.width,
            height: self.height,
            acquire_fence_fd: -1,
        };
        self.session
            .copy_out_wait(id, &target)
            .map_err(|e| e.to_string())
    }
    /// The picture's samples with rows packed at `width * bytes per sample`: the luma plane, then
    /// the interleaved CbCr plane (4:2:0) or the Cb and Cr planes (4:4:4), see [`Format::packed`].
    pub fn readback(&mut self, target: u32) -> Result<Vec<u8>, String> {
        let id = *self
            .pictures
            .get(&target)
            .ok_or("surface has not been decoded")?;
        let (w, rows) = (self.width as usize, self.height as usize);
        let bps = self.format.bytes_per_sample() as usize;
        // LINEAR copy targets need a 4-byte-aligned pitch; the result is repacked to `width`.
        let pitch = (w * bps).next_multiple_of(4);
        let plane = pitch * rows;
        let chroma_rows = if self.format.chroma_444 { rows } else { rows.div_ceil(2) };
        let size = plane + pitch * chroma_rows * (self.format.planes() - 1);
        if self.staging.is_none() {
            let alloc = self
                .session
                .allocate_output(size.next_multiple_of(65536))
                .map_err(|e| e.to_string())?;
            self.staging = Some(alloc);
        }
        let alloc = self.staging.as_ref().unwrap();
        let fd = alloc.fd.as_raw_fd();
        let planes = self.format.planes();
        let chroma_plane = pitch * chroma_rows;
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
        let map = alloc.map().map_err(|e| e.to_string())?;
        let line = w * bps;
        let mut packed = vec![0; self.format.packed(self.width, self.height).2];
        let mut dest = packed.chunks_exact_mut(line);
        let planes = [(0, rows), (plane, chroma_rows), (plane + chroma_plane, chroma_rows)];
        for &(start, count) in &planes[..self.format.planes()] {
            for row in 0..count {
                let d = dest.next().ok_or("packed read-back is shorter than its planes")?;
                map.read_bytes(start + row * pitch, d).map_err(|e| e.to_string())?;
            }
        }
        Ok(packed)
    }
    pub fn release_surface(&mut self, target: u32) {
        if let Some(id) = self.pictures.remove(&target) {
            let _ = self.session.release(id);
        }
        self.front.release(target);
    }
}
