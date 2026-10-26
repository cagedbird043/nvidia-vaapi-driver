//! RM sessions behind the VA frontend. Each codec module turns a VA picture's parameter buffers
//! into input for the session's CodecDecoder; this module owns the session, the stable exported
//! surface storage and CPU read-back.
mod av1;
mod h264;
mod hevc;
mod vp9;

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

/// The codec half of a VA decoder: validates a picture and stages its parameters and references
/// for the session's CodecDecoder.
trait Frontend {
    /// Returns the bitstream unit the session submits for this picture.
    fn stage(&mut self, picture: &Picture) -> Result<Vec<u8>, String>;
    /// The client released `target`; forget its reference mapping.
    fn release(&mut self, target: u32);
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

pub struct Decoder {
    session: Session,
    front: Box<dyn Frontend>,
    pictures: HashMap<u32, u64>,
    /// Linear CPU-readable copy target, allocated on the first download and reused.
    staging: Option<OutputAllocation>,
    width: u32,
    height: u32,
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
    ) -> Result<Self, String> {
        let dev = video::VideoDevice::open_on(Some(pci)).map_err(|e| e.to_string())?;
        // This exact DRM modifier describes the verified pre-Blackwell NV12 layout.
        if !matches!(dev.info.nvdec_class, 0xc4b0 | 0xc7b0 | 0xc9b0) {
            return Err(
                "VA export requires verified Turing/Ampere/Ada block-linear NV12 layout".into(),
            );
        }
        let class = dev.info.nvdec_class;
        let (front, codec) = match codec {
            Codec::H264 => h264::create(class, width, height)?,
            Codec::Hevc => hevc::create(class, width, height)?,
            Codec::Vp9 => vp9::create(class, width, height)?,
            Codec::Av1 => av1::create(class, width, height)?,
        };
        // The session pool holds dpb_size() + 2 surfaces before the extra slots.
        let base = codec.dpb_size() + 2;
        let session = Session::with_codec(
            &dev,
            SessionInfo {
                codec: codec.codec(),
                max_width: width,
                max_height: height,
                bit_depth: 8,
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
        // LINEAR copy targets need a 4-byte-aligned pitch; the result is repacked to `width`.
        let pitch = self.width.next_multiple_of(4);
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
        let (w, rows) = (self.width as usize, self.height as usize);
        let mut packed = vec![0; w * rows + w * rows.div_ceil(2)];
        let (luma, chroma) = packed.split_at_mut(w * rows);
        for (row, dest) in luma.chunks_exact_mut(w).enumerate() {
            map.read_bytes(row * pitch as usize, dest).map_err(|e| e.to_string())?;
        }
        for (row, dest) in chroma.chunks_exact_mut(w).enumerate() {
            map.read_bytes(y + row * pitch as usize, dest).map_err(|e| e.to_string())?;
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
