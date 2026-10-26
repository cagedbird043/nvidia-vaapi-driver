// LD_PRELOAD shim: every VLD config is created with RTFormat = YUV420, as Chrome does for 10-bit
// AV1 / VP9 profile 2 before it creates 10-bit surfaces. FFmpeg passes no attributes at all.
#define _GNU_SOURCE
#include <dlfcn.h>
#include <va/va.h>

VAStatus vaCreateConfig(VADisplay display, VAProfile profile, VAEntrypoint entrypoint, VAConfigAttrib *attributes, int count, VAConfigID *config) {
    static VAStatus (*real)(VADisplay, VAProfile, VAEntrypoint, VAConfigAttrib *, int, VAConfigID *);
    if (!real) real = dlsym(RTLD_NEXT, "vaCreateConfig");
    VAConfigAttrib hint = {.type = VAConfigAttribRTFormat, .value = VA_RT_FORMAT_YUV420};
    if (entrypoint == VAEntrypointVLD && count == 0) return real(display, profile, entrypoint, &hint, 1, config);
    return real(display, profile, entrypoint, attributes, count, config);
}
