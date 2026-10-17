import type { RawImage } from "./doc"

export type Rgb = readonly [number, number, number]

/** Ink fainter than this is scan noise or JPEG haze on the paper, and is dropped. */
const NOISE = 0.08

/** Relative luminance (0–1) of the RGBA pixel at byte offset `i`. */
const luminance = (pixels: Uint8Array, i: number) =>
  (0.2126 * (pixels[i] ?? 0) + 0.7152 * (pixels[i + 1] ?? 0) + 0.0722 * (pixels[i + 2] ?? 0)) / 255

export const Mono = {
  /**
   * Repaints an RGBA image as `ink` on nothing: every pixel takes the ink
   * color, and how much ink it holds becomes its opacity. The paper turns
   * fully transparent, so the drawing sits on the terminal like its text.
   * Mostly light images (diagrams on white) treat their dark lines as ink;
   * mostly dark images (photos) treat their light parts as ink. Contrast is
   * stretched so faint scans still read.
   */
  paint: (image: RawImage, ink: Rgb): RawImage => {
    const { pixels, width, height, stride } = image
    const levels = new Float32Array(width * height)
    let min = 1
    let max = 0
    let total = 0
    for (let y = 0; y < height; y++) {
      for (let x = 0; x < width; x++) {
        const value = luminance(pixels, y * stride + x * 4)
        levels[y * width + x] = value
        min = Math.min(min, value)
        max = Math.max(max, value)
        total += value
      }
    }
    const range = max - min
    const inkIsDark = total / Math.max(1, levels.length) > 0.5

    const out = new Uint8Array(width * height * 4)
    for (let i = 0; i < levels.length; i++) {
      const level = range === 0 ? 0 : ((levels[i] ?? 0) - min) / range
      // A flat image is all paper.
      const amount = range === 0 ? 0 : inkIsDark ? 1 - level : level
      const alpha = Math.max(0, (amount - NOISE) / (1 - NOISE))
      out.set([ink[0], ink[1], ink[2], Math.round(alpha * 255)], i * 4)
    }
    return { pixels: out, width, height, stride: width * 4 }
  },
} as const
