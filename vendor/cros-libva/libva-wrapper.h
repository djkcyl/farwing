// Copyright 2022 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <va/va.h>
#include <va/va_drm.h>
#include <va/va_drmcommon.h>
// The encode structures (packed headers, H.264 parameters) live here; va.h only
// forward-declares the buffer types.
#include <va/va_enc_h264.h>
