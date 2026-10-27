// One VA VideoProc pipeline from a raw input file into a raw output file, for tests/vpp.py.
//
//   vpp_probe DEVICE IN_FOURCC W H IN_FILE OUT_FOURCC W H OUT_FILE [key=value ...]
//
// Raw files are tightly packed planes (pitch = width x bytes per sample; 4:2:0 chroma interleaved at
// half height; 4:4:4 three planes; RGB one plane of 4-byte pixels). The input is uploaded with
// vaPutImage, the output read back with vaGetImage. Keys:
//   src=x,y,w,h dst=x,y,w,h      surface_region / output_region
//   bg=0xAARRGGBB                output_background_color
//   in_std=N out_std=N           VAProcColorStandardType
//   in_props=loc,range,prim,trc,mat  out_props=...   VAProcColorProperties
//   filter=1                     also render a VAProcFilterParameterBuffer
//   filters=1                    set num_filters = 1 in the pipeline
// Prints "FAIL <call> <status>" and exits 1 on the first failing call, "OK" otherwise.
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <va/va.h>
#include <va/va_drm.h>
#include <va/va_vpp.h>

#define CHECK(call)                                                   \
    do {                                                              \
        VAStatus s_ = (call);                                         \
        if (s_ != VA_STATUS_SUCCESS) {                                \
            printf("FAIL %s %#x\n", #call, s_);                       \
            exit(1);                                                  \
        }                                                             \
    } while (0)

struct layout {
    unsigned rt, planes, bps, rows[3], line[3];
};

static struct layout layout_of(unsigned fourcc, unsigned w, unsigned h)
{
    struct layout l = {0};
    switch (fourcc) {
    case VA_FOURCC_NV12: l = (struct layout){VA_RT_FORMAT_YUV420, 2, 1}; break;
    case VA_FOURCC_P010: l = (struct layout){VA_RT_FORMAT_YUV420_10, 2, 2}; break;
    case VA_FOURCC_P012: l = (struct layout){VA_RT_FORMAT_YUV420_12, 2, 2}; break;
    case VA_FOURCC_444P: l = (struct layout){VA_RT_FORMAT_YUV444, 3, 1}; break;
    case VA_FOURCC_Q416: l = (struct layout){VA_RT_FORMAT_YUV444_10, 3, 2}; break;
    default: l = (struct layout){VA_RT_FORMAT_RGB32, 1, 4}; break;
    }
    for (unsigned p = 0; p < l.planes; p++) {
        l.line[p] = w * l.bps;
        l.rows[p] = (l.planes == 2 && p == 1) ? h / 2 : h;
    }
    return l;
}

static unsigned fourcc_of(const char *s)
{
    return VA_FOURCC(s[0], s[1], s[2], s[3]);
}

static VASurfaceID surface(VADisplay dpy, unsigned fourcc, unsigned w, unsigned h)
{
    VASurfaceAttrib a = {.type = VASurfaceAttribPixelFormat, .flags = VA_SURFACE_ATTRIB_SETTABLE,
                         .value = {.type = VAGenericValueTypeInteger, .value.i = (int)fourcc}};
    VASurfaceID s;
    CHECK(vaCreateSurfaces(dpy, layout_of(fourcc, w, h).rt, w, h, &s, 1, &a, 1));
    return s;
}

static void transfer(VADisplay dpy, VASurfaceID s, unsigned fourcc, unsigned w, unsigned h, const char *path, int upload)
{
    struct layout l = layout_of(fourcc, w, h);
    VAImageFormat format = {.fourcc = fourcc, .byte_order = VA_LSB_FIRST};
    VAImage image;
    CHECK(vaCreateImage(dpy, &format, w, h, &image));
    FILE *f = fopen(path, upload ? "rb" : "wb");
    if (!f) {
        perror(path);
        exit(2);
    }
    if (!upload)
        CHECK(vaGetImage(dpy, s, 0, 0, w, h, image.image_id));
    unsigned char *data;
    CHECK(vaMapBuffer(dpy, image.buf, (void **)&data));
    for (unsigned p = 0; p < l.planes; p++)
        for (unsigned y = 0; y < l.rows[p]; y++) {
            unsigned char *row = data + image.offsets[p] + (size_t)y * image.pitches[p];
            size_t n = upload ? fread(row, 1, l.line[p], f) : fwrite(row, 1, l.line[p], f);
            if (n != l.line[p]) {
                fprintf(stderr, "%s: short %s\n", path, upload ? "read" : "write");
                exit(2);
            }
        }
    CHECK(vaUnmapBuffer(dpy, image.buf));
    if (upload)
        CHECK(vaPutImage(dpy, s, image.image_id, 0, 0, w, h, 0, 0, w, h));
    fclose(f);
    CHECK(vaDestroyImage(dpy, image.image_id));
}

static void props(const char *v, VAProcColorProperties *p)
{
    unsigned loc, range, prim, trc, mat;
    if (sscanf(v, "%u,%u,%u,%u,%u", &loc, &range, &prim, &trc, &mat) != 5) {
        fprintf(stderr, "bad colour properties %s\n", v);
        exit(2);
    }
    *p = (VAProcColorProperties){.chroma_sample_location = loc, .color_range = range, .colour_primaries = prim,
                                 .transfer_characteristics = trc, .matrix_coefficients = mat};
}

int main(int argc, char **argv)
{
    if (argc < 10) {
        fprintf(stderr, "usage: %s DEVICE IN_FOURCC W H IN_FILE OUT_FOURCC W H OUT_FILE [key=value ...]\n", argv[0]);
        return 2;
    }
    unsigned in_fourcc = fourcc_of(argv[2]), iw = atoi(argv[3]), ih = atoi(argv[4]);
    unsigned out_fourcc = fourcc_of(argv[6]), ow = atoi(argv[7]), oh = atoi(argv[8]);
    VARectangle src, dst;
    VAProcPipelineParameterBuffer pipe;
    memset(&pipe, 0, sizeof(pipe));
    pipe.output_background_color = 0xff000000;
    int filter = 0;
    for (int i = 10; i < argc; i++) {
        char *v = strchr(argv[i], '=');
        if (!v) {
            fprintf(stderr, "bad option %s\n", argv[i]);
            return 2;
        }
        *v++ = 0;
        int x, y, w, h;
        if (!strcmp(argv[i], "src") && sscanf(v, "%d,%d,%d,%d", &x, &y, &w, &h) == 4) {
            src = (VARectangle){x, y, w, h};
            pipe.surface_region = &src;
        } else if (!strcmp(argv[i], "dst") && sscanf(v, "%d,%d,%d,%d", &x, &y, &w, &h) == 4) {
            dst = (VARectangle){x, y, w, h};
            pipe.output_region = &dst;
        } else if (!strcmp(argv[i], "bg")) {
            pipe.output_background_color = strtoul(v, NULL, 0);
        } else if (!strcmp(argv[i], "in_std")) {
            pipe.surface_color_standard = atoi(v);
        } else if (!strcmp(argv[i], "out_std")) {
            pipe.output_color_standard = atoi(v);
        } else if (!strcmp(argv[i], "in_props")) {
            props(v, &pipe.input_color_properties);
        } else if (!strcmp(argv[i], "out_props")) {
            props(v, &pipe.output_color_properties);
        } else if (!strcmp(argv[i], "filter")) {
            filter = atoi(v);
        } else if (!strcmp(argv[i], "filters")) {
            pipe.num_filters = atoi(v);
        } else {
            fprintf(stderr, "bad option %s\n", argv[i]);
            return 2;
        }
    }
    int fd = open(argv[1], O_RDWR);
    if (fd < 0) {
        perror(argv[1]);
        return 2;
    }
    VADisplay dpy = vaGetDisplayDRM(fd);
    int major, minor;
    CHECK(vaInitialize(dpy, &major, &minor));
    VAConfigID config;
    CHECK(vaCreateConfig(dpy, VAProfileNone, VAEntrypointVideoProc, NULL, 0, &config));
    VASurfaceID in = surface(dpy, in_fourcc, iw, ih), out = surface(dpy, out_fourcc, ow, oh);
    transfer(dpy, in, in_fourcc, iw, ih, argv[5], 1);
    VAContextID context;
    CHECK(vaCreateContext(dpy, config, ow, oh, VA_PROGRESSIVE, &out, 1, &context));
    pipe.surface = in;
    VABufferID buffers[2];
    unsigned n = 0;
    CHECK(vaCreateBuffer(dpy, context, VAProcPipelineParameterBufferType, sizeof(pipe), 1, &pipe, &buffers[n++]));
    if (filter) {
        VAProcFilterParameterBuffer denoise = {.type = VAProcFilterNoiseReduction};
        CHECK(vaCreateBuffer(dpy, context, VAProcFilterParameterBufferType, sizeof(denoise), 1, &denoise, &buffers[n++]));
    }
    CHECK(vaBeginPicture(dpy, context, out));
    VAStatus render = vaRenderPicture(dpy, context, buffers, n);
    if (render != VA_STATUS_SUCCESS) {
        printf("FAIL vaRenderPicture %#x\n", render);
        return 1;
    }
    CHECK(vaEndPicture(dpy, context));
    CHECK(vaSyncSurface(dpy, out));
    transfer(dpy, out, out_fourcc, ow, oh, argv[9], 0);
    vaDestroyContext(dpy, context);
    vaDestroySurfaces(dpy, &in, 1);
    vaDestroySurfaces(dpy, &out, 1);
    vaDestroyConfig(dpy, config);
    vaTerminate(dpy);
    close(fd);
    printf("OK\n");
    return 0;
}
