#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <vulkan/vulkan.h>

/* When started from init on Redox, stdout has no useful sink. Mirror it to
 * the kernel debug console so the result is visible on the framebuffer. */
static void attach_debug_console(void) {
    static const char *const paths[] = {
        "/scheme/debug/no-preserve",
        "/dev/console",
    };
    for (size_t i = 0; i < sizeof(paths) / sizeof(paths[0]); i++) {
        FILE *stream = fopen(paths[i], "w");
        if (!stream) continue;
        setvbuf(stream, NULL, _IONBF, 0);
        dup2(fileno(stream), STDOUT_FILENO);
        dup2(fileno(stream), STDERR_FILENO);
        printf("vulkan-smoketest: output attached to %s\n", paths[i]);
        return;
    }
}

static const char *result_name(VkResult r) {
    switch (r) {
        case VK_SUCCESS: return "VK_SUCCESS";
        case VK_ERROR_INCOMPATIBLE_DRIVER: return "VK_ERROR_INCOMPATIBLE_DRIVER";
        case VK_ERROR_OUT_OF_HOST_MEMORY: return "VK_ERROR_OUT_OF_HOST_MEMORY";
        case VK_ERROR_EXTENSION_NOT_PRESENT: return "VK_ERROR_EXTENSION_NOT_PRESENT";
        case VK_ERROR_INITIALIZATION_FAILED: return "VK_ERROR_INITIALIZATION_FAILED";
        default: return "VK_ERROR_<other>";
    }
}

int main(void) {
    attach_debug_console();

    void *lib = dlopen("libvulkan.so.1", RTLD_NOW | RTLD_LOCAL);
    if (!lib) {
        fprintf(stderr, "dlopen(libvulkan.so.1) failed: %s\n", dlerror());
        return 1;
    }
    printf("loader: libvulkan.so.1 opened\n");

    PFN_vkGetInstanceProcAddr gipa =
        (PFN_vkGetInstanceProcAddr)dlsym(lib, "vkGetInstanceProcAddr");
    if (!gipa) {
        fprintf(stderr, "vkGetInstanceProcAddr missing\n");
        return 1;
    }

    PFN_vkEnumerateInstanceVersion enum_version =
        (PFN_vkEnumerateInstanceVersion)gipa(NULL, "vkEnumerateInstanceVersion");
    PFN_vkCreateInstance create_instance =
        (PFN_vkCreateInstance)gipa(NULL, "vkCreateInstance");
    if (!create_instance) {
        fprintf(stderr, "vkCreateInstance missing\n");
        return 1;
    }

    uint32_t api = VK_API_VERSION_1_0;
    if (enum_version) {
        VkResult r = enum_version(&api);
        printf("vkEnumerateInstanceVersion: %s (%u.%u.%u)\n", result_name(r),
               VK_VERSION_MAJOR(api), VK_VERSION_MINOR(api), VK_VERSION_PATCH(api));
    }

    VkApplicationInfo app = {
        .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
        .pApplicationName = "vulkan-smoketest",
        .applicationVersion = 1,
        .pEngineName = "none",
        .engineVersion = 1,
        .apiVersion = VK_API_VERSION_1_0,
    };
    VkInstanceCreateInfo cinfo = {
        .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
        .pApplicationInfo = &app,
    };
    VkInstance instance = VK_NULL_HANDLE;
    VkResult r = create_instance(&cinfo, NULL, &instance);
    printf("vkCreateInstance: %s\n", result_name(r));
    if (r != VK_SUCCESS) return 1;

    PFN_vkEnumerateInstanceExtensionProperties enum_ext =
        (PFN_vkEnumerateInstanceExtensionProperties)gipa(instance, "vkEnumerateInstanceExtensionProperties");
    PFN_vkEnumeratePhysicalDevices enum_devs =
        (PFN_vkEnumeratePhysicalDevices)gipa(instance, "vkEnumeratePhysicalDevices");
    PFN_vkGetPhysicalDeviceProperties get_props =
        (PFN_vkGetPhysicalDeviceProperties)gipa(instance, "vkGetPhysicalDeviceProperties");

    uint32_t ext_count = 0;
    if (enum_ext) {
        enum_ext(NULL, &ext_count, NULL);
        printf("instance extensions: %u\n", ext_count);
        VkExtensionProperties *exts = calloc(ext_count ? ext_count : 1, sizeof(*exts));
        if (exts && ext_count) {
            enum_ext(NULL, &ext_count, exts);
            for (uint32_t i = 0; i < ext_count; i++) printf("  %s (rev %u)\n", exts[i].extensionName, exts[i].specVersion);
        }
        free(exts);
    }

    uint32_t dev_count = 0;
    if (enum_devs) {
        r = enum_devs(instance, &dev_count, NULL);
        printf("vkEnumeratePhysicalDevices: %s, count=%u\n", result_name(r), dev_count);
        VkPhysicalDevice *devs = calloc(dev_count ? dev_count : 1, sizeof(*devs));
        if (devs && dev_count) {
            enum_devs(instance, &dev_count, devs);
            for (uint32_t i = 0; i < dev_count; i++) {
                VkPhysicalDeviceProperties props;
                if (get_props) get_props(devs[i], &props);
                printf("  device %u: %s (type=%d, api=%u.%u.%u)\n", i, props.deviceName, props.deviceType,
                       VK_VERSION_MAJOR(props.apiVersion), VK_VERSION_MINOR(props.apiVersion), VK_VERSION_PATCH(props.apiVersion));
            }
        }
        free(devs);
    }

    PFN_vkDestroyInstance destroy_instance =
        (PFN_vkDestroyInstance)gipa(instance, "vkDestroyInstance");
    if (destroy_instance) destroy_instance(instance, NULL);
    dlclose(lib);
    return 0;
}
