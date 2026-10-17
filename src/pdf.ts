import * as mupdf from "mupdf"
import { readFileSync } from "node:fs"
import { basename } from "node:path"
import { Result, TaggedError } from "better-result"
import { Doc, type DocChunk, type LazyImage, type RawImage } from "./doc"

export class PdfOpenFailed extends TaggedError("PdfOpenFailed")<{
  path: string
  cause: unknown
  message: string
}> {}

/** One line of text. `top` is its top edge on the page, in PDF points. */
export type PdfLine = { text: string; size: number; bold: boolean; top: number }

export type PdfBlock =
  | { kind: "text"; lines: PdfLine[] }
  | { kind: "image"; id: string }
  /** A message from the reader itself, e.g. a page that failed to parse. */
  | { kind: "note"; text: string }

/** Document-wide typography, learned from a sample of pages. */
export type PdfLayout = {
  /** Font size of body text. */
  body: number
  /** Sizes larger than body, biggest first; index 0 is `#`. */
  headingSizes: number[]
  /** Usual top-to-top distance between lines inside a paragraph. */
  pitch: number
  /** Normalized text of running headers and footers to drop. */
  running: ReadonlySet<string>
}

const errorMessage = (error: unknown) => {
  const cause = error instanceof Error && error.cause !== undefined ? error.cause : error
  return cause instanceof Error ? cause.message : String(cause)
}

/** Images smaller than this (in PDF points) are icons, bullets or rules, not content. */
const MIN_IMAGE_SIZE = 40
/** Embedded images are scaled down to this width; terminals never show more. */
const MAX_IMAGE_PIXELS_WIDE = 800
/** Pages read to learn the layout, spread evenly through the document. */
const SAMPLE_PAGES = 24

const BULLET = /^[•◦▪▸►‣⁃●○■□–-]\s*/
const BOLD_FONT_NAME = /bold|black|heavy|semibold/i
const PAGE_NUMBER = String.raw`(?:\d+|m{0,3}(?:cm|cd|d?c{0,3})(?:xc|xl|l?x{0,3})(?:ix|iv|v?i{0,3}))`
/** A page label like "xiv | Preface", "Chapter 3 | 57" or a bare "57". */
const PAGE_LABEL = new RegExp(`^(?:${PAGE_NUMBER}\\s*\\|.{0,60}|.{0,60}\\|\\s*${PAGE_NUMBER}|${PAGE_NUMBER})$`, "i")
/**
 * Line-end hyphens: ASCII, Unicode hyphens, and the "!" some fonts map their
 * hyphen glyph to.
 */
const LINE_END_HYPHEN = /[a-z][-‐!]$/

const round = (n: number) => Math.round(n * 2) / 2

const mostCommon = (weights: ReadonlyMap<number, number>) => {
  let best: number | undefined
  let bestWeight = -1
  for (const [value, weight] of weights) {
    if (weight > bestWeight) {
      best = value
      bestWeight = weight
    }
  }
  return best
}

const blockSize = (lines: readonly PdfLine[]) => {
  const weights = new Map<number, number>()
  for (const line of lines) {
    weights.set(line.size, (weights.get(line.size) ?? 0) + line.text.length)
  }
  return mostCommon(weights) ?? 0
}

const blockText = (lines: readonly PdfLine[]) => lines.map((l) => l.text).join(" ").replace(/\s+/g, " ").trim()

/** Normalizes page numbers so "Page 3" and "Page 4" count as the same running header. */
const runningKey = (text: string) => text.replace(/\d+/g, "#").trim()

/** Joins wrapped lines into a paragraph, undoing end-of-line hyphenation. */
const joinLines = (lines: readonly string[]) =>
  lines.reduce((acc, line) => {
    if (acc === "") return line
    if (LINE_END_HYPHEN.test(acc) && /^[a-z]/.test(line)) return acc.slice(0, -1) + line
    return `${acc} ${line}`
  }, "")

/**
 * Keeps a paragraph that starts like markdown syntax ("10. Title", "# 1")
 * from parsing as a list, heading or quote. The viewer draws backslash escapes
 * literally, so this prefixes an invisible word joiner instead.
 */
const escapeLeading = (text: string) => text.replace(/^(?=[#>+]|\d+[.)]\s)/, "⁠")

/**
 * Table-of-contents entries: dot leaders ("Title. . . . . 57") collapse to
 * "Title · 57", and entries set apart by wide gaps ("A      4 B      9") get
 * a line each.
 */
const collapseLeaders = (text: string) =>
  text
    .replace(/\s*(?:\.\s*){4,}/g, " · ")
    .replace(/\s{3,}(\d+|[ivxlc]+)(?:\s+|$)/g, " · $1\n")
    .trimEnd()

const tidy = (text: string) => escapeLeading(collapseLeaders(text))

const paragraphMarkdown = (lines: readonly PdfLine[]) => {
  const items: string[][] = []
  let isList = false
  for (const { text } of lines) {
    const trimmed = text.trim()
    if (trimmed === "") continue
    if (BULLET.test(trimmed)) {
      isList = true
      items.push([trimmed.replace(BULLET, "")])
    } else {
      const last = items.at(-1)
      if (last) last.push(trimmed)
      else items.push([trimmed])
    }
  }
  if (isList) {
    return items.map((item) => `- ${tidy(joinLines(item))}`).join("\n")
  }
  return tidy(joinLines(items.flat()))
}

/**
 * The most common top-to-top distance between consecutive same-size lines.
 * Lines inside paragraphs outnumber paragraph breaks, so this is the line pitch.
 */
const linePitch = (pages: readonly (readonly PdfBlock[])[]) => {
  const deltas = new Map<number, number>()
  for (const page of pages) {
    const lines = page.flatMap((b) => (b.kind === "text" ? b.lines : []))
    for (let i = 1; i < lines.length; i++) {
      const [a, b] = [lines[i - 1], lines[i]]
      if (!a || !b || a.size !== b.size) continue
      const delta = Math.round(b.top - a.top)
      if (delta > 0) deltas.set(delta, (deltas.get(delta) ?? 0) + 1)
    }
  }
  return mostCommon(deltas) ?? 0
}

/**
 * Some PDFs put every line in its own text block. Rejoins consecutive blocks
 * with the same size and boldness that sit one line pitch apart, so wrapped
 * paragraphs and list items come back together. A block starting lowercase
 * continues the previous sentence, so it may sit a little further away.
 */
const mergeWrapped = (blocks: readonly PdfBlock[], pitch: number) => {
  const merged: PdfBlock[] = []
  for (const block of blocks) {
    const prev = merged.at(-1)
    const prevLast = prev?.kind === "text" ? prev.lines.at(-1) : undefined
    const first = block.kind === "text" ? block.lines[0] : undefined
    if (prev?.kind === "text" && block.kind === "text" && prevLast && first) {
      const continues = /^[a-z]/.test(first.text.trim())
      const sameStyle = prevLast.size === first.size && prevLast.bold === first.bold
      const delta = first.top - prevLast.top
      if (sameStyle && delta > 0 && delta <= pitch * (continues ? 1.6 : 1.15)) {
        merged[merged.length - 1] = { kind: "text", lines: [...prev.lines, ...block.lines] }
        continue
      }
    }
    merged.push(block)
  }
  return merged
}

const mergeAll = (pages: readonly (readonly PdfBlock[])[], pitch: number) =>
  pages.map((page) => mergeWrapped(page, pitch))

// mupdf's structured-text JSON is parsed field by field: it crosses a WASM
// boundary and malformed entries are skipped rather than trusted.
const isRecord = (v: unknown): v is Record<string, unknown> => typeof v === "object" && v !== null
const num = (v: unknown) => (typeof v === "number" ? v : undefined)
const str = (v: unknown) => (typeof v === "string" ? v : undefined)

type JsonBlock = { kind: "text"; lines: PdfLine[] } | { kind: "image"; width: number; height: number }

const parsePageJson = (json: string) => {
  const parsed: unknown = JSON.parse(json)
  const blocks: JsonBlock[] = []
  if (!isRecord(parsed) || !Array.isArray(parsed.blocks)) return blocks
  for (const block of parsed.blocks) {
    if (!isRecord(block) || !isRecord(block.bbox)) continue
    if (block.type === "image") {
      blocks.push({ kind: "image", width: num(block.bbox.w) ?? 0, height: num(block.bbox.h) ?? 0 })
      continue
    }
    if (block.type !== "text" || !Array.isArray(block.lines)) continue
    const lines: PdfLine[] = []
    for (const line of block.lines) {
      if (!isRecord(line) || !isRecord(line.font) || !isRecord(line.bbox)) continue
      const text = str(line.text) ?? ""
      if (text.trim() === "") continue
      lines.push({
        text,
        size: round(num(line.font.size) ?? 0),
        bold: line.font.weight === "bold" || BOLD_FONT_NAME.test(str(line.font.name) ?? ""),
        top: num(line.bbox.y) ?? 0,
      })
    }
    if (lines.length > 0) blocks.push({ kind: "text", lines })
  }
  return blocks
}

/** mupdf can put a heading and its body in one block; split where the font size changes. */
const splitBySize = (lines: readonly PdfLine[]) => {
  const groups: PdfLine[][] = []
  for (const line of lines) {
    const group = groups.at(-1)
    if (group && group[0]?.size === line.size) group.push(line)
    else groups.push([line])
  }
  return groups
}

/** Pixel size an embedded image decodes to, read from its header without decoding. */
const decodedSize = (image: mupdf.Image) => {
  const scale = Math.min(1, MAX_IMAGE_PIXELS_WIDE / image.getWidth())
  return {
    width: Math.max(1, Math.round(image.getWidth() * scale)),
    height: Math.max(1, Math.round(image.getHeight() * scale)),
  }
}

/** Draws an embedded image into a small opaque RGBA buffer. Masks are drawn as black ink. */
const decodeImage = (image: mupdf.Image): RawImage => {
  const { width, height } = decodedSize(image)
  const pixmap = new mupdf.Pixmap(mupdf.ColorSpace.DeviceRGB, [0, 0, width, height], true)
  pixmap.clear(255)
  const device = new mupdf.DrawDevice(mupdf.Matrix.identity, pixmap)
  const ctm: mupdf.Matrix = [width, 0, 0, height, 0, 0]
  if (image.getImageMask()) device.fillImageMask(image, ctm, mupdf.ColorSpace.DeviceRGB, [0, 0, 0], 1)
  else device.fillImage(image, ctm, 1)
  device.close()
  return { pixels: new Uint8Array(pixmap.getPixels()), width, height, stride: pixmap.getStride() }
}

/** Indexes of `count` pages spread evenly across a document of `total` pages. */
const samplePages = (total: number, count: number) => {
  if (total <= count) return Array.from({ length: total }, (_, i) => i)
  return Array.from({ length: count }, (_, i) => Math.floor((i * total) / count))
}

const yieldToEventLoop = () => new Promise<void>((resolve) => setTimeout(resolve, 0))

export const Pdf = {
  /**
   * Learns body size, heading sizes, line pitch and running headers from a
   * sample of pages. Body text is the most common size; larger sizes become
   * headings ranked by size. Text on at least half the sampled pages is a
   * running header or footer.
   */
  analyze: (pages: readonly (readonly PdfBlock[])[]): PdfLayout => {
    const sizeWeights = new Map<number, number>()
    for (const block of pages.flat()) {
      if (block.kind !== "text") continue
      for (const line of block.lines) {
        sizeWeights.set(line.size, (sizeWeights.get(line.size) ?? 0) + line.text.length)
      }
    }
    const body = mostCommon(sizeWeights) ?? 0

    const pitch = linePitch(pages)
    const repeats = new Map<string, number>()
    for (const page of mergeAll(pages, pitch)) {
      const seen = new Set(page.flatMap((b) => (b.kind === "text" ? [runningKey(blockText(b.lines))] : [])))
      for (const key of seen) repeats.set(key, (repeats.get(key) ?? 0) + 1)
    }
    const running = new Set(
      pages.length >= 3
        ? [...repeats].filter(([key, count]) => key.length < 80 && count >= pages.length * 0.5).map(([key]) => key)
        : [],
    )

    return {
      body,
      headingSizes: [...sizeWeights.keys()].filter((s) => s >= body * 1.15).sort((a, b) => b - a),
      pitch,
      running,
    }
  },

  /** Turns pages into markdown using a layout learned by `analyze`. */
  render: (pages: readonly (readonly PdfBlock[])[], layout: PdfLayout) => {
    const out: string[] = []
    for (const block of mergeAll(pages, layout.pitch).flat()) {
      if (block.kind === "image") {
        out.push(`![](${Doc.imageRef(block.id)})`)
        continue
      }
      if (block.kind === "note") {
        out.push(`> *${block.text}*`)
        continue
      }
      const text = collapseLeaders(blockText(block.lines))
      if (text === "" || layout.running.has(runningKey(text)) || PAGE_LABEL.test(text)) continue

      const level = layout.headingSizes.indexOf(blockSize(block.lines))
      if (level !== -1 && text.length < 200) {
        out.push(`${"#".repeat(Math.min(level + 1, 3))} ${text}`)
      } else if (block.lines.every((l) => l.bold) && text.length < 100 && !BULLET.test(text)) {
        out.push(`#### ${text}`)
      } else {
        out.push(paragraphMarkdown(block.lines))
      }
    }
    return out.join("\n\n")
  },

  /** Analyze and render in one step, for documents already fully read. */
  toMarkdown: (pages: readonly (readonly PdfBlock[])[]) => Pdf.render(pages, Pdf.analyze(pages)),

  /**
   * Opens a PDF and returns at once. Only a small sample of pages is read up
   * front, to learn the layout; the rest stream in as chunks, and images are
   * decoded only when the viewer shows them.
   */
  open: (path: string) =>
    Result.try({
      try: () => readFileSync(path),
      catch: (cause) =>
        new PdfOpenFailed({
          path,
          cause,
          message:
            cause instanceof Error && "code" in cause && cause.code === "EPERM"
              ? `macOS blocked access to ${path} (${cause.message}). Allow your terminal in System Settings → Privacy & Security → Files & Folders, then restart it.`
              : `Could not read ${path} (${errorMessage(cause)}). Check that the path is right and the file exists.`,
        }),
    }).andThen((bytes) => Pdf.fromBytes(bytes, path, basename(path, ".pdf"))),

  /**
   * `open` for PDF bytes already in hand (a download, say). `source` names the
   * PDF in errors; `fallbackTitle` is used when the PDF has no title of its own.
   */
  fromBytes: (bytes: Uint8Array, source: string, fallbackTitle: string) =>
    Result.try({
      try: () => mupdf.Document.openDocument(bytes, "application/pdf"),
      catch: (cause) =>
        new PdfOpenFailed({
          path: source,
          cause,
          message: `Could not open ${source} as a PDF (${errorMessage(cause)}). It may be encrypted or damaged. Open it in a PDF viewer to confirm it is valid.`,
        }),
    }).map((document) => {
      const pageCount = document.countPages()
      const images = new Map<string, LazyImage>()

      const readPage = (i: number): PdfBlock[] => {
        const stext = document.loadPage(i).toStructuredText("preserve-images")
        const parsed = parsePageJson(stext.asJSON())

        // The JSON has image positions but not pixels; a second, image-only walk
        // (cheap: no per-character callbacks) yields the image objects in the same order.
        const pageImages: mupdf.Image[] = []
        if (parsed.some((b) => b.kind === "image")) {
          stext.walk({ onImageBlock: (_bbox, _transform, image) => void pageImages.push(image) })
        }

        const blocks: PdfBlock[] = []
        let imageIndex = 0
        for (const block of parsed) {
          if (block.kind === "text") {
            for (const lines of splitBySize(block.lines)) blocks.push({ kind: "text", lines })
            continue
          }
          const image = pageImages[imageIndex++]
          if (!image || block.width < MIN_IMAGE_SIZE || block.height < MIN_IMAGE_SIZE) continue
          const id = `p${i + 1}-${imageIndex}`
          let decoded: RawImage | undefined
          images.set(id, {
            ...decodedSize(image),
            decode: () => {
              // An image mupdf cannot decode is left out; the text around it still shows.
              decoded ??= Result.try(() => decodeImage(image)).unwrapOr(undefined)
              return decoded
            },
          })
          blocks.push({ kind: "image", id })
        }
        return blocks
      }

      // One malformed page must not lose the whole document: note it and keep going.
      const safeReadPage = (i: number): PdfBlock[] =>
        Result.try(() => readPage(i)).match({
          ok: (blocks) => blocks,
          err: (error) => [
            { kind: "note", text: `Page ${i + 1} could not be read (${errorMessage(error)}). The other pages are unaffected.` },
          ],
        })

      const layout = Pdf.analyze(samplePages(pageCount, SAMPLE_PAGES).map(safeReadPage))

      // One chunk per page, so the viewer can show the document page by page.
      const chunks = async function* (): AsyncGenerator<DocChunk> {
        for (let i = 0; i < pageCount; i++) {
          yield { markdown: Pdf.render([safeReadPage(i)], layout), page: i + 1 }
          // Let the viewer draw and handle keys between pages.
          await yieldToEventLoop()
        }
      }

      const doc: Doc = {
        title: document.getMetaData("info:Title")?.trim() || fallbackTitle,
        pageCount,
        chunks: chunks(),
        images,
      }
      return doc
    }),
} as const
