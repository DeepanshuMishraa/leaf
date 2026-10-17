import { expect, test } from "bun:test"
import type { RawImage } from "./doc"
import { Mono, type Rgb } from "./mono"

const INK: Rgb = [200, 210, 220]

/** An RGBA image from rows of grey levels. */
const grey = (rows: number[][]): RawImage => {
  const height = rows.length
  const width = rows[0]?.length ?? 0
  const pixels = new Uint8Array(width * height * 4)
  rows.flat().forEach((v, i) => pixels.set([v, v, v, 255], i * 4))
  return { pixels, width, height, stride: width * 4 }
}

/** The opacity of each pixel. */
const alphas = (image: RawImage) => Array.from({ length: image.width * image.height }, (_, i) => image.pixels[i * 4 + 3])

test("every pixel takes the ink color", () => {
  const out = Mono.paint(grey([[255, 0, 128, 255]]), INK)
  for (let i = 0; i < 4; i++) expect(Array.from(out.pixels.slice(i * 4, i * 4 + 3))).toEqual([...INK])
})

test("a diagram on white keeps its dark lines and drops its paper", () => {
  expect(alphas(Mono.paint(grey([[255, 255, 255, 0]]), INK))).toEqual([0, 0, 0, 255])
})

test("a dark image keeps its light parts and drops its dark ground", () => {
  expect(alphas(Mono.paint(grey([[0, 0, 0, 255]]), INK))).toEqual([0, 0, 0, 255])
})

test("faint haze on the paper is dropped, mid-tones stay partly opaque", () => {
  // Mostly white, so dark is ink: 250 is near-paper haze, 128 a mid-tone.
  const [paper, haze, mid, line] = alphas(Mono.paint(grey([[255, 250, 128, 0, 255, 255]]), INK))
  expect(paper).toBe(0)
  expect(haze).toBe(0)
  expect(mid).toBeGreaterThan(64)
  expect(mid).toBeLessThan(192)
  expect(line).toBe(255)
})

test("color is dropped: a red and a grey of equal brightness come out the same", () => {
  // Pure red has luminance 0.2126, the same as grey 54.
  const pixels = new Uint8Array([255, 255, 255, 255, 255, 0, 0, 255, 54, 54, 54, 255, 0, 0, 0, 255])
  const [, red, grey54] = alphas(Mono.paint({ pixels, width: 4, height: 1, stride: 16 }, INK))
  expect(red).toBe(grey54)
})

test("a blank page paints nothing", () => {
  expect(alphas(Mono.paint(grey([[255, 255], [255, 255]]), INK))).toEqual([0, 0, 0, 0])
})

test("output is tightly packed even when the input has row padding", () => {
  const image: RawImage = { pixels: new Uint8Array([0, 0, 0, 255, 9, 9, 9, 9, 255, 255, 255, 255, 9, 9, 9, 9]), width: 1, height: 2, stride: 8 }
  const out = Mono.paint(image, INK)
  expect(out.stride).toBe(4)
  expect(Array.from(out.pixels)).toEqual([...INK, 0, ...INK, 255])
})
