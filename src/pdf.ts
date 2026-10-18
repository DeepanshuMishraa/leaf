import * as mupdf from "mupdf"
import { readFileSync } from "node:fs"
import { basename } from "node:path"
import { Result, TaggedError } from "better-result"
import { Doc, type LazyImage, type RawImage } from "./doc"

export class PdfOpenFailed extends TaggedError("PdfOpenFailed")<{
  path: string
  cause: unknown
  message: string
}> {}

/**
 * One line of text. `top` is its top edge on the page, in PDF points; `left`
 * and `right` are its edges as shares of the page width; `mono` marks code.
 * Text on one row in separate runs (table cells, a list number and its item)
 * comes as separate lines with the same `top`.
 */
export type PdfLine = {
  text: string
  size: number
  bold: boolean
  italic: boolean
  top: number
  left: number
  right: number
  mono: boolean
}

export type PdfBlock =
  | { kind: "text"; lines: PdfLine[] }
  | { kind: "image"; id: string }
  /** A message from the reader itself, e.g. a page that failed to parse. */
  | { kind: "note"; text: string }

/** A block once the page is laid out: tables found among the text. */
type Block = PdfBlock | { kind: "table"; rows: string[][] }

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
  /** Usual width of a full line of body text, as a share of the page width. */
  measure: number
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

/**
 * mupdf rounds positions and font sizes in its JSON to whole points, which
 * turns a 9.9pt/10.1pt jitter in one paragraph into two sizes. The JSON is
 * printed at this scale and divided back.
 */
const JSON_SCALE = 100
/** Shapes in a vector drawing; thinner paths are rules, underlines and borders. */
const MIN_SHAPE_SIZE = 4
/** A cluster of at least this many shapes is a drawing (one box is a shaded panel). */
const MIN_FIGURE_SHAPES = 3
/** Shapes this close (in points) belong to the same drawing. */
const FIGURE_GAP = 8
/** Diagram labels are short; a line this long inside shapes means a boxed panel of prose. */
const MAX_LABEL_LENGTH = 60
/** A line whose middle is this close to the page's middle (as a share of the width) is centered. */
const CENTER_SLACK = 0.03
/** Lines narrower than this share of a full line can show they are centered; full lines are centered trivially. */
const CENTER_MAX_WIDTH = 0.85
/** Cells whose left edges are this close (as a share of the page width) share a column. */
const COLUMN_SLACK = 0.02

const BULLET = /^[•◦▪▸►‣⁃●○■□–-]\s*/
/** A numbered list item: "1. ", "12) ". Longer numbers are years or figures, not items. */
const ORDERED = /^(\d{1,3})[.)]\s+/
/** A run that is only a list marker, set apart from its item text. */
const MARKER = /^(?:[•◦▪▸►‣⁃●○■□–-]|\d{1,3}[.)]|\(?[a-z\d]{1,3}\))$/i
/** Figure and table captions. */
const CAPTION = /^(?:Figure|Fig\.|Table|Listing|Algorithm|Chart|Exhibit)\s*[\dA-Z]+(?:[.-]\d+)*[.:]\s/
/** A URL broken at the end of a line, after a character a URL breaks at. */
const URL_BREAK = /https?:\/\/\S*[/.\-_?=&#~]$/
const BOLD_FONT_NAME = /bold|black|heavy|semibold/i
const ITALIC_FONT_NAME = /italic|oblique/i
/** Monospace font names; code in PDFs is set in one of these. */
const MONO_FONT_NAME = /mono|courier|consol|menlo|monaco|inconsolata|typewriter|code|cmtt|txtt|lmtt/i
/** A line counts as code when at least this share of its characters are monospace. */
const MONO_SHARE = 0.8
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

/** Joins wrapped lines into a paragraph, undoing end-of-line hyphenation and mended URLs. */
const joinLines = (lines: readonly string[]) =>
  lines.reduce((acc, line) => {
    if (acc === "") return line
    if (URL_BREAK.test(acc) && /^[a-z0-9]/.test(line)) return acc + line
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

/** A table-of-contents entry ends in its page number; "3. Methods · 41" is not a list item. */
const isTocEntry = (text: string) => / · (?:\d+|[ivxlc]+)$/i.test(collapseLeaders(text))

/**
 * A paragraph, or a list when it opens with a bullet or a number. Each line
 * that opens with the same kind of marker starts a new item. `list` tells the
 * caller to keep it tight against neighbouring list items.
 */
const paragraphMarkdown = (lines: readonly PdfLine[]) => {
  const texts = lines.map((l) => l.text.trim()).filter((t) => t !== "")
  const first = texts[0] ?? ""
  const marker = BULLET.test(first) ? BULLET : ORDERED.test(first) && !isTocEntry(joinLines(texts)) ? ORDERED : undefined
  if (!marker) return { md: tidy(joinLines(texts)), list: false }
  const items: string[][] = []
  for (const text of texts) {
    const last = items.at(-1)
    if (marker.test(text) || !last) items.push([text])
    else last.push(text)
  }
  const md = items
    .map((item) => {
      const text = joinLines(item)
      const number = marker === ORDERED ? ORDERED.exec(text)?.[1] : undefined
      return `${number === undefined ? "-" : `${number}.`} ${tidy(text.replace(marker, ""))}`
    })
    .join("\n")
  return { md, list: true }
}

/** Code lines as set: line breaks kept, trailing spaces dropped, the shared indent removed. */
const codeText = (lines: readonly PdfLine[]) => {
  const texts = lines.map((l) => l.text.trimEnd())
  const indent = Math.min(...texts.map((t) => t.length - t.trimStart().length))
  return texts.map((t) => t.slice(indent)).join("\n")
}

/** A code fence longer than any backtick run inside, so the code can't close it early. */
const fenced = (code: string) => {
  const longest = Math.max(0, ...[...code.matchAll(/`+/g)].map((m) => m[0].length))
  const fence = "`".repeat(Math.max(3, longest + 1))
  return `${fence}\n${code}\n${fence}`
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
      if (!a || !b || !sameSize(a.size, b.size)) continue
      const delta = Math.round(b.top - a.top)
      if (delta > 0) deltas.set(delta, (deltas.get(delta) ?? 0) + 1)
    }
  }
  return mostCommon(deltas) ?? 0
}

/** What line spacing is judged against. */
type Spacing = Pick<PdfLayout, "body" | "pitch">

/** Sizes this close are one size set with a little jitter. */
const sameSize = (a: number, b: number) => Math.abs(a - b) <= 0.5

/** Top-to-top distance of wrapped lines at `size`: the body pitch, scaled up for larger type. */
const pitchAt = (spacing: Spacing, size: number) => spacing.pitch * (spacing.body > 0 ? Math.max(1, size / spacing.body) : 1)

/**
 * Whether `next` sits one line below `prev`, as the next line of the same
 * paragraph. A line starting lowercase continues the sentence, so it may sit
 * a little further away.
 */
const wraps = (prev: PdfLine, next: PdfLine, spacing: Spacing) => {
  const continues = /^[a-z]/.test(next.text.trim())
  const delta = next.top - prev.top
  return delta > 0 && delta <= pitchAt(spacing, next.size) * (continues ? 1.6 : 1.15)
}

/** mupdf can put two paragraphs in one block; split where the gap between lines is wider than a line. */
const splitByGap = (lines: readonly PdfLine[], spacing: Spacing) => {
  const groups: PdfLine[][] = []
  for (const line of lines) {
    const group = groups.at(-1)
    const prev = group?.at(-1)
    if (group && prev && (line.top <= prev.top || wraps(prev, line, spacing))) group.push(line)
    else groups.push([line])
  }
  return groups
}

/**
 * Some PDFs put every line in its own text block. Rejoins consecutive blocks
 * with the same size and boldness that sit one line pitch apart, so wrapped
 * paragraphs, list items and two-line headings come back together.
 */
const mergeWrapped = (blocks: readonly Block[], spacing: Spacing) => {
  const merged: Block[] = []
  for (const block of blocks) {
    const prev = merged.at(-1)
    const prevLast = prev?.kind === "text" ? prev.lines.at(-1) : undefined
    const first = block.kind === "text" ? block.lines[0] : undefined
    if (prev?.kind === "text" && block.kind === "text" && prevLast && first) {
      const sameStyle = sameSize(prevLast.size, first.size) && prevLast.bold === first.bold && !prevLast.mono && !first.mono
      if (sameStyle && wraps(prevLast, first, spacing)) {
        merged[merged.length - 1] = { kind: "text", lines: [...prev.lines, ...block.lines] }
        continue
      }
    }
    merged.push(block)
  }
  return merged
}

/** Lines on one row, left to right. Lines come in reading order, so a row's lines are adjacent. */
const rowsOf = (lines: readonly PdfLine[]) => {
  const rows: PdfLine[][] = []
  for (const line of lines) {
    const row = rows.at(-1)
    const head = row?.[0]
    if (row && head && Math.abs(line.top - head.top) < Math.min(line.size, head.size) * 0.5) row.push(line)
    else rows.push([line])
  }
  return rows.map((row) => [...row].sort((a, b) => a.left - b.left))
}

/** One row as one line: the runs joined by spaces. */
const joinRow = (row: readonly PdfLine[]): PdfLine[] => {
  const [head] = row
  if (!head || row.length === 1) return [...row]
  return [
    {
      ...head,
      text: row.map((l) => l.text.trim()).join(" "),
      right: Math.max(...row.map((l) => l.right)),
      bold: row.every((l) => l.bold),
      italic: row.every((l) => l.italic),
      mono: row.every((l) => l.mono),
    },
  ]
}

const joinRows = (lines: readonly PdfLine[]) => rowsOf(lines).flatMap(joinRow)

/** A row of cells: two or more runs, the first not a list marker set apart from its item. */
const isTableRow = (row: readonly PdfLine[]) => row.length >= 2 && !MARKER.test(row[0]?.text.trim() ?? "")

/** Left edges of the columns the cells line up in, left to right. */
const columnsOf = (rows: readonly (readonly PdfLine[])[]) => {
  const lefts = rows.flatMap((row) => row.map((l) => l.left)).sort((a, b) => a - b)
  const columns: number[] = []
  for (const left of lefts) {
    const last = columns.at(-1)
    if (last === undefined || left - last > COLUMN_SLACK) columns.push(left)
  }
  return columns
}

const cellText = (lines: readonly PdfLine[]) => {
  const text = joinLines(lines.map((l) => l.text.trim())).replace(/\|/g, "\\|")
  return lines.every((l) => l.bold) && text !== "" ? `**${text}**` : text
}

/**
 * Turns rows of cells into a table. A row with nothing in the first column
 * carries on the cells of the row above, wrapped onto a second line.
 */
const tableOf = (rows: readonly (readonly PdfLine[])[]): Block => {
  const columns = columnsOf(rows)
  const column = (line: PdfLine) => {
    let index = 0
    columns.forEach((left, i) => {
      if (left <= line.left + COLUMN_SLACK / 2) index = i
    })
    return index
  }
  const cells: PdfLine[][][] = []
  for (const row of rows) {
    const placed: PdfLine[][] = columns.map(() => [])
    for (const line of row) placed[column(line)]?.push(line)
    const last = cells.at(-1)
    if (last && placed[0]?.length === 0) placed.forEach((lines, i) => last[i]?.push(...lines))
    else cells.push(placed)
  }
  return { kind: "table", rows: cells.map((row) => row.map(cellText)) }
}

/**
 * Finds tables: runs of adjacent blocks whose rows hold cells lining up in
 * two or more columns. Rows in every other block join into single lines.
 */
const tabulate = (blocks: readonly PdfBlock[]) => {
  const out: Block[] = []
  let run: PdfLine[][] = []
  const flush = () => {
    const tableRows = run.filter(isTableRow)
    if (tableRows.length >= 2 && columnsOf(tableRows).length >= 2) out.push(tableOf(run))
    else if (run.length > 0) out.push({ kind: "text", lines: run.flatMap(joinRow) })
    run = []
  }
  for (const block of blocks) {
    if (block.kind !== "text") {
      flush()
      out.push(block)
      continue
    }
    const rows = rowsOf(block.lines)
    const prev = run.at(-1)?.[0]
    const first = rows[0]?.[0]
    // A table's rows sit close together; a block far below starts afresh.
    const adjacent = prev && first && first.top - prev.top < prev.size * 4
    if (rows.some(isTableRow)) {
      if (!adjacent) flush()
      run.push(...rows)
      continue
    }
    flush()
    out.push({ kind: "text", lines: rows.flatMap(joinRow) })
  }
  flush()
  return out
}

/** Text and images in reading order, with paragraphs split and rejoined. */
const arrange = (blocks: readonly Block[], spacing: Spacing) =>
  mergeWrapped(
    blocks.flatMap((block) =>
      block.kind === "text"
        ? splitBySize(block.lines).flatMap((group) => splitByGap(group, spacing).map((lines): Block => ({ kind: "text", lines })))
        : [block],
    ),
    spacing,
  )

// mupdf's structured-text JSON is parsed field by field: it crosses a WASM
// boundary and malformed entries are skipped rather than trusted.
const isRecord = (v: unknown): v is Record<string, unknown> => typeof v === "object" && v !== null
const num = (v: unknown) => (typeof v === "number" ? v : undefined)
const str = (v: unknown) => (typeof v === "string" ? v : undefined)

/** `keys[i]` locates `lines[i]` on the page, to match it with the character walk. */
type JsonBlock = { kind: "text"; lines: PdfLine[]; keys: string[] } | { kind: "image"; top: number; width: number; height: number }

/** A line's place on its page: which text block, which line in it. */
const lineKey = (textBlock: number, line: number) => `${textBlock}:${line}`

/**
 * The JSON gives each line only its first font, so a sentence that opens with
 * an inline code word looks monospace. Counts each line's characters by font
 * and returns the keys of lines that are mostly monospace.
 */
const monoLines = (stext: mupdf.StructuredText) => {
  const counts = new Map<string, { mono: number; all: number }>()
  let textBlock = -1
  let line = -1
  stext.walk({
    beginTextBlock: () => {
      textBlock++
      line = -1
    },
    beginLine: () => void line++,
    onChar: (c, _origin, font) => {
      if (c.trim() === "") return
      const key = lineKey(textBlock, line)
      const count = counts.get(key) ?? { mono: 0, all: 0 }
      count.all++
      if (font.isMono() || MONO_FONT_NAME.test(font.getName())) count.mono++
      counts.set(key, count)
    },
  })
  const keys = new Set<string>()
  for (const [key, { mono, all }] of counts) if (mono >= all * MONO_SHARE) keys.add(key)
  return keys
}

/** Parses JSON printed at `JSON_SCALE`; `width` is the page's, in points. */
const parsePageJson = (json: string, width: number) => {
  const parsed: unknown = JSON.parse(json)
  const pt = (v: unknown) => (num(v) ?? 0) / JSON_SCALE
  const blocks: JsonBlock[] = []
  if (!isRecord(parsed) || !Array.isArray(parsed.blocks)) return blocks
  let textBlock = -1
  for (const block of parsed.blocks) {
    if (!isRecord(block) || !isRecord(block.bbox)) continue
    if (block.type === "image") {
      blocks.push({ kind: "image", top: pt(block.bbox.y), width: pt(block.bbox.w), height: pt(block.bbox.h) })
      continue
    }
    if (block.type !== "text") continue
    textBlock++
    if (!Array.isArray(block.lines)) continue
    const lines: PdfLine[] = []
    const keys: string[] = []
    for (const [lineIndex, line] of block.lines.entries()) {
      if (!isRecord(line) || !isRecord(line.font) || !isRecord(line.bbox)) continue
      const text = str(line.text) ?? ""
      if (text.trim() === "") continue
      const font = str(line.font.name) ?? ""
      lines.push({
        text,
        size: round(pt(line.font.size)),
        bold: line.font.weight === "bold" || BOLD_FONT_NAME.test(font),
        italic: line.font.style === "italic" || ITALIC_FONT_NAME.test(font),
        top: pt(line.bbox.y),
        left: pt(line.bbox.x) / width,
        right: (pt(line.bbox.x) + pt(line.bbox.w)) / width,
        mono: MONO_FONT_NAME.test(font),
      })
      keys.push(lineKey(textBlock, lineIndex))
    }
    if (lines.length > 0) blocks.push({ kind: "text", lines, keys })
  }
  return blocks
}

/** mupdf can put a heading and its body in one block; split where the font size changes. */
const splitBySize = (lines: readonly PdfLine[]) => {
  const groups: PdfLine[][] = []
  for (const line of lines) {
    const group = groups.at(-1)
    const head = group?.[0]
    if (group && head && sameSize(head.size, line.size)) group.push(line)
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

/** A rendered block of markdown, with the lines it came from (none for images, code and tables). */
type Piece = { md: string; list: boolean; lines: readonly PdfLine[] }

const tableMarkdown = (rows: readonly (readonly string[])[]) => {
  const [head = [], ...body] = rows
  const row = (cells: readonly string[]) => `| ${cells.join(" | ")} |`
  return [row(head), row(head.map(() => "---")), ...body.map(row)].join("\n")
}

/**
 * Centered text: every line's middle is the page's middle, and some line is
 * short enough for that to mean something.
 */
const isCentered = (lines: readonly Pick<PdfLine, "left" | "right">[], measure: number) =>
  lines.length > 0 &&
  lines.every((l) => Math.abs((l.left + l.right) / 2 - 0.5) <= CENTER_SLACK) &&
  lines.some((l) => l.right - l.left < measure * CENTER_MAX_WIDTH)

/**
 * Joins pieces into the page's markdown. List items in a row form one list.
 * Centered text goes in a `<div align="center">` block, which the viewer
 * draws centered; a centered list is judged as a whole, since its items are
 * aligned with each other inside the centered column.
 */
const joinPieces = (pieces: readonly Piece[], measure: number) => {
  const out: string[] = []
  for (let i = 0; i < pieces.length; ) {
    let end = i + 1
    if (pieces[i]?.list) while (pieces[end]?.list) end++
    const run = pieces.slice(i, end)
    i = end
    const md = run.map((p) => p.md).join("\n")
    const lines = run.flatMap((p) => p.lines)
    const span = run[0]?.list
      ? [{ left: Math.min(...lines.map((l) => l.left)), right: Math.max(...lines.map((l) => l.right)) }]
      : lines
    out.push(lines.length > 0 && isCentered(span, measure) ? `<div align="center">\n${md}\n</div>` : md)
  }
  return out.join("\n\n")
}

/** A rectangle on the page, in PDF points. */
type Region = { x0: number; y0: number; x1: number; y1: number }

const near = (a: Region, b: Region) =>
  a.x0 - FIGURE_GAP <= b.x1 && b.x0 - FIGURE_GAP <= a.x1 && a.y0 - FIGURE_GAP <= b.y1 && b.y0 - FIGURE_GAP <= a.y1

const union = (a: Region, b: Region): Region => ({
  x0: Math.min(a.x0, b.x0),
  y0: Math.min(a.y0, b.y0),
  x1: Math.max(a.x1, b.x1),
  y1: Math.max(a.y1, b.y1),
})

/** Bounds of every filled or stroked path on a page. */
const pathBounds = (page: mupdf.Page) => {
  const regions: Region[] = []
  const hairline = new mupdf.StrokeState({ lineCap: "Butt", lineJoin: "Miter", lineWidth: 0, miterLimit: 10 })
  const add = ([x0, y0, x1, y1]: mupdf.Rect) => void regions.push({ x0, y0, x1, y1 })
  const device = new mupdf.Device({
    fillPath: (path, _evenOdd, ctm) => add(path.getBounds(hairline, ctm)),
    strokePath: (path, stroke, ctm) => add(path.getBounds(stroke, ctm)),
  })
  page.run(device, mupdf.Matrix.identity)
  device.close()
  return regions
}

/** A line's middle, in points, on a page `width` wide. */
const lineMiddle = (line: PdfLine, width: number) => ({
  x: ((line.left + line.right) / 2) * width,
  y: line.top + line.size / 2,
})

const holds = (region: Region, { x, y }: { x: number; y: number }) =>
  x >= region.x0 && x <= region.x1 && y >= region.y0 && y <= region.y1

/**
 * Vector drawings (diagrams, charts) on a page, to show as images. Shapes
 * that touch or nearly touch group into one drawing. A lone box is a shaded
 * panel and a page-sized one a backdrop, not drawings; a group holding long
 * lines of text is a boxed panel of prose.
 */
const figureRegions = (paths: readonly Region[], lines: readonly PdfLine[], width: number, height: number) => {
  const groups: { region: Region; shapes: number }[] = []
  for (const path of paths) {
    const w = path.x1 - path.x0
    const h = path.y1 - path.y0
    if (w < MIN_SHAPE_SIZE || h < MIN_SHAPE_SIZE || w * h > width * height * 0.5) continue
    let group = { region: path, shapes: 1 }
    for (let i = groups.length - 1; i >= 0; i--) {
      const other = groups[i]
      if (!other || !near(other.region, group.region)) continue
      group = { region: union(other.region, group.region), shapes: other.shapes + group.shapes }
      groups.splice(i, 1)
    }
    groups.push(group)
  }
  // Growing a group can bring it near one passed over earlier.
  for (let merged = true; merged; ) {
    merged = false
    for (let i = 0; i < groups.length && !merged; i++) {
      for (let j = i + 1; j < groups.length && !merged; j++) {
        const [a, b] = [groups[i], groups[j]]
        if (!a || !b || !near(a.region, b.region)) continue
        groups[i] = { region: union(a.region, b.region), shapes: a.shapes + b.shapes }
        groups.splice(j, 1)
        merged = true
      }
    }
  }
  return groups
    .filter(({ shapes }) => shapes >= MIN_FIGURE_SHAPES)
    .map(({ region }) => ({ x0: region.x0 - 2, y0: region.y0 - 2, x1: region.x1 + 2, y1: region.y1 + 2 }))
    .filter((region) => region.x1 - region.x0 >= MIN_IMAGE_SIZE && region.y1 - region.y0 >= MIN_IMAGE_SIZE)
    .filter((region) => !lines.some((l) => l.text.trim().length > MAX_LABEL_LENGTH && holds(region, lineMiddle(l, width))))
    .map((region) =>
      // Labels that poke out of the shapes still belong to the drawing.
      lines
        .filter((l) => {
          const [x0, x1, y0, y1] = [l.left * width, l.right * width, l.top, l.top + l.size]
          return x0 < region.x1 && x1 > region.x0 && y0 < region.y1 && y1 > region.y0
        })
        .reduce((grown, l) => union(grown, { x0: l.left * width, y0: l.top, x1: l.right * width, y1: l.top + l.size }), region),
    )
}

/** Pixel size a page region renders at. */
const regionSize = (region: Region) => {
  const scale = MAX_IMAGE_PIXELS_WIDE / (region.x1 - region.x0)
  return {
    scale,
    width: MAX_IMAGE_PIXELS_WIDE,
    height: Math.max(1, Math.round((region.y1 - region.y0) * scale)),
  }
}

/** Renders a region of a page, text and all, on white. */
const renderRegion = (page: mupdf.Page, region: Region): RawImage => {
  const { scale, width, height } = regionSize(region)
  const x = Math.round(region.x0 * scale)
  const y = Math.round(region.y0 * scale)
  const pixmap = new mupdf.Pixmap(mupdf.ColorSpace.DeviceRGB, [x, y, x + width, y + height], true)
  pixmap.clear(255)
  const device = new mupdf.DrawDevice(mupdf.Matrix.identity, pixmap)
  page.run(device, mupdf.Matrix.scale(scale, scale))
  device.close()
  return { pixels: new Uint8Array(pixmap.getPixels()), width, height, stride: pixmap.getStride() }
}

/** Indexes of `count` pages spread evenly across a document of `total` pages. */
const samplePages = (total: number, count: number) => {
  if (total <= count) return Array.from({ length: total }, (_, i) => i)
  return Array.from({ length: count }, (_, i) => Math.floor((i * total) / count))
}

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

    const spacing = { body, pitch: linePitch(pages) }
    const repeats = new Map<string, number>()
    for (const page of pages) {
      const blocks = arrange(
        page.map((b) => (b.kind === "text" ? { kind: "text", lines: joinRows(b.lines) } : b)),
        spacing,
      )
      // Running headers are set no bigger than body text; a title repeated on a cover is not one.
      const seen = new Set(
        blocks.flatMap((b) => (b.kind === "text" && blockSize(b.lines) <= body + 0.5 ? [runningKey(blockText(b.lines))] : [])),
      )
      for (const key of seen) repeats.set(key, (repeats.get(key) ?? 0) + 1)
    }
    const running = new Set(
      pages.length >= 3
        ? [...repeats].filter(([key, count]) => key.length < 80 && count >= pages.length * 0.5).map(([key]) => key)
        : [],
    )

    // Jitter makes one heading size read as two; the larger stands for both.
    const headingSizes: number[] = []
    for (const size of [...sizeWeights.keys()].filter((s) => s >= body * 1.15).sort((a, b) => b - a)) {
      if (!headingSizes.some((s) => sameSize(s, size))) headingSizes.push(size)
    }
    const widths = new Map<number, number>()
    for (const block of pages.flat()) {
      if (block.kind !== "text") continue
      for (const line of block.lines) {
        if (!sameSize(line.size, body)) continue
        const width = Math.round((line.right - line.left) * 100) / 100
        widths.set(width, (widths.get(width) ?? 0) + 1)
      }
    }
    return { ...spacing, headingSizes, running, measure: mostCommon(widths) ?? 1 }
  },

  /** Turns pages into markdown using a layout learned by `analyze`. */
  render: (pages: readonly (readonly PdfBlock[])[], layout: PdfLayout) => {
    const furniture = (text: string) => text === "" || layout.running.has(runningKey(text)) || PAGE_LABEL.test(text)
    const pieces: Piece[] = []
    // Back-to-back code blocks gather here and print as one fence.
    let code: string[] = []
    const flushCode = () => {
      if (code.length > 0) pieces.push({ md: fenced(code.join("\n\n")), list: false, lines: [] })
      code = []
    }
    for (const page of pages) {
      // Headers and footers go before tables are found, so they don't join one as a row.
      const kept = page.filter((b) => b.kind !== "text" || !furniture(collapseLeaders(blockText(joinRows(b.lines)))))
      for (const block of arrange(tabulate(kept), layout)) {
        // Code is set no bigger than body text; a large monospace line is a title.
        if (block.kind === "text" && block.lines.every((l) => l.mono) && blockSize(block.lines) <= layout.body) {
          code.push(codeText(block.lines))
          continue
        }
        flushCode()
        if (block.kind === "image") {
          pieces.push({ md: `![](${Doc.imageRef(block.id)})`, list: false, lines: [] })
          continue
        }
        if (block.kind === "note") {
          pieces.push({ md: `> *${block.text}*`, list: false, lines: [] })
          continue
        }
        if (block.kind === "table") {
          pieces.push({ md: tableMarkdown(block.rows), list: false, lines: [] })
          continue
        }
        const text = collapseLeaders(blockText(block.lines))
        if (furniture(text)) continue

        const size = blockSize(block.lines)
        const level = layout.headingSizes.findIndex((s) => sameSize(s, size))
        const { lines } = block
        if (CAPTION.test(text)) {
          pieces.push({ md: `*${text.replace(/\*/g, "\\*")}*`, list: false, lines })
        } else if (level !== -1 && text.length < 200) {
          pieces.push({ md: `${"#".repeat(Math.min(level + 1, 3))} ${text}`, list: false, lines })
        } else if (lines.every((l) => l.bold) && text.length < 100 && !BULLET.test(text) && !ORDERED.test(text)) {
          pieces.push({ md: `#### ${text}`, list: false, lines })
        } else {
          const paragraph = paragraphMarkdown(lines)
          // A paragraph set wholly in italics (a stated question, a pull quote) keeps them.
          const italic = !paragraph.list && lines.every((l) => l.italic) && !paragraph.md.includes("*")
          pieces.push({ ...paragraph, md: italic ? `*${paragraph.md}*` : paragraph.md, lines })
        }
      }
    }
    flushCode()
    return joinPieces(pieces, layout.measure)
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
        const page = document.loadPage(i)
        const [x0, y0, x1, y1] = page.getBounds()
        const [width, height] = [x1 - x0, y1 - y0]
        const stext = page.toStructuredText("preserve-images")
        const parsed = parsePageJson(stext.asJSON(JSON_SCALE), width)

        // The JSON has image positions but not pixels; a second, image-only walk
        // (cheap: no per-character callbacks) yields the image objects in the same order.
        const pageImages: mupdf.Image[] = []
        if (parsed.some((b) => b.kind === "image")) {
          stext.walk({ onImageBlock: (_bbox, _transform, image) => void pageImages.push(image) })
        }

        // Lines that only start in a monospace font get checked character by
        // character; pages without any skip the slower walk.
        const hasMono = parsed.some((b) => b.kind === "text" && b.lines.some((l) => l.mono))
        const mono = hasMono ? monoLines(stext) : new Set<string>()

        // Vector drawings show as images of their part of the page; the labels inside go with them.
        const regions = figureRegions(
          pathBounds(page),
          parsed.flatMap((b) => (b.kind === "text" ? b.lines : [])),
          width,
          height,
        )
        const inFigure = (line: PdfLine) => regions.some((region) => holds(region, lineMiddle(line, width)))

        // Each block with its top edge, so drawings can go where they sit on the page.
        const placed: { top: number; block: PdfBlock }[] = []
        let imageIndex = 0
        for (const block of parsed) {
          if (block.kind === "text") {
            const lines = block.lines
              .map((line, j) => ({ ...line, mono: line.mono && mono.has(block.keys[j] ?? "") }))
              .filter((line) => !inFigure(line))
            const first = lines[0]
            if (first) placed.push({ top: first.top, block: { kind: "text", lines } })
            continue
          }
          const image = pageImages[imageIndex++]
          if (!image || block.width < MIN_IMAGE_SIZE || block.height < MIN_IMAGE_SIZE) continue
          const id = `p${i + 1}-${imageIndex}`
          images.set(id, {
            ...decodedSize(image),
            // Decoded each time it's shown rather than kept: a book's worth of pixels doesn't fit in memory.
            // An image mupdf cannot decode is left out; the text around it still shows.
            decode: () => Result.try(() => decodeImage(image)).unwrapOr(undefined),
          })
          placed.push({ top: block.top, block: { kind: "image", id } })
        }
        regions.forEach((region, n) => {
          const id = `p${i + 1}-figure-${n + 1}`
          const { width: w, height: h } = regionSize(region)
          images.set(id, {
            width: w,
            height: h,
            // The page is loaded again to draw: pages are not kept, and a drawing is shown rarely.
            decode: () => Result.try(() => renderRegion(document.loadPage(i), region)).unwrapOr(undefined),
          })
          const at = placed.findIndex((p) => p.top >= region.y0)
          placed.splice(at === -1 ? placed.length : at, 0, { top: region.y0, block: { kind: "image", id } })
        })
        return placed.map((p) => p.block)
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

      // Markdown is small next to mupdf's page structures, so each page is read once and kept.
      const markdown = new Map<number, string>()
      const doc: Doc = {
        title: document.getMetaData("info:Title")?.trim() || fallbackTitle,
        pageCount,
        page: (i) => {
          let page = markdown.get(i)
          if (page === undefined) {
            page = Pdf.render([safeReadPage(i)], layout)
            markdown.set(i, page)
          }
          return page
        },
        images,
      }
      return doc
    }),
} as const
