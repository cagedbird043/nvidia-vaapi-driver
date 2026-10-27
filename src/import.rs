//! External DRM PRIME buffers as VA surfaces. `Dma` is the validated, immutable description of a
//! dma-buf-backed picture (a client's buffer, or a decode surface's exported backing); the encoder
//! thread ingests it on the copy engine without touching the pixels on the CPU.
use crate::{va::*, FAILED, INVALID, NV12, P010, Q416, R, YUV444P};
use nvrm::video::enc::ingest;
use std::{
    os::fd::{AsRawFd, BorrowedFd, OwnedFd},
};

/// DRM_FORMAT_ARGB8888 / XRGB8888 ("AR24" / "XR24"): the same little-endian BGRA byte order as
/// VA_FOURCC_ARGB / XRGB.
const AR24: u32 = u32::from_le_bytes(*b"AR24");
const XR24: u32 = u32::from_le_bytes(*b"XR24");
const P012: u32 = VA_FOURCC_P012;
const P016: u32 = VA_FOURCC_P016;
const DRM_FORMAT_MOD_LINEAR: u64 = 0;
const UNSUPPORTED_MEMORY: VAStatus = VA_STATUS_ERROR_UNSUPPORTED_MEMORY_TYPE as VAStatus;
const ALLOCATION: VAStatus = VA_STATUS_ERROR_ALLOCATION_FAILED as VAStatus;

/// Samples of a picture the encoder can ingest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// 8-bit 4:2:0, interleaved CbCr.
    Nv12,
    /// 16-bit containers holding 10..16 significant bits in the high bits, interleaved CbCr.
    P01x,
    /// 8-bit 4:4:4, three planes (444P).
    Yuv444,
    /// 16-bit 4:4:4 with the samples in the high bits, three planes (Q416).
    Q416,
    /// 8-bit B, G, R, A in memory order.
    Bgra,
}
impl Kind {
    pub fn of(fourcc: u32) -> Option<Self> {
        match fourcc {
            NV12 => Some(Kind::Nv12),
            P010 | P012 | P016 => Some(Kind::P01x),
            YUV444P => Some(Kind::Yuv444),
            Q416 => Some(Kind::Q416),
            VA_FOURCC_ARGB | VA_FOURCC_XRGB | AR24 | XR24 => Some(Kind::Bgra),
            _ => None,
        }
    }
    fn planes(self) -> usize {
        match self {
            Kind::Bgra => 1,
            Kind::Nv12 | Kind::P01x => 2,
            Kind::Yuv444 | Kind::Q416 => 3,
        }
    }
    fn bytes_per_sample(self) -> u32 {
        match self {
            Kind::Nv12 | Kind::Yuv444 => 1,
            Kind::P01x | Kind::Q416 => 2,
            Kind::Bgra => 4,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Plane {
    pub object: usize,
    pub offset: u32,
    pub pitch: u32,
}

pub struct Dma {
    pub fourcc: u32,
    pub kind: Kind,
    pub width: u32,
    pub height: u32,
    objects: Vec<OwnedFd>,
    sizes: Vec<u32>,
    modifiers: Vec<u64>,
    /// Luma and CbCr for 4:2:0, luma, Cb and Cr for 4:4:4, one plane for BGRA.
    planes: Vec<Plane>,
}

fn duplicate(fd: i32) -> R<OwnedFd> {
    if fd < 0 {
        return Err(INVALID);
    }
    // SAFETY: the client lends a descriptor valid for the duration of the call.
    unsafe { BorrowedFd::borrow_raw(fd) }
        .try_clone_to_owned()
        .map_err(|_| INVALID)
}

fn dma_buf_size(fd: &OwnedFd) -> R<u32> {
    // SAFETY: lseek on a live descriptor; dma-bufs implement SEEK_END as their size.
    let size = unsafe { libc_lseek(fd.as_raw_fd(), 0, 2) };
    u32::try_from(size).ok().filter(|s| *s > 0).ok_or(INVALID)
}
extern "C" {
    #[link_name = "lseek"]
    fn libc_lseek(fd: i32, offset: i64, whence: i32) -> i64;
}

impl Dma {
    fn new(
        fourcc: u32,
        width: u32,
        height: u32,
        objects: Vec<OwnedFd>,
        modifiers: Vec<u64>,
        planes: Vec<Plane>,
    ) -> R<Self> {
        let kind = Kind::of(fourcc).ok_or_else(|| {
            eprintln!("rustnv: unsupported external fourcc {fourcc:#x}");
            UNSUPPORTED_MEMORY
        })?;
        let wanted = kind.planes();
        if planes.len() < wanted || objects.is_empty() || planes.iter().any(|p| p.object >= objects.len()) {
            return Err(INVALID);
        }
        let sizes = objects.iter().map(dma_buf_size).collect::<R<Vec<_>>>()?;
        let dma = Self { fourcc, kind, width, height, objects, sizes, modifiers, planes };
        dma.source()?; // layout and modifier checked up front
        Ok(dma)
    }
    /// `VADRMPRIMESurfaceDescriptor`: layers are flattened into one plane list, so a client may
    /// describe NV12 as one two-plane layer or as separate R8 and GR88 layers.
    pub unsafe fn from_prime2(desc: &VADRMPRIMESurfaceDescriptor) -> R<Self> {
        let objects = desc.num_objects as usize;
        if objects == 0 || objects > desc.objects.len() || desc.num_layers == 0 || desc.num_layers as usize > desc.layers.len() {
            return Err(INVALID);
        }
        let fds = desc.objects[..objects].iter().map(|o| duplicate(o.fd)).collect::<R<Vec<_>>>()?;
        let modifiers = desc.objects[..objects].iter().map(|o| o.drm_format_modifier).collect();
        let mut planes = Vec::new();
        for layer in &desc.layers[..desc.num_layers as usize] {
            for p in 0..(layer.num_planes as usize).min(layer.object_index.len()) {
                planes.push(Plane { object: layer.object_index[p] as usize, offset: layer.offset[p], pitch: layer.pitch[p] });
            }
        }
        Self::new(desc.fourcc, desc.width, desc.height, fds, modifiers, planes)
    }
    /// Legacy `VASurfaceAttribExternalBuffers` carry no modifier: only buffers the client vouches for
    /// as linear (`NVD_DRM_PRIME_ASSUME_LINEAR`, as in the C driver) are imported.
    pub unsafe fn from_prime(ext: &VASurfaceAttribExternalBuffers, assume_linear: bool) -> R<Self> {
        if !assume_linear {
            eprintln!(
                "rustnv: DRM_PRIME import needs DRM_PRIME_2 to carry modifiers; set NVD_DRM_PRIME_ASSUME_LINEAR=1 for linear callers"
            );
            return Err(UNSUPPORTED_MEMORY);
        }
        let buffers = ext.num_buffers as usize;
        if ext.buffers.is_null() || buffers == 0 || buffers > 4 || ext.num_planes == 0 || ext.num_planes > 4 {
            return Err(INVALID);
        }
        let fds = std::slice::from_raw_parts(ext.buffers, buffers)
            .iter()
            .map(|fd| duplicate(*fd as i32))
            .collect::<R<Vec<_>>>()?;
        let planes = (0..ext.num_planes as usize)
            .map(|p| Plane {
                object: if buffers == 1 { 0 } else { p.min(buffers - 1) },
                offset: ext.offsets[p],
                pitch: ext.pitches[p],
            })
            .collect();
        let modifiers = vec![DRM_FORMAT_MOD_LINEAR; buffers];
        Self::new(ext.pixel_format, ext.width, ext.height, fds, modifiers, planes)
    }
    /// A decode surface's exported backing: one dma-buf holding luma then the chroma planes at
    /// `offsets[1]` (CbCr) for 4:2:0, or `offsets[1]` (Cb) and `offsets[2]` (Cr) for 4:4:4.
    #[allow(clippy::too_many_arguments)]
    pub fn from_backing(fourcc: u32, fd: &OwnedFd, size: u32, modifier: u64, pitch: u32, offsets: [u32; 3], width: u32, height: u32) -> R<Self> {
        let kind = Kind::of(fourcc).filter(|k| *k != Kind::Bgra).ok_or(INVALID)?;
        let planes = offsets[..kind.planes()].iter().map(|&offset| Plane { object: 0, offset, pitch }).collect();
        let objects = vec![fd.try_clone().map_err(|_| FAILED)?];
        let dma = Self { fourcc, kind, width, height, objects, sizes: vec![size], modifiers: vec![modifier], planes };
        dma.source()?;
        Ok(dma)
    }
    fn layout(&self) -> R<ingest::Layout> {
        let first = ingest::layout_from_modifier(self.modifiers[self.planes[0].object]);
        let layout = first.ok_or_else(|| {
            eprintln!("rustnv: unsupported dma-buf modifier {:#x}", self.modifiers[self.planes[0].object]);
            ALLOCATION
        })?;
        for plane in &self.planes[..self.kind.planes()] {
            if ingest::layout_from_modifier(self.modifiers[plane.object]) != Some(layout) {
                return Err(ALLOCATION);
            }
        }
        Ok(layout)
    }
    /// The description the RM ingests; borrows this buffer's descriptors.
    pub fn source(&self) -> R<ingest::Source> {
        let plane = |i: usize| {
            let p = self.planes[i.min(self.planes.len() - 1)];
            ingest::Plane { fd: self.objects[p.object].as_raw_fd(), offset: u64::from(p.offset), pitch: p.pitch }
        };
        Ok(ingest::Source {
            planes: [plane(0), plane(1), plane(2)],
            layout: self.layout()?,
            bytes_per_sample: self.kind.bytes_per_sample(),
            chroma444: matches!(self.kind, Kind::Yuv444 | Kind::Q416),
        })
    }
    /// Duplicated objects and per-plane geometry for `vaExportSurfaceHandle`.
    pub fn export(&self, composed: bool) -> R<VADRMPRIMESurfaceDescriptor> {
        let planes = self.kind.planes();
        let composed_format = match self.kind {
            Kind::Bgra => Some(AR24),
            Kind::Yuv444 => Some(u32::from_le_bytes(*b"YU24")),
            Kind::Q416 => None,
            Kind::Nv12 | Kind::P01x => Some(self.fourcc),
        };
        let layers = if composed || planes == 1 { 1 } else { planes };
        let layer_format = if layers == 1 {
            composed_format.ok_or(VA_STATUS_ERROR_INVALID_SURFACE as VAStatus)?
        } else {
            0
        };
        let mut desc = VADRMPRIMESurfaceDescriptor { fourcc: self.fourcc, width: self.width, height: self.height, num_objects: self.objects.len() as u32, ..Default::default() };
        for (i, fd) in self.objects.iter().enumerate() {
            let copy = fd.try_clone().map_err(|_| FAILED)?;
            desc.objects[i].fd = std::os::fd::IntoRawFd::into_raw_fd(copy);
            desc.objects[i].size = self.sizes[i];
            desc.objects[i].drm_format_modifier = self.modifiers[i];
        }
        let drm = |p: usize| match (self.kind, p) {
            (Kind::Nv12 | Kind::Yuv444, 0) | (Kind::Yuv444, _) => u32::from_le_bytes(*b"R8  "),
            (Kind::Nv12, _) => u32::from_le_bytes(*b"RG88"),
            (Kind::P01x | Kind::Q416, 0) | (Kind::Q416, _) => u32::from_le_bytes(*b"R16 "),
            (Kind::P01x, _) => u32::from_le_bytes(*b"RG32"),
            (Kind::Bgra, _) => AR24,
        };
        desc.num_layers = layers as u32;
        for (l, layer) in desc.layers[..layers].iter_mut().enumerate() {
            let covered = if layers == 1 { planes } else { 1 };
            layer.drm_format = if layers == 1 { layer_format } else { drm(l) };
            layer.num_planes = covered as u32;
            for k in 0..covered {
                let p = self.planes[if layers == 1 { k } else { l }];
                layer.object_index[k] = p.object as u32;
                layer.offset[k] = p.offset;
                layer.pitch[k] = p.pitch;
            }
        }
        Ok(desc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        assert_eq!(Kind::of(NV12), Some(Kind::Nv12));
        assert_eq!(Kind::of(P010), Some(Kind::P01x));
        assert_eq!(Kind::of(VA_FOURCC_P016), Some(Kind::P01x));
        assert_eq!(Kind::of(AR24), Some(Kind::Bgra));
        assert_eq!(Kind::of(VA_FOURCC_ARGB), Some(Kind::Bgra));
        assert_eq!(Kind::of(u32::from_le_bytes(*b"444P")), Some(Kind::Yuv444));
        assert_eq!(Kind::of(u32::from_le_bytes(*b"Q416")), Some(Kind::Q416));
        assert_eq!(Kind::of(u32::from_le_bytes(*b"YUY2")), None);
    }
}
