/* 决定性探针：VA-API 导出 dmabuf 后，用 Vulkan 将其导入为两个单平面 image。
 *
 * 这验证零拷贝方案的第二道门槛：HasVK 能否接受 i965 导出的 DRM modifier。
 * 流程与 wgpu-hal 的 texture_from_dmabuf_fd 完全一致：
 *   ExternalMemoryImageCreateInfo(DMA_BUF) + ImageDrmFormatModifierExplicitCreateInfoEXT
 *   + Tiling::DRM_FORMAT_MODIFIER + ImportMemoryFdInfoKHR。
 *
 * 编译：gcc -O2 -o vulkan_dmabuf_probe vulkan_dmabuf_probe.c \
 *         -lva -lva-drm -lvulkan
 */
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <va/va.h>
#include <va/va_drm.h>
#include <va/va_drmcommon.h>
#include <vulkan/vulkan.h>

#ifndef VA_FOURCC_NV12
#define VA_FOURCC_NV12 0x3231564e
#endif

#define CHECK_VK(expr)                                                        \
    do {                                                                      \
        VkResult _r = (expr);                                                 \
        if (_r != VK_SUCCESS) {                                               \
            fprintf(stderr, "VK 失败 %s -> VkResult %d\n", #expr, (int)_r);   \
            return 1;                                                         \
        }                                                                     \
    } while (0)

/* 与 wgpu-hal 一致的四元组。 */
struct PlaneImport {
    const char *name;
    VkFormat format;
    uint32_t width;
    uint32_t height;
    uint64_t offset;
    uint64_t stride;
};

static int import_plane(VkDevice dev, VkPhysicalDevice phys, uint32_t qfi,
                        int dmabuf_fd, uint64_t modifier,
                        const struct PlaneImport *p) {
    VkPhysicalDeviceMemoryProperties memprops;
    vkGetPhysicalDeviceMemoryProperties(phys, &memprops);

    /* 复制 fd：Vulkan 在 vkAllocateMemory 成功后接管 fd。 */
    int dup_fd = dup(dmabuf_fd);
    if (dup_fd < 0) { perror("dup"); return 1; }

    VkExternalMemoryImageCreateInfo ext = {
        .sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
        .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
    };

    VkSubresourceLayout plane_layout = {
        .offset = p->offset,
        .rowPitch = p->stride,
    };
    VkImageDrmFormatModifierExplicitCreateInfoEXT mod = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_EXPLICIT_CREATE_INFO_EXT,
        .pNext = &ext,
        .drmFormatModifier = modifier,
        .drmFormatModifierPlaneCount = 1,
        .pPlaneLayouts = &plane_layout,
    };

    VkImageCreateInfo ici = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
        .pNext = &mod,
        .flags = 0,
        .imageType = VK_IMAGE_TYPE_2D,
        .format = p->format,
        .extent = { p->width, p->height, 1 },
        .mipLevels = 1,
        .arrayLayers = 1,
        .samples = VK_SAMPLE_COUNT_1_BIT,
        .tiling = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT,
        .usage = VK_IMAGE_USAGE_SAMPLED_BIT,
        .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
        .queueFamilyIndexCount = 1,
        .pQueueFamilyIndices = &qfi,
        .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED,
    };

    VkImage image;
    CHECK_VK(vkCreateImage(dev, &ici, NULL, &image));

    VkMemoryRequirements reqs;
    vkGetImageMemoryRequirements(dev, image, &reqs);

    /* 按 reqs.typeBits 与 device-local 求交；找不到再退回任意可用类型。 */
    int chosen = -1;
    for (uint32_t i = 0; i < memprops.memoryTypeCount; i++) {
        if (!(reqs.memoryTypeBits & (1u << i))) continue;
        if (!(memprops.memoryTypes[i].propertyFlags & VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT)) continue;
        chosen = (int)i;
        break;
    }
    if (chosen < 0) {
        for (uint32_t i = 0; i < memprops.memoryTypeCount; i++) {
            if (reqs.memoryTypeBits & (1u << i)) { chosen = (int)i; break; }
        }
    }
    if (chosen < 0) {
        fprintf(stderr, "  [%s] 找不到合适的内存类型 (bits=0x%x)\n",
                p->name, reqs.memoryTypeBits);
        vkDestroyImage(dev, image, NULL);
        close(dup_fd);
        return 1;
    }

    VkImportMemoryFdInfoKHR import = {
        .sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR,
        .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
        .fd = dup_fd,
    };
    VkMemoryAllocateInfo alloc = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        .pNext = &import,
        .allocationSize = reqs.size,
        .memoryTypeIndex = (uint32_t)chosen,
    };

    VkDeviceMemory memory;
    VkResult r = vkAllocateMemory(dev, &alloc, NULL, &memory);
    if (r != VK_SUCCESS) {
        fprintf(stderr, "  [%s] 导入 dmabuf 失败 VkResult=%d\n", p->name, (int)r);
        vkDestroyImage(dev, image, NULL);
        /* 分配失败时 Vulkan 不接管 fd，需要自己关。 */
        close(dup_fd);
        return 1;
    }
    /* 成功后 fd 归 Vulkan 所有，不要 close。 */

    CHECK_VK(vkBindImageMemory(dev, image, memory, 0));

    printf("  [%s] 导入成功 format=%d %ux%u offset=%llu stride=%llu mem=%llu bytes\n",
           p->name, (int)p->format, p->width, p->height,
           (unsigned long long)p->offset, (unsigned long long)p->stride,
           (unsigned long long)reqs.size);

    /* 测试用，用完即释放。 */
    vkDestroyImage(dev, image, NULL);
    vkFreeMemory(dev, memory, NULL);
    return 0;
}

int main(int argc, char **argv) {
    const char *path = argc > 1 ? argv[1] : "/dev/dri/renderD128";
    unsigned width = argc > 2 ? (unsigned)atoi(argv[2]) : 1920;
    unsigned height = argc > 3 ? (unsigned)atoi(argv[3]) : 1080;

    int va_fd = open(path, O_RDWR);
    if (va_fd < 0) { perror("open"); return 1; }
    VADisplay dpy = vaGetDisplayDRM(va_fd);
    if (!dpy) { fprintf(stderr, "vaGetDisplayDRM failed\n"); return 1; }
    int major, minor;
    if (vaInitialize(dpy, &major, &minor) != VA_STATUS_SUCCESS) return 1;

    VAConfigID config;
    if (vaCreateConfig(dpy, VAProfileNone, VAEntrypointVideoProc, NULL, 0, &config)
        != VA_STATUS_SUCCESS) { fprintf(stderr, "config failed\n"); return 1; }

    VASurfaceAttrib attrib = {
        .type = VASurfaceAttribMemoryType,
        .flags = VA_SURFACE_ATTRIB_SETTABLE,
        .value.type = VAGenericValueTypeInteger,
        .value.value.i = VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
    };
    VASurfaceID surface;
    if (vaCreateSurfaces(dpy, VA_RT_FORMAT_YUV420, width, height, &surface, 1,
                         &attrib, 1) != VA_STATUS_SUCCESS) {
        fprintf(stderr, "create_surfaces failed\n"); return 1;
    }

    VADRMPRIMESurfaceDescriptor desc = {0};
    if (vaExportSurfaceHandle(dpy, surface,
                              VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
                              VA_EXPORT_SURFACE_READ_ONLY | VA_EXPORT_SURFACE_COMPOSED_LAYERS,
                              &desc) != VA_STATUS_SUCCESS) {
        fprintf(stderr, "vaExportSurfaceHandle failed\n"); return 1;
    }
    if (desc.num_objects < 1 || desc.num_layers < 1 || desc.layers[0].num_planes < 2) {
        fprintf(stderr, "意外的布局: objects=%u layers=%u planes=%u\n",
                desc.num_objects, desc.num_layers,
                desc.num_layers ? desc.layers[0].num_planes : 0);
        return 1;
    }
    int dmabuf = desc.objects[0].fd;
    uint64_t modifier = desc.objects[0].drm_format_modifier;
    uint32_t luma_pitch = desc.layers[0].pitch[0];
    uint32_t chroma_pitch = desc.layers[0].pitch[1];
    uint32_t chroma_offset = desc.layers[0].offset[1];

    printf("导出: modifier=0x%llx y_pitch=%u uv_pitch=%u uv_offset=%u\n",
           (unsigned long long)modifier, luma_pitch, chroma_pitch, chroma_offset);

    /* ---- Vulkan：选 Intel 适配器 ---- */
    VkInstanceCreateInfo inst_ci = {
        .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
        .enabledExtensionCount = 0,
    };
    VkInstance instance;
    CHECK_VK(vkCreateInstance(&inst_ci, NULL, &instance));

    uint32_t ndev = 0;
    vkEnumeratePhysicalDevices(instance, &ndev, NULL);
    VkPhysicalDevice *devs = calloc(ndev, sizeof(*devs));
    vkEnumeratePhysicalDevices(instance, &ndev, devs);

    VkPhysicalDevice phys = VK_NULL_HANDLE;
    for (uint32_t i = 0; i < ndev; i++) {
        VkPhysicalDeviceProperties props;
        vkGetPhysicalDeviceProperties(devs[i], &props);
        printf("  GPU%u: %s (vendor=0x%x)\n", i, props.deviceName, props.vendorID);
        if (props.vendorID == 0x8086) { phys = devs[i]; }
    }
    if (phys == VK_NULL_HANDLE) { fprintf(stderr, "没有 Intel 适配器\n"); return 1; }

    /* 确认扩展支持。 */
    uint32_t nex = 0;
    vkEnumerateDeviceExtensionProperties(phys, NULL, &nex, NULL);
    VkExtensionProperties *exts = calloc(nex, sizeof(*exts));
    vkEnumerateDeviceExtensionProperties(phys, NULL, &nex, exts);
    int has_dmabuf = 0, has_modifier = 0, has_ext_fd = 0;
    for (uint32_t i = 0; i < nex; i++) {
        if (!strcmp(exts[i].extensionName, VK_EXT_EXTERNAL_MEMORY_DMA_BUF_EXTENSION_NAME)) has_dmabuf = 1;
        if (!strcmp(exts[i].extensionName, VK_EXT_IMAGE_DRM_FORMAT_MODIFIER_EXTENSION_NAME)) has_modifier = 1;
        if (!strcmp(exts[i].extensionName, VK_KHR_EXTERNAL_MEMORY_FD_EXTENSION_NAME)) has_ext_fd = 1;
    }
    printf("扩展: dma_buf=%d drm_format_modifier=%d external_memory_fd=%d\n",
           has_dmabuf, has_modifier, has_ext_fd);
    if (!has_dmabuf || !has_modifier || !has_ext_fd) {
        fprintf(stderr, "=> 缺少必要扩展，无法导入\n"); return 2;
    }

    const char *dev_exts[] = {
        VK_EXT_EXTERNAL_MEMORY_DMA_BUF_EXTENSION_NAME,
        VK_EXT_IMAGE_DRM_FORMAT_MODIFIER_EXTENSION_NAME,
        VK_KHR_EXTERNAL_MEMORY_FD_EXTENSION_NAME,
    };
    float prio = 1.0f;
    VkDeviceQueueCreateInfo qci = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
        .queueFamilyIndex = 0,
        .queueCount = 1,
        .pQueuePriorities = &prio,
    };
    VkDeviceCreateInfo dci = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
        .queueCreateInfoCount = 1,
        .pQueueCreateInfos = &qci,
        .enabledExtensionCount = 3,
        .ppEnabledExtensionNames = dev_exts,
    };
    VkDevice dev;
    CHECK_VK(vkCreateDevice(phys, &dci, NULL, &dev));
    printf("已创建 Vulkan 设备\n\n");

    /* ---- 两个单平面导入（NV12 拆 Y/UV）---- */
    struct PlaneImport planes[2] = {
        { .name = "Y",  .format = VK_FORMAT_R8_UNORM,
          .width = width, .height = height,
          .offset = desc.layers[0].offset[0], .stride = luma_pitch },
        { .name = "UV", .format = VK_FORMAT_R8G8_UNORM,
          .width = width / 2, .height = height / 2,
          .offset = chroma_offset, .stride = chroma_pitch },
    };

    int failures = 0;
    for (int i = 0; i < 2; i++) {
        failures += import_plane(dev, phys, 0, dmabuf, modifier, &planes[i]);
    }

    printf("\n=== 判定 ===\n");
    printf("Vulkan 导入 dmabuf: %s\n", failures == 0 ? "全部成功" : "存在失败");

    vkDestroyDevice(dev, NULL);
    vkDestroyInstance(instance, NULL);
    vaTerminate(dpy);
    return failures == 0 ? 0 : 3;
}
