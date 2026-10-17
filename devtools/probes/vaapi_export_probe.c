/* 探针：验证 i965/Haswell 能否把 NV12 解码表面导出为 DRM PRIME dmabuf。
 *
 * 用途：确认零拷贝方案的门槛是否成立。不是产品代码。
 * 编译：gcc -O2 -o vaapi_export_probe vaapi_export_probe.c -lva -lva-drm
 */
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <va/va.h>
#include <va/va_drm.h>
#include <va/va_drmcommon.h>

/* va.h 提供 VA_FOURCC_NV12，不依赖 libdrm 头文件。 */
#ifndef VA_FOURCC_NV12
#define VA_FOURCC_NV12 0x3231564e
#endif

static const char *fourcc_str(uint32_t f, char out[5]) {
    out[0] = (char)(f & 0xff);
    out[1] = (char)((f >> 8) & 0xff);
    out[2] = (char)((f >> 16) & 0xff);
    out[3] = (char)((f >> 24) & 0xff);
    out[4] = 0;
    return out;
}

int main(int argc, char **argv) {
    const char *path = argc > 1 ? argv[1] : "/dev/dri/renderD128";
    unsigned width = argc > 2 ? (unsigned)atoi(argv[2]) : 1920;
    unsigned height = argc > 3 ? (unsigned)atoi(argv[3]) : 1080;

    int fd = open(path, O_RDWR);
    if (fd < 0) { perror("open render node"); return 1; }

    VADisplay dpy = vaGetDisplayDRM(fd);
    if (!dpy) { fprintf(stderr, "vaGetDisplayDRM failed\n"); return 1; }

    int major = 0, minor = 0;
    if (vaInitialize(dpy, &major, &minor) != VA_STATUS_SUCCESS) {
        fprintf(stderr, "vaInitialize failed\n"); return 1;
    }
    printf("VA version %d.%d on %s\n", major, minor, path);

    /* 查驱动是否支持 H.264 VLD（零拷贝需要真实解码表面）。 */
    VAConfigID config = VA_INVALID_ID;
    VAEntrypoint entry = VAEntrypointVLD;
    if (vaCreateConfig(dpy, VAProfileH264Main, VAEntrypointVLD, NULL, 0, &config)
        != VA_STATUS_SUCCESS) {
        fprintf(stderr, "H.264 Main/VLD 不可用，退回 VAProfileNone\n");
        entry = VAEntrypointVideoProc;
        if (vaCreateConfig(dpy, VAProfileNone, VAEntrypointVideoProc, NULL, 0, &config)
            != VA_STATUS_SUCCESS) {
            fprintf(stderr, "vaCreateConfig failed\n"); return 1;
        }
    }
    printf("config=%u entrypoint=%d\n", config, entry);

    /* 用 EXPORT 提示创建表面：驱动据此分配可导出的 backing。 */
    VASurfaceID surface = VA_INVALID_ID;
    VASurfaceAttrib export_attrib = {
        .type = VASurfaceAttribMemoryType,
        .flags = VA_SURFACE_ATTRIB_SETTABLE,
        .value.type = VAGenericValueTypeInteger,
        .value.value.i = VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
    };
    if (vaCreateSurfaces(dpy, VA_RT_FORMAT_YUV420, width, height, &surface, 1,
                         &export_attrib, 1) != VA_STATUS_SUCCESS) {
        fprintf(stderr, "vaCreateSurfaces(PRIME_2) 失败，尝试无属性创建\n");
        if (vaCreateSurfaces(dpy, VA_RT_FORMAT_YUV420, width, height, &surface, 1,
                             NULL, 0) != VA_STATUS_SUCCESS) {
            fprintf(stderr, "vaCreateSurfaces failed\n"); return 1;
        }
    }
    printf("surface=%u (%ux%u)\n", surface, width, height);

    VADRMPRIMESurfaceDescriptor desc = {0};
    VAStatus st = vaExportSurfaceHandle(
        dpy, surface, VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
        VA_EXPORT_SURFACE_READ_ONLY | VA_EXPORT_SURFACE_COMPOSED_LAYERS, &desc);
    if (st != VA_STATUS_SUCCESS) {
        fprintf(stderr, "vaExportSurfaceHandle: %s\n", vaErrorStr(st));
        fprintf(stderr, "=> 该驱动不支持 dmabuf 导出，零拷贝方案不可行\n");
        return 2;
    }

    char tag[5];
    printf("\n=== 导出成功 ===\n");
    printf("fourcc=%s width=%u height=%u num_objects=%u num_layers=%u\n",
           fourcc_str(desc.fourcc, tag), desc.width, desc.height,
           desc.num_objects, desc.num_layers);
    for (unsigned i = 0; i < desc.num_objects && i < 4; i++) {
        printf("  object[%u]: fd=%d size=%llu modifier=0x%llx\n", i,
               desc.objects[i].fd, (unsigned long long)desc.objects[i].size,
               (unsigned long long)desc.objects[i].drm_format_modifier);
    }
    for (unsigned l = 0; l < desc.num_layers && l < 4; l++) {
        /* C 头文件里 layers 是匿名结构体，用 __typeof__ 取其真实类型。 */
        __typeof__(desc.layers[0]) *ly = &desc.layers[l];
        printf("  layer[%u]: drm_format=%s num_planes=%u\n", l,
               fourcc_str(ly->drm_format, tag), ly->num_planes);
        for (unsigned p = 0; p < ly->num_planes && p < 4; p++) {
            printf("    plane[%u]: object_index=%u offset=%u pitch=%u\n", p,
                   ly->object_index[p], ly->offset[p], ly->pitch[p]);
        }
    }

    /* 对照：NV12 期望 YUV420 fourcc 与两个平面。 */
    printf("\n=== 判定 ===\n");
    printf("fourcc NV12: %s\n", desc.fourcc == VA_FOURCC_NV12 ? "YES" : "NO");
    printf("单 object 双平面（可一次导入）: %s\n",
           desc.num_objects == 1 && desc.num_layers == 1
               && desc.layers[0].num_planes == 2 ? "YES" : "NO");

    vaTerminate(dpy);
    close(fd);
    return 0;
}
