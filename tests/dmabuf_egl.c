// Decodes the first frames with VA-API, exports each surface with vaExportSurfaceHandle(DRM_PRIME_2,
// separate layers), imports every plane into EGL (EGL_EXT_image_dma_buf_import(_modifiers)) as an
// R8/GR88/R16/GR1616 image on the same GPU, reads it back through a framebuffer and compares it with
// vaGetImage of the same surface.
// usage: dmabuf_egl <render node> <input> <NV12|P010|P012|444P|Q416> [frames]
#define EGL_EGLEXT_PROTOTYPES
#define GL_GLEXT_PROTOTYPES
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl3.h>
#include <GLES2/gl2ext.h>
#include <gbm.h>
#include <drm_fourcc.h>
#include <fcntl.h>
#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_vaapi.h>
#include <va/va.h>
#include <va/va_drmcommon.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define CHECK(call) do { VAStatus status_ = (call); if (status_) { fprintf(stderr, "%s: %s\n", #call, vaErrorStr(status_)); exit(1); } } while (0)

static VADisplay display;
static unsigned render_target_format, surface_width, surface_height;
static enum AVPixelFormat frames_format;
static void free_surface(void *opaque, uint8_t *data) {
    VASurfaceID id = (VASurfaceID)(uintptr_t)data;
    vaDestroySurfaces(display, &id, 1);
}
static AVBufferRef *alloc_surface(void *opaque, size_t size) {
    VASurfaceID id;
    VASurfaceAttrib attribute = {.type = VASurfaceAttribUsageHint, .flags = VA_SURFACE_ATTRIB_SETTABLE,
        .value = {.type = VAGenericValueTypeInteger, .value.i = VA_SURFACE_ATTRIB_USAGE_HINT_DECODER}};
    CHECK(vaCreateSurfaces(display, render_target_format, surface_width, surface_height, &id, 1, &attribute, 1));
    return av_buffer_create((uint8_t *)(uintptr_t)id, sizeof id, free_surface, NULL, 0);
}
static enum AVPixelFormat pick(AVCodecContext *c, const enum AVPixelFormat *f) {
    for (; *f != AV_PIX_FMT_NONE; f++) {
        if (*f != AV_PIX_FMT_VAAPI) continue;
        AVBufferRef *frames = av_hwframe_ctx_alloc(c->hw_device_ctx);
        AVHWFramesContext *fc = (AVHWFramesContext *)frames->data;
        fc->format = AV_PIX_FMT_VAAPI;
        fc->sw_format = frames_format;
        fc->width = surface_width = FFALIGN(c->coded_width, 32);
        fc->height = surface_height = FFALIGN(c->coded_height, 32);
        fc->pool = av_buffer_pool_init2(sizeof(VASurfaceID), NULL, alloc_surface, NULL);
        if (av_hwframe_ctx_init(frames) < 0) return AV_PIX_FMT_NONE;
        c->hw_frames_ctx = frames;
        return *f;
    }
    return AV_PIX_FMT_NONE;
}

static PFNGLEGLIMAGETARGETTEXTURE2DOESPROC target_texture;

// Reads one imported plane (a DRM layer with a single plane) back as tightly packed rows.
static void read_plane(EGLDisplay egl, const VADRMPRIMESurfaceDescriptor *d, int layer, int w, int h, int channels, int bytes, unsigned char *out) {
    unsigned plane_fourcc = d->layers[layer].drm_format;
    int object = d->layers[layer].object_index[0];
    EGLAttrib attributes[] = {
        EGL_WIDTH, w, EGL_HEIGHT, h, EGL_LINUX_DRM_FOURCC_EXT, plane_fourcc,
        EGL_DMA_BUF_PLANE0_FD_EXT, d->objects[object].fd,
        EGL_DMA_BUF_PLANE0_OFFSET_EXT, d->layers[layer].offset[0],
        EGL_DMA_BUF_PLANE0_PITCH_EXT, d->layers[layer].pitch[0],
        EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT, (uint32_t)d->objects[object].drm_format_modifier,
        EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT, (uint32_t)(d->objects[object].drm_format_modifier >> 32),
        EGL_NONE};
    EGLImage image = eglCreateImage(egl, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL, attributes);
    if (!image) { fprintf(stderr, "eglCreateImage layer %d fourcc %.4s: %#x\n", layer, (char *)&plane_fourcc, eglGetError()); exit(1); }
    GLuint texture, framebuffer;
    glGenTextures(1, &texture);
    glBindTexture(GL_TEXTURE_2D, texture);
    target_texture(GL_TEXTURE_2D, (GLeglImageOES)image);
    glGenFramebuffers(1, &framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, texture, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) { fprintf(stderr, "framebuffer incomplete layer %d\n", layer); exit(1); }
    glPixelStorei(GL_PACK_ALIGNMENT, 1);
    glReadPixels(0, 0, w, h, channels == 1 ? GL_RED : GL_RG, bytes == 1 ? GL_UNSIGNED_BYTE : GL_UNSIGNED_SHORT, out);
    if (glGetError()) { fprintf(stderr, "glReadPixels layer %d failed\n", layer); exit(1); }
    glDeleteFramebuffers(1, &framebuffer);
    glDeleteTextures(1, &texture);
    eglDestroyImage(egl, image);
}

int main(int argc, char **argv) {
    if (argc < 4) return 2;
    const char *name = argv[3];
    int limit = argc > 4 ? atoi(argv[4]) : 3;
    unsigned fourcc = VA_FOURCC(name[0], name[1], name[2], name[3]);
    int bytes = (fourcc == VA_FOURCC_NV12 || fourcc == VA_FOURCC_444P) ? 1 : 2;
    int full = fourcc == VA_FOURCC_Q416 || fourcc == VA_FOURCC_444P;

    int fd = open(argv[1], O_RDWR);
    struct gbm_device *gbm = gbm_create_device(fd);
    PFNEGLGETPLATFORMDISPLAYEXTPROC get_display = (void *)eglGetProcAddress("eglGetPlatformDisplayEXT");
    EGLDisplay egl = get_display(EGL_PLATFORM_GBM_KHR, gbm, NULL);
    EGLint major, minor;
    if (!eglInitialize(egl, &major, &minor)) { fprintf(stderr, "eglInitialize failed\n"); return 1; }
    eglBindAPI(EGL_OPENGL_ES_API);
    EGLint config_attributes[] = {EGL_RENDERABLE_TYPE, EGL_OPENGL_ES3_BIT, EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_NONE};
    EGLConfig config; EGLint n;
    eglChooseConfig(egl, config_attributes, &config, 1, &n);
    EGLint context_attributes[] = {EGL_CONTEXT_CLIENT_VERSION, 3, EGL_NONE};
    EGLContext context = eglCreateContext(egl, config, EGL_NO_CONTEXT, context_attributes);
    EGLint pbuffer_attributes[] = {EGL_WIDTH, 16, EGL_HEIGHT, 16, EGL_NONE};
    eglMakeCurrent(egl, eglCreatePbufferSurface(egl, config, pbuffer_attributes), eglCreatePbufferSurface(egl, config, pbuffer_attributes), context);
    target_texture = (void *)eglGetProcAddress("glEGLImageTargetTexture2DOES");
    fprintf(stderr, "EGL %d.%d renderer %s\n", major, minor, glGetString(GL_RENDERER));

    frames_format = fourcc == VA_FOURCC_NV12 ? AV_PIX_FMT_NV12 : fourcc == VA_FOURCC_P010 ? AV_PIX_FMT_P010 :
                    fourcc == VA_FOURCC_P012 ? AV_PIX_FMT_P012 : AV_PIX_FMT_YUV444P;
    AVFormatContext *fmt = NULL;
    if (avformat_open_input(&fmt, argv[2], NULL, NULL) < 0 || avformat_find_stream_info(fmt, NULL) < 0) return 1;
    int idx = av_find_best_stream(fmt, AVMEDIA_TYPE_VIDEO, -1, -1, NULL, 0);
    enum AVCodecID id = fmt->streams[idx]->codecpar->codec_id;
    const AVCodec *codec = id == AV_CODEC_ID_AV1 ? avcodec_find_decoder_by_name("av1") : avcodec_find_decoder(id);
    AVCodecContext *dec = avcodec_alloc_context3(codec);
    avcodec_parameters_to_context(dec, fmt->streams[idx]->codecpar);
    int twelve = dec->pix_fmt == AV_PIX_FMT_YUV444P12LE || dec->pix_fmt == AV_PIX_FMT_YUV420P12LE;
    render_target_format = fourcc == VA_FOURCC_NV12 ? VA_RT_FORMAT_YUV420 : fourcc == VA_FOURCC_P010 ? VA_RT_FORMAT_YUV420_10 :
                           fourcc == VA_FOURCC_P012 ? VA_RT_FORMAT_YUV420_12 : fourcc == VA_FOURCC_444P ? VA_RT_FORMAT_YUV444 :
                           twelve ? VA_RT_FORMAT_YUV444_12 : VA_RT_FORMAT_YUV444_10;
    AVBufferRef *dev = NULL;
    if (av_hwdevice_ctx_create(&dev, AV_HWDEVICE_TYPE_VAAPI, argv[1], NULL, 0) < 0) return 1;
    dec->hw_device_ctx = av_buffer_ref(dev);
    dec->get_format = pick;
    if (avcodec_open2(dec, codec, NULL) < 0) return 1;
    VADisplay d = display = ((AVVAAPIDeviceContext *)((AVHWDeviceContext *)dev->data)->hwctx)->display;

    AVPacket *pkt = av_packet_alloc();
    AVFrame *frame = av_frame_alloc();
    int frames = 0, bad = 0;
    while (frames < limit && av_read_frame(fmt, pkt) >= 0) {
        if (pkt->stream_index == idx) avcodec_send_packet(dec, pkt);
        av_packet_unref(pkt);
        while (frames < limit && avcodec_receive_frame(dec, frame) == 0) {
            VASurfaceID surface = (VASurfaceID)(uintptr_t)frame->data[3];
            int w = frame->width, h = frame->height;
            CHECK(vaSyncSurface(d, surface));
            VADRMPRIMESurfaceDescriptor desc;
            CHECK(vaExportSurfaceHandle(d, surface, VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2, VA_EXPORT_SURFACE_READ_ONLY | VA_EXPORT_SURFACE_SEPARATE_LAYERS, &desc));
            CHECK(vaSyncSurface(d, surface));
            VAImage image;
            VAImageFormat format = {.fourcc = fourcc, .byte_order = VA_LSB_FIRST};
            CHECK(vaCreateImage(d, &format, w, h, &image));
            CHECK(vaGetImage(d, surface, 0, 0, w, h, image.image_id));
            unsigned char *base;
            CHECK(vaMapBuffer(d, image.buf, (void **)&base));
            if (frames == 0)
                fprintf(stderr, "descriptor fourcc %.4s %ux%u objects %u layers %u modifier %#llx\n", (char *)&desc.fourcc, desc.width, desc.height,
                        desc.num_objects, desc.num_layers, (unsigned long long)desc.objects[0].drm_format_modifier);
            int planes = full ? 3 : 2;
            if ((int)desc.num_layers != planes) { fprintf(stderr, "expected %d layers, got %u\n", planes, desc.num_layers); return 1; }
            for (int plane = 0; plane < planes; plane++) {
                int channels = plane == 1 && !full ? 2 : 1;
                int pw = plane == 0 || full ? w : w / 2, ph = plane == 0 || full ? h : h / 2;
                unsigned char *got = malloc((size_t)pw * ph * channels * bytes);
                read_plane(egl, &desc, plane, pw, ph, channels, bytes, got);
                for (int row = 0; row < ph; row++)
                    if (memcmp(got + (size_t)row * pw * channels * bytes, base + image.offsets[plane] + (size_t)row * image.pitches[plane], (size_t)pw * channels * bytes)) {
                        fprintf(stderr, "frame %d plane %d row %d differs\n", frames, plane, row);
                        bad = 1;
                        break;
                    }
                free(got);
            }
            CHECK(vaUnmapBuffer(d, image.buf));
            CHECK(vaDestroyImage(d, image.image_id));
            for (uint32_t i = 0; i < desc.num_objects; i++) close(desc.objects[i].fd);
            av_frame_unref(frame);
            frames++;
        }
    }
    printf("%s: %d frames, %s\n", name, frames, bad ? "MISMATCH" : "EGL readback equals vaGetImage");
    return bad || !frames;
}
