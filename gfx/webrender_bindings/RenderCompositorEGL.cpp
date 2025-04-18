/* -*- Mode: C++; tab-width: 8; indent-tabs-mode: nil; c-basic-offset: 2 -*- */
/* vim: set ts=8 sts=2 et sw=2 tw=80: */
/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include "RenderCompositorEGL.h"

#include "GLContext.h"
#include "GLContextEGL.h"
#include "GLContextProvider.h"
#include "GLLibraryEGL.h"
#include "mozilla/StaticPrefs_gfx.h"
#include "mozilla/gfx/Logging.h"
#include "mozilla/gfx/gfxVars.h"
#include "mozilla/layers/BuildConstants.h"
#include "mozilla/webrender/RenderThread.h"
#include "mozilla/widget/CompositorWidget.h"

#ifdef MOZ_WIDGET_GTK
#  include "mozilla/WidgetUtilsGtk.h"
#  include "mozilla/widget/GtkCompositorWidget.h"
#endif

#ifdef MOZ_WIDGET_ANDROID
#  include "mozilla/java/GeckoSurfaceTextureWrappers.h"
#  include "mozilla/layers/AndroidHardwareBuffer.h"
#  include "mozilla/widget/AndroidCompositorWidget.h"
#  include <android/native_window.h>
#  include <android/native_window_jni.h>
#endif

namespace mozilla::wr {

extern LazyLogModule gRenderThreadLog;
#define LOG(...) MOZ_LOG(gRenderThreadLog, LogLevel::Debug, (__VA_ARGS__))

/* static */
UniquePtr<RenderCompositor> RenderCompositorEGL::Create(
    const RefPtr<widget::CompositorWidget>& aWidget, nsACString& aError) {
  if (kIsLinux && !gfx::gfxVars::UseEGL()) {
    return nullptr;
  }
  RefPtr<gl::GLContext> gl = RenderThread::Get()->SingletonGL(aError);
  if (!gl) {
    if (aError.IsEmpty()) {
      aError.Assign("RcANGLE(no shared GL)"_ns);
    } else {
      aError.Append("(Create)"_ns);
    }
    return nullptr;
  }
  return MakeUnique<RenderCompositorEGL>(aWidget, std::move(gl));
}

EGLSurface RenderCompositorEGL::CreateEGLSurface() {
  EGLSurface surface = EGL_NO_SURFACE;
  surface = gl::GLContextEGL::CreateEGLSurfaceForCompositorWidget(
      mWidget, gl::GLContextEGL::Cast(gl())->mSurfaceConfig);
  if (surface == EGL_NO_SURFACE) {
    const auto* renderThread = RenderThread::Get();
    gfxCriticalNote << "Failed to create EGLSurface. "
                    << renderThread->RendererCount() << " renderers, "
                    << renderThread->ActiveRendererCount() << " active.";
  }
  return surface;
}

RenderCompositorEGL::RenderCompositorEGL(
    const RefPtr<widget::CompositorWidget>& aWidget,
    RefPtr<gl::GLContext>&& aGL)
    : RenderCompositor(aWidget), mGL(aGL), mEGLSurface(EGL_NO_SURFACE) {
  MOZ_ASSERT(mGL);
  LOG("RenderCompositorEGL::RenderCompositorEGL()");
}

RenderCompositorEGL::~RenderCompositorEGL() {
  LOG("RenderCompositorEGL::~RenderCompositorEGL()");
#ifdef MOZ_WIDGET_ANDROID
  java::GeckoSurfaceTexture::DestroyUnused((int64_t)gl());
#endif
  DestroyEGLSurface();
}

bool RenderCompositorEGL::BeginFrame() {
  if (kIsLinux && mEGLSurface == EGL_NO_SURFACE) {
    gfxCriticalNote
        << "We don't have EGLSurface to draw into. Called too early?";
    return false;
  }
#ifdef MOZ_WAYLAND
  if (mWidget->AsGTK()) {
    if (!mWidget->AsGTK()->SetEGLNativeWindowSize(GetBufferSize())) {
      // Wayland only check we have correct window size to avoid
      // rendering artifacts.
      return false;
    }
  }
#endif
  if (!MakeCurrent()) {
    gfxCriticalNote << "Failed to make render context current, can't draw.";
    return false;
  }

#ifdef MOZ_WIDGET_ANDROID
  java::GeckoSurfaceTexture::DestroyUnused((int64_t)gl());
  gl()->MakeCurrent();  // DestroyUnused can change the current context!
#endif

  return true;
}

RenderedFrameId RenderCompositorEGL::EndFrame(
    const nsTArray<DeviceIntRect>& aDirtyRects) {
#ifdef MOZ_WIDGET_ANDROID
  const auto& gle = gl::GLContextEGL::Cast(gl());
  const auto& egl = gle->mEgl;

  EGLSync sync = nullptr;
  if (layers::AndroidHardwareBufferApi::Get()) {
    sync = egl->fCreateSync(LOCAL_EGL_SYNC_NATIVE_FENCE_ANDROID, nullptr);
  }
  if (sync) {
    int fenceFd = egl->fDupNativeFenceFDANDROID(sync);
    if (fenceFd >= 0) {
      mReleaseFence = new layers::FenceFileHandle(UniqueFileHandle(fenceFd));
    }
    egl->fDestroySync(sync);
    sync = nullptr;
  }
#endif

  RenderedFrameId frameId = GetNextRenderFrameId();
#ifdef MOZ_WIDGET_GTK
  if (mWidget->IsHidden()) {
    return frameId;
  }
#endif
  if (mEGLSurface != EGL_NO_SURFACE && aDirtyRects.Length() > 0) {
    gfx::IntRegion bufferInvalid;
    const auto bufferSize = GetBufferSize();
    for (const DeviceIntRect& rect : aDirtyRects) {
      const auto left = std::clamp(rect.min.x, 0, bufferSize.width);
      const auto top = std::clamp(rect.min.y, 0, bufferSize.height);

      const auto right = std::clamp(rect.max.x, 0, bufferSize.width);
      const auto bottom = std::clamp(rect.max.y, 0, bufferSize.height);

      const auto width = right - left;
      const auto height = bottom - top;

      bufferInvalid.OrWith(
          gfx::IntRect(left, (GetBufferSize().height - bottom), width, height));
    }
    gl()->SetDamage(bufferInvalid);
  }

#ifdef MOZ_WIDGET_GTK
  // Rendering on Wayland has to be atomic (buffer attach + commit) and
  // wayland surface is also used by main thread so lock it before
  // we paint at SwapBuffers().
  UniquePtr<widget::WaylandSurfaceLock> lock;
  if (auto* gtkWidget = mWidget->AsGTK()) {
    lock = gtkWidget->LockSurface();
  }
#endif
  gl()->SwapBuffers();
  return frameId;
}

void RenderCompositorEGL::Pause() { DestroyEGLSurface(); }

bool RenderCompositorEGL::Resume() {
  if (kIsAndroid) {
    // Destroy EGLSurface if it exists.
    DestroyEGLSurface();

    auto size = GetBufferSize();
    GLint maxTextureSize = 0;
    gl()->fGetIntegerv(LOCAL_GL_MAX_TEXTURE_SIZE, (GLint*)&maxTextureSize);

    // When window size is too big, hardware buffer allocation could fail.
    if (maxTextureSize < size.width || maxTextureSize < size.height) {
      gfxCriticalNote << "Too big ANativeWindow size(" << size.width << ", "
                      << size.height << ") MaxTextureSize " << maxTextureSize;
      return false;
    }

    mEGLSurface = CreateEGLSurface();
    if (mEGLSurface == EGL_NO_SURFACE) {
      // Often when we fail to create an EGL surface it is because the Java
      // Surface we have been provided is invalid. Therefore the on the first
      // occurence we don't raise a WebRenderError and instead just return
      // failure. This allows the widget a chance to request a new Java
      // Surface. On subsequent failures, raising the WebRenderError will
      // result in the compositor being recreated, falling back through
      // webrender configurations, and eventually crashing if we still do not
      // succeed.
      if (!mHandlingNewSurfaceError) {
        mHandlingNewSurfaceError = true;
      } else {
        RenderThread::Get()->HandleWebRenderError(WebRenderError::NEW_SURFACE);
      }
      return false;
    }
    mHandlingNewSurfaceError = false;

    gl::GLContextEGL::Cast(gl())->SetEGLSurfaceOverride(mEGLSurface);
  } else if (kIsLinux) {
    // Destroy EGLSurface if it exists and create a new one. We will set the
    // swap interval after MakeCurrent() has been called.
    DestroyEGLSurface();
    mEGLSurface = CreateEGLSurface();
    if (mEGLSurface != EGL_NO_SURFACE) {
      // We have a new EGL surface, which on wayland needs to be configured for
      // non-blocking buffer swaps. We need MakeCurrent() to set our current EGL
      // context before we call eglSwapInterval, which is why we do it here
      // rather than where the surface was created.
      const auto& gle = gl::GLContextEGL::Cast(gl());
      const auto& egl = gle->mEgl;
      MakeCurrent();

      const int interval = gfx::gfxVars::SwapIntervalEGL() ? 1 : 0;
      egl->fSwapInterval(interval);
    } else {
      RenderThread::Get()->HandleWebRenderError(WebRenderError::NEW_SURFACE);
      return false;
    }
  }
  return true;
}

bool RenderCompositorEGL::IsPaused() { return mEGLSurface == EGL_NO_SURFACE; }

bool RenderCompositorEGL::MakeCurrent() {
  const auto& gle = gl::GLContextEGL::Cast(gl());

  gle->SetEGLSurfaceOverride(mEGLSurface);
  bool ok = gl()->MakeCurrent();
  if (!gl()->IsGLES() && ok && mEGLSurface != EGL_NO_SURFACE) {
    // If we successfully made a surface current, set the draw buffer
    // appropriately. It's not well-defined by the EGL spec whether
    // eglMakeCurrent should do this automatically. See bug 1646135.
    gl()->fDrawBuffer(gl()->IsDoubleBuffered() ? LOCAL_GL_BACK
                                               : LOCAL_GL_FRONT);
  }
  return ok;
}

void RenderCompositorEGL::DestroyEGLSurface() {
  const auto& gle = gl::GLContextEGL::Cast(gl());
  const auto& egl = gle->mEgl;

  // Release EGLSurface of back buffer before calling ResizeBuffers().
  if (mEGLSurface) {
    gle->SetEGLSurfaceOverride(EGL_NO_SURFACE);
    gl::GLContextEGL::DestroySurface(*egl, mEGLSurface);
    mEGLSurface = nullptr;
  }
}

RefPtr<layers::Fence> RenderCompositorEGL::GetAndResetReleaseFence() {
#ifdef MOZ_WIDGET_ANDROID
  MOZ_ASSERT(!layers::AndroidHardwareBufferApi::Get() || mReleaseFence);
  return mReleaseFence.forget();
#else
  return nullptr;
#endif
}

LayoutDeviceIntSize RenderCompositorEGL::GetBufferSize() {
  return mWidget->GetClientSize();
}

bool RenderCompositorEGL::UsePartialPresent() {
  return gfx::gfxVars::WebRenderMaxPartialPresentRects() > 0;
}

bool RenderCompositorEGL::RequestFullRender() { return false; }

uint32_t RenderCompositorEGL::GetMaxPartialPresentRects() {
  return gfx::gfxVars::WebRenderMaxPartialPresentRects();
}

bool RenderCompositorEGL::ShouldDrawPreviousPartialPresentRegions() {
  return true;
}

size_t RenderCompositorEGL::GetBufferAge() const {
  if (!StaticPrefs::
          gfx_webrender_allow_partial_present_buffer_age_AtStartup()) {
    return 0;
  }
  return gl()->GetBufferAge();
}

void RenderCompositorEGL::SetBufferDamageRegion(const wr::DeviceIntRect* aRects,
                                                size_t aNumRects) {
  const auto& gle = gl::GLContextEGL::Cast(gl());
  const auto& egl = gle->mEgl;
  if (gle->HasKhrPartialUpdate() &&
      StaticPrefs::gfx_webrender_allow_partial_present_buffer_age_AtStartup()) {
    std::vector<EGLint> rects;
    rects.reserve(4 * aNumRects);
    const auto bufferSize = GetBufferSize();
    for (size_t i = 0; i < aNumRects; i++) {
      const auto left = std::clamp(aRects[i].min.x, 0, bufferSize.width);
      const auto top = std::clamp(aRects[i].min.y, 0, bufferSize.height);

      const auto right = std::clamp(aRects[i].max.x, 0, bufferSize.width);
      const auto bottom = std::clamp(aRects[i].max.y, 0, bufferSize.height);

      const auto width = right - left;
      const auto height = bottom - top;

      rects.push_back(left);
      rects.push_back(bufferSize.height - bottom);
      rects.push_back(width);
      rects.push_back(height);
    }
    const auto ret =
        egl->fSetDamageRegion(mEGLSurface, rects.data(), rects.size() / 4);
    if (ret == LOCAL_EGL_FALSE) {
      const auto err = egl->mLib->fGetError();
      gfxCriticalError() << "Error in eglSetDamageRegion: " << gfx::hexa(err);
    }
  }
}

}  // namespace mozilla::wr
