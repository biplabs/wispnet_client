#include <gdk/gdk.h>

#ifdef GDK_WINDOWING_WAYLAND
#include <EGL/egl.h>
#include <gdk/gdkwayland.h>

#include <array>
#include <cstddef>

namespace {

EGLBoolean choose_config(EGLDisplay display, const EGLint* attributes,
                         EGLConfig* configs, EGLint capacity, EGLint* count) {
  const EGLBoolean result =
      eglChooseConfig(display, attributes, configs, capacity, count);
  if (!result || !count || *count != 0 || !attributes ||
      !GDK_IS_WAYLAND_DISPLAY(gdk_display_get_default())) {
    return result;
  }

  // GTK 3 requests an RGBA Wayland window without EGL_RENDERABLE_TYPE.
  // EGL defaults that request to GLES 1, which recent Mesa versions no
  // longer expose for these formats. Retry for the API GTK actually uses.
  std::array<EGLint, 32> retry;
  bool window_surface = false;
  bool alpha_buffer = false;
  for (std::size_t index = 0; index + 2 < retry.size(); index += 2) {
    const EGLint name = attributes[index];
    if (name == EGL_NONE) {
      if (!window_surface || !alpha_buffer) {
        return result;
      }
      const EGLenum api = eglQueryAPI();
      if (api != EGL_OPENGL_API && api != EGL_OPENGL_ES_API) {
        return result;
      }
      retry[index] = EGL_RENDERABLE_TYPE;
      retry[index + 1] =
          api == EGL_OPENGL_API ? EGL_OPENGL_BIT : EGL_OPENGL_ES2_BIT;
      retry[index + 2] = EGL_NONE;
      return eglChooseConfig(display, retry.data(), configs, capacity, count);
    }
    if (name == EGL_RENDERABLE_TYPE) {
      return result;
    }
    const EGLint value = attributes[index + 1];
    if (name == EGL_SURFACE_TYPE) {
      window_surface = (value & EGL_WINDOW_BIT) != 0;
    }
    if (name == EGL_ALPHA_SIZE) {
      alpha_buffer = value >= 8;
    }
    retry[index] = name;
    retry[index + 1] = value;
  }
  return result;
}

}  // namespace

extern "C" {
EGLBoolean (*epoxy_eglChooseConfig)(EGLDisplay, const EGLint*, EGLConfig*,
                                    EGLint, EGLint*) = choose_config;
}
#endif  // GDK_WINDOWING_WAYLAND
