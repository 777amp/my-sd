// Copyright 2021 Signal Messenger, LLC
// SPDX-License-Identifier: AGPL-3.0-only

export function useReducedMotion(): boolean {
  // Always enable reduced motion on Raspberry Pi 4B to avoid unnecessary
  // video/Lottie/animated-sticker decoding overhead.
  return true;
}
