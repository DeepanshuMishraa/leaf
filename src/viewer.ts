import { dirname, join } from "node:path"
import {
  addDefaultParsers,
  ASCIIFontRenderable,
  bold,
  BoxRenderable,
  createCliRenderer,
  fg,
  ImageRenderable,
  MarkdownRenderable,
  measureText,
  NativeImage,
  RGBA,
  ScrollBoxRenderable,
  SyntaxStyle,
  t,
  TextAttributes,
  TextBufferRenderable,
  TextRenderable,
  type CliRenderer,
  type KeyEvent,
  type MarkdownOptions,
  type OptimizedBuffer,
  type Renderable,
  type Selection,
} from "@opentui/core"
import { Doc, type LazyImage, type RawImage } from "./doc"
import { Highlight } from "./highlight"
import { Mono, type Rgb } from "./mono"
import { Progress, type ImageMode } from "./progress"

type MarkdownToken = Parameters<NonNullable<MarkdownOptions["renderNode"]>>[0]

/**
 * OpenTUI hides the brackets of "[text]" as if it were a link, so citations
 * like "[1]" lost theirs. Its inline markdown grammar runs with a copy of its
 * highlights that keeps them.
 */
const keepBrackets = () =>
  addDefaultParsers([
    {
      filetype: "markdown_inline",
      queries: { highlights: [join(import.meta.dir, "markdown-inline.scm")] },
      wasm: join(dirname(Bun.resolveSync("@opentui/core", import.meta.dir)), "assets/markdown_inline/tree-sitter-markdown_inline.wasm"),
    },
  ])

/** Widest the text column gets; a book-like measure is easiest to read. */
const MAX_TEXT_WIDTH = 76
/** Tallest an image gets, in rows, so one figure never fills several screens. */
const MAX_IMAGE_ROWS = 28
/** Rough pixels per terminal column, so small images aren't blown up to full width. */
const PIXELS_PER_COLUMN = 8
/** Windows at least this big show one page per screen; smaller ones scroll. */
const PAGED_MIN_WIDTH = 100
const PAGED_MIN_HEIGHT = 24
/** Ink for painted images when the terminal doesn't report its text color. */
const FALLBACK_INK: Rgb = [220, 220, 220]

// The terminal's own colors, so the reader takes on the user's theme.
const colors = {
  fg: RGBA.defaultForeground(),
  bg: RGBA.defaultBackground(),
  dim: RGBA.fromIndex(8),
  code: RGBA.fromIndex(3),
  selection: RGBA.fromIndex(8),
  // Highlighter yellow under dark ink reads the same on light and dark themes.
  mark: RGBA.fromInts(250, 214, 64),
  markInk: RGBA.fromInts(20, 20, 20),
}

/** The text blocks under `renderable` that take part in selection, in reading order. */
const textBlocks = (renderable: Renderable): TextBufferRenderable[] =>
  renderable instanceof TextBufferRenderable
    ? renderable.selectable
      ? [renderable]
      : []
    : renderable.getChildren().flatMap(textBlocks)

/**
 * Selected text is drawn in the terminal's own colors; the inverted default
 * colors OpenTUI falls back to don't show in every terminal.
 */
const showSelection = (renderable: Renderable) => {
  for (const block of textBlocks(renderable)) {
    block.selectionBg = colors.selection
    block.selectionFg = colors.fg
  }
}

const theme = (renderer: CliRenderer) =>
  SyntaxStyle.fromStyles(
    {
      default: { fg: colors.fg },
      conceal: { fg: colors.dim },
      "markup.heading": { fg: colors.fg, bold: true },
      "markup.strong": { bold: true },
      "markup.italic": { italic: true },
      "markup.strikethrough": { dim: true },
      "markup.raw": { fg: colors.code },
      "markup.link": { fg: colors.dim },
      "markup.link.label": { underline: true },
      "markup.link.url": { fg: colors.dim },
      "markup.list": { fg: colors.dim },
      "markup.quote": { fg: colors.dim, italic: true },
    },
    renderer.nativeScene,
  )

/**
 * `paged` shows one source page per screen; `scroll` stacks every page in one
 * column with the page number between them.
 */
type Layout = "paged" | "scroll"
/**
 * Shifted symbols leaf binds. With the kitty keyboard protocol, terminals can
 * report shift+; as ";" with shift rather than ":".
 */
const SHIFTED: Record<string, string> = { ";": ":", "/": "?" }

/** The character a key types. */
const typed = (key: KeyEvent) => (key.shift ? SHIFTED[key.name] : undefined) ?? key.sequence

/** What floats over the text: nothing, the key list, or the go-to-page box with what's typed so far. */
type Dialog = { kind: "none" } | { kind: "help" } | { kind: "goto"; digits: string; pageCount: number }

export type ViewerOptions = {
  startPage?: number
  onPage?: (page: number) => void
  imageMode?: ImageMode
  onImageMode?: (mode: ImageMode) => void
  highlights?: readonly Highlight[]
  /** Hears every highlight there is, each time one is added or cleared. */
  onHighlights?: (highlights: readonly Highlight[]) => void
}

/** Terminal cell size for an image, given the column budget and pixel size. */
const imageCells = (maxCols: number, width: number, height: number) => {
  const cols = Math.max(1, Math.min(maxCols, Math.ceil(width / PIXELS_PER_COLUMN)))
  // Cells are about twice as tall as wide.
  const rows = Math.max(1, Math.min(MAX_IMAGE_ROWS, Math.round((cols * height) / width / 2)))
  return { cols, rows }
}

/**
 * Kitty uploads send the image's encoded PNG when it has one and raw RGB
 * otherwise; raw is ~10x larger and gets re-sent as images scroll back into
 * view, so encode once up front.
 */
const nativeImage = (raw: RawImage, alpha: "straight" | "opaque") => {
  const image = NativeImage.fromPixels(raw.pixels, raw.width, raw.height, { stride: raw.stride, alpha })
  image.ensureEncodedPng()
  return image
}

/** Copies a loaded image's pixels out of native memory. */
const rawImage = (image: NativeImage): RawImage => {
  const raw = image.raw("rgba8")
  return { pixels: new Uint8Array(raw.data), width: raw.width, height: raw.height, stride: raw.stride }
}

const hexRgb = (hex: string | null, fallback: Rgb): Rgb => {
  if (hex === null) return fallback
  const [r, g, b] = RGBA.fromHex(hex).toInts()
  return [r, g, b]
}

/** A paragraph that holds only images (and whitespace) renders as figures. */
const imageHrefs = (token: MarkdownToken) => {
  if (token.type !== "paragraph") return []
  const hrefs: string[] = []
  for (const child of token.tokens ?? []) {
    if (child.type === "image") hrefs.push(child.href)
    else if (!(child.type === "text" && child.raw.trim() === "")) return []
  }
  return hrefs
}

/** The plain text of inline tokens, with markdown markers and escapes resolved. */
const inlineText = (tokens: readonly MarkdownToken[]): string =>
  tokens
    .map((token) => {
      if ("tokens" in token && Array.isArray(token.tokens) && token.tokens.length > 0) return inlineText(token.tokens)
      return "text" in token && typeof token.text === "string" ? token.text : ""
    })
    .join("")

type Align = "left" | "center"

/** A block of centered lines, as PDF pages mark title pages and captions. */
const CENTERED = /^<div align="center">\n([\s\S]*?)\n<\/div>\s*$/
const HEADING = /^(#{1,6}) (.+)$/

type FigureSource = { kind: "embedded"; lazy: LazyImage } | { kind: "remote"; url: string }

/**
 * An image in the document. Its holder reserves space at once; the image is
 * drawn only once the figure is near the screen, and redrawn when the image
 * mode changes. `laidOut` flips after the first paint: before it, the holder's
 * position reads 0.
 */
type Figure = {
  holder: BoxRenderable
  page: number
  source: FigureSource
  shown: boolean
  laidOut: boolean
  /** Pixels of a remote image, once fetched. */
  remote: RawImage | undefined
}

/**
 * A source page (or the whole document, for sources without pages) that is
 * built because it is on screen or near it. `index` is 0-based.
 */
type Page = { index: number; box: BoxRenderable; separator: TextRenderable | undefined }

/** Pages built on each side of the one being read in paged mode, so turning a page is instant. */
const PAGED_NEIGHBORS = 1

export const Viewer = {
  open: async (doc: Doc, options?: ViewerOptions) => {
    const renderer = await createCliRenderer({ exitOnCtrlC: true, backgroundColor: colors.bg })
    // The title names the terminal tab; on screen it would only take rows from the text.
    renderer.setTerminalTitle(doc.title)
    return Viewer.mount(renderer, doc, options)
  },

  /**
   * Builds the reader UI on an existing renderer (a real terminal or a test renderer).
   * For paged documents, `startPage` (1-based) is where reading resumes, and
   * `onPage` hears each page the reader settles on.
   */
  mount: (renderer: CliRenderer, doc: Doc, options: ViewerOptions = {}) => {
    keepBrackets()
    // `imageMode` starts figures painted or not; `onImageMode` hears each switch.
    const textWidth = () => Math.max(1, Math.min(MAX_TEXT_WIDTH, renderer.width - 4))
    const layoutFor = (): Layout =>
      doc.pageCount !== undefined && renderer.width >= PAGED_MIN_WIDTH && renderer.height >= PAGED_MIN_HEIGHT
        ? "paged"
        : "scroll"

    let layout = layoutFor()
    let imageMode: ImageMode = options.imageMode ?? "color"
    const pageTotal = Doc.pages(doc)
    /**
     * The page being read: in paged mode the one shown, when scrolling the one
     * at the top of the screen.
     */
    let current =
      doc.pageCount === undefined || options.startPage === undefined
        ? 0
        : Math.max(0, Math.min(doc.pageCount, options.startPage) - 1)
    /** Built pages, in order and without gaps; every other page exists only as its index. */
    let pages: Page[] = []
    let figures: Figure[] = []

    // The terminal's text color, asked for once, the first time an image is painted.
    let terminalInk: Promise<Rgb> | undefined
    const ink = () =>
      (terminalInk ??= renderer
        .getPalette({ timeout: 500 })
        .then((palette) => hexRgb(palette.defaultForeground, FALLBACK_INK))
        .catch(() => FALLBACK_INK))

    const pixelsOf = async (figure: Figure) => {
      const { source } = figure
      if (source.kind === "embedded") return source.lazy.decode()
      if (!figure.remote) {
        const image = await NativeImage.load(source.url)
        figure.remote = rawImage(image)
        image.dispose()
      }
      return figure.remote
    }

    const showFigure = (figure: Figure) => {
      figure.shown = true
      const mode = imageMode
      const { holder } = figure
      // An image that can't be decoded or fetched is left out; the text around it still shows.
      void Promise.all([pixelsOf(figure), mode === "mono" ? ink() : undefined])
        .then(([raw, color]) => {
          if (!raw || holder.isDestroyed || imageMode !== mode) return
          const size = imageCells(textWidth(), raw.width, raw.height)
          holder.width = size.cols
          holder.height = size.rows
          for (const child of holder.getChildren()) child.destroyRecursively()
          const image = color ? nativeImage(Mono.paint(raw, color), "straight") : nativeImage(raw, "opaque")
          holder.add(new ImageRenderable(renderer, { source: image, width: "100%", height: "100%", fit: "fit" }))
          image.dispose() // ImageRenderable retains its own reference.
        })
        .catch(() => {})
    }

    const figure = (href: string, page: number) => {
      const resolved = Doc.resolveImage(doc, href)
      if (resolved === undefined) return null
      const source: FigureSource =
        typeof resolved === "string" ? { kind: "remote", url: resolved } : { kind: "embedded", lazy: resolved }
      const size =
        source.kind === "remote" ? { cols: textWidth(), rows: 12 } : imageCells(textWidth(), source.lazy.width, source.lazy.height)
      const holder = new BoxRenderable(renderer, { width: size.cols, height: size.rows, alignSelf: "center" })
      figures.push({ holder, page, source, shown: false, laidOut: false, remote: undefined })
      return holder
    }

    /**
     * Shows figures that are on screen or close to it; the rest wait. In paged
     * mode that is every figure on the current page.
     */
    const showNearbyFigures = () => {
      const margin = scroll.viewport.height
      const top = scroll.viewport.y - margin
      const bottom = scroll.viewport.y + scroll.viewport.height + margin
      for (const figure of figures) {
        if (figure.shown || figure.holder.isDestroyed) continue
        if (layout === "paged") {
          if (figure.page === current) showFigure(figure)
          continue
        }
        if (!figure.laidOut) {
          // Runs before paint; this figure gets its position in the paint that follows.
          figure.laidOut = true
          continue
        }
        const { holder } = figure
        if (holder.y + holder.height >= top && holder.y <= bottom) showFigure(figure)
      }
    }

    /**
     * The terminal has one font size, so rank comes from size where it fits
     * (large letters for a short title), then weight and space.
     */
    const heading = (depth: number, text: string, align: Align = "left") => {
      const box = new BoxRenderable(renderer, {
        flexDirection: "column",
        width: "100%",
        alignItems: align === "center" ? "center" : "flex-start",
        marginTop: 1,
        marginBottom: 1,
      })
      if (depth === 1 && measureText({ text, font: "tiny" }).width <= textWidth()) {
        box.add(new ASCIIFontRenderable(renderer, { text, font: "tiny", color: colors.fg, selectable: false }))
        return box
      }
      box.add(
        new TextRenderable(renderer, {
          content: text,
          fg: depth <= 3 ? colors.fg : colors.dim,
          attributes: depth <= 3 ? TextAttributes.BOLD : TextAttributes.BOLD | TextAttributes.ITALIC,
          wrapMode: "word",
          width: "100%",
          textAlign: align,
        }),
      )
      showSelection(box)
      return box
    }

    /** Code as set, in the terminal's font and colors, beside a dim rule. Long lines wrap rather than vanish. */
    const codeBlock = (code: string) => {
      const box = new BoxRenderable(renderer, {
        width: "100%",
        marginBottom: 1,
        paddingLeft: 1,
        border: ["left"],
        // Heavier than the rule markdown draws beside quotes, so code and quotes read differently.
        borderStyle: "heavy",
        borderColor: colors.dim,
      })
      box.add(new TextRenderable(renderer, { content: code, fg: colors.fg, wrapMode: "char" }))
      showSelection(box)
      return box
    }

    /**
     * A centered block: each line a heading or a line of text, drawn centered.
     * A lone heading keeps a heading's spacing.
     */
    const centered = (content: string) => {
      const lines = content.split("\n")
      const [only] = lines
      const lone = only !== undefined && lines.length === 1 ? HEADING.exec(only) : null
      if (lone?.[1] && lone[2] !== undefined) return heading(lone[1].length, lone[2], "center")
      const box = new BoxRenderable(renderer, { flexDirection: "column", width: "100%", marginBottom: 1 })
      for (const line of lines) {
        const match = HEADING.exec(line)
        if (match?.[1] && match[2] !== undefined) {
          box.add(heading(match[1].length, match[2], "center"))
          continue
        }
        const italic = /^\*(.+)\*$/.exec(line)?.[1]
        box.add(
          new TextRenderable(renderer, {
            content: (italic ?? line).replace(/\\\*/g, "*").replace(/^- /, "• "),
            fg: colors.fg,
            attributes: italic === undefined ? TextAttributes.NONE : TextAttributes.ITALIC,
            wrapMode: "word",
            width: "100%",
            textAlign: "center",
          }),
        )
      }
      showSelection(box)
      return box
    }

    const syntaxStyle = theme(renderer)
    const markdownView = (content: string, page: number) =>
      new MarkdownRenderable(renderer, {
        content,
        syntaxStyle,
        fg: colors.fg,
        width: "100%",
        tableOptions: { borderColor: colors.dim, wrapMode: "word" },
        renderNode: (token, context): Renderable | null => {
          if (token.type === "heading" && "depth" in token && typeof token.depth === "number") {
            return heading(token.depth, inlineText(token.tokens ?? []))
          }
          if (token.type === "code" && "text" in token && typeof token.text === "string") return codeBlock(token.text)
          const center = token.type === "html" ? CENTERED.exec(token.raw)?.[1] : undefined
          if (center !== undefined) return centered(center)
          const hrefs = imageHrefs(token)
          if (hrefs.length === 0) {
            // Markdown spaces blocks unevenly (none after a list or code, two before code),
            // so every block owns exactly one blank line below it.
            const block = context.defaultRender()
            if (!block) return null
            block.marginBottom = 0
            showSelection(block)
            const box = new BoxRenderable(renderer, { width: "100%", marginBottom: 1 })
            box.add(block)
            return box
          }
          const box = new BoxRenderable(renderer, { flexDirection: "column", width: "100%", marginY: 1 })
          for (const href of hrefs) {
            const image = figure(href, page)
            if (image) box.add(image)
          }
          return box
        },
      })

    /** Shows only the current page in paged mode; when scrolling, every built page with its number above it. */
    const stylePage = (page: Page) => {
      page.box.visible = layout === "scroll" || page.index === current
      if (page.separator) page.separator.visible = layout === "scroll"
    }

    const buildPage = (index: number): Page => {
      const box = new BoxRenderable(renderer, { id: `page-${index}`, flexDirection: "column", width: "100%", maxWidth: MAX_TEXT_WIDTH })
      const separator =
        doc.pageCount === undefined || index === 0
          ? undefined
          : new TextRenderable(renderer, {
              content: `·  ${index + 1}  ·`,
              fg: colors.dim,
              alignSelf: "center",
              marginY: 1,
              selectable: false,
            })
      if (separator) box.add(separator)
      const markdown = doc.page(index)
      if (markdown === "") {
        box.add(
          new TextRenderable(renderer, { content: "blank page", fg: colors.dim, alignSelf: "center", marginY: 1, selectable: false }),
        )
      } else {
        box.add(markdownView(markdown, index))
      }
      // A page above the one being read that grows (just built, or still laying out) would push
      // the text on screen down; scroll by as much so the reader's place holds still.
      let height = 0
      box.onSizeChange = () => {
        const grown = box.height - height
        height = box.height
        if (layout === "scroll" && index < current && grown !== 0) scroll.scrollTop += grown
      }
      const page = { index, box, separator }
      stylePage(page)
      return page
    }

    const dropPages = (dropped: readonly Page[]) => {
      if (dropped.length === 0) return
      for (const page of dropped) page.box.destroyRecursively()
      figures = figures.filter((figure) => !figure.holder.isDestroyed)
    }

    /** Builds the pages from `first` to `last` that aren't built yet, and drops the rest. */
    const keepPages = (first: number, last: number) => {
      const from = Math.max(0, first)
      const to = Math.min(pageTotal - 1, last)
      const kept = pages.filter((page) => page.index >= from && page.index <= to)
      dropPages(pages.filter((page) => !kept.includes(page)))
      const before: Page[] = []
      for (let index = (kept[0]?.index ?? to + 1) - 1; index >= from; index--) {
        const page = buildPage(index)
        scroll.add(page.box, 0)
        before.unshift(page)
      }
      const after: Page[] = []
      for (let index = (kept.at(-1)?.index ?? from - 1) + 1; index <= to && kept.length > 0; index++) {
        const page = buildPage(index)
        scroll.add(page.box)
        after.push(page)
      }
      pages = [...before, ...kept, ...after]
    }

    /** Set when the reader asked for the end of a page that is still laying out; holds its last seen height. */
    let pendingBottom: { height: number } | undefined

    const goToPage = (index: number, at: "top" | "bottom" = "top") => {
      current = Math.max(0, Math.min(pageTotal - 1, index))
      keepPages(current - (layout === "paged" ? PAGED_NEIGHBORS : 0), current + (layout === "paged" ? PAGED_NEIGHBORS : 0))
      pages.forEach(stylePage)
      scroll.scrollTo(0)
      pendingBottom = at === "bottom" ? { height: -1 } : undefined
    }

    /** Scrolls to the end of the current page each frame until it stops growing. */
    const settleBottom = () => {
      const page = pages.find((p) => p.index === current)
      if (!pendingBottom || !page) return
      if (page.box.height === pendingBottom.height) {
        pendingBottom = undefined
        return
      }
      pendingBottom.height = page.box.height
      scroll.scrollTo(scroll.scrollHeight)
    }

    /** In scroll mode, the built page under the top of the screen. */
    const pageAtTop = () => {
      let index = pages[0]?.index ?? 0
      for (const page of pages) {
        if (page.box.y <= scroll.viewport.y + 1) index = page.index
        else break
      }
      return index
    }

    /**
     * When scrolling, keeps about a screen of built pages above the screen and
     * a few below, building one page each way per frame as the reader moves
     * and dropping pages far out of sight.
     */
    const fillScroll = () => {
      if (layout !== "scroll" || pendingBottom) return
      const first = pages[0]
      const last = pages.at(-1)
      if (!first || !last) return
      const top = scroll.viewport.y
      const height = scroll.viewport.height
      // At the very end the last page may start below the top of the screen; the book is still finished.
      current = last.index === pageTotal - 1 && atBottom() ? last.index : pageAtTop()
      const far = 4 * height
      if (pages.length > 3 && first.index < current && first.box.y + first.box.height < top - far) {
        // Its rows go away above the screen; scroll back by as many so the text stays put.
        scroll.scrollTop -= first.box.height
        pages.shift()
        dropPages([first])
        return
      }
      if (pages.length > 3 && last.index > current && last.box.y > top + height + far) {
        pages.pop()
        dropPages([last])
        return
      }
      if (last.box.y + last.box.height < top + 3 * height && last.index < pageTotal - 1) {
        const page = buildPage(last.index + 1)
        scroll.add(page.box)
        pages.push(page)
      }
      if (first.box.y > top - height && first.index > 0) {
        const page = buildPage(first.index - 1)
        scroll.add(page.box, 0)
        pages.unshift(page)
      }
    }

    let reported: number | undefined
    const reportPage = () => {
      if (doc.pageCount === undefined || !options.onPage) return
      const page = current + 1
      if (page === reported) return
      reported = page
      options.onPage(page)
    }

    const syncLayout = () => {
      const next = layoutFor()
      if (next === layout) return
      layout = next
      // Pages that just appeared (or reappeared) let their figures find their position before loading.
      for (const figure of figures) figure.laidOut = false
      goToPage(current)
    }

    const toggleImages = () => {
      imageMode = imageMode === "color" ? "mono" : "color"
      options.onImageMode?.(imageMode)
      for (const figure of figures) {
        if (figure.shown) showFigure(figure)
      }
    }

    let highlights: readonly Highlight[] = options.highlights ?? []

    /** The selection as ranges of page text, one per page it spans. */
    const selectedRanges = (selection: Selection) => {
      const selected = new Set(selection.selectedRenderables)
      const ranges: Highlight[] = []
      for (const { index, box } of pages) {
        let base = 0
        for (const block of textBlocks(box)) {
          const range = selected.has(block) ? block.getSelection() : null
          if (range) ranges.push({ page: index, start: base + range.start, end: base + range.end, text: block.getSelectedText() })
          base += block.textLength + 1
        }
      }
      return Highlight.merge(ranges)
    }

    const toggleHighlight = () => {
      const selection = renderer.getSelection()
      if (!selection) return
      highlights = Highlight.toggle(highlights, selectedRanges(selection))
      renderer.clearSelection()
      options.onHighlights?.(highlights)
    }

    /**
     * Paints highlights over the finished frame, keeping each cell's character
     * and style, on the pages in view and outside any open dialog.
     */
    const paintHighlights = (buffer: OptimizedBuffer) => {
      if (highlights.length === 0) return
      const top = scroll.viewport.y
      const bottom = top + scroll.viewport.height
      const covers = (x: number, y: number) =>
        overlay.visible && x >= panel.x && x < panel.x + panel.width && y >= panel.y && y < panel.y + panel.height
      buffer.withBuffers((cells) => {
        const paint = (block: TextBufferRenderable, from: number, to: number) => {
          const { lineStartCols, lineWidthCols } = block.lineInfo
          lineStartCols.forEach((lineStart, line) => {
            const y = block.y + line
            if (y < top || y >= bottom || y >= cells.height) return
            const end = Math.min(to, lineStart + (lineWidthCols[line] ?? 0))
            for (let col = Math.max(from, lineStart); col < end; col++) {
              const x = block.x + col - lineStart
              if (x < 0 || x >= cells.width || covers(x, y)) continue
              const cell = (y * cells.width + x) * 4
              cells.bg.set(colors.mark.buffer, cell)
              cells.fg.set(colors.markInk.buffer, cell)
            }
          })
        }
        for (const { index, box } of pages) {
          if (!box.visible || box.y >= bottom || box.y + box.height <= top) continue
          const marks = highlights.filter((highlight) => highlight.page === index)
          if (marks.length === 0) continue
          let base = 0
          for (const block of textBlocks(box)) {
            for (const mark of marks) {
              const from = Math.max(mark.start, base) - base
              const to = Math.min(mark.end, base + block.textLength) - base
              if (from < to) paint(block, from, to)
            }
            base += block.textLength + 1
          }
        }
      })
    }

    const scroll = new ScrollBoxRenderable(renderer, {
      id: "scroll",
      flexGrow: 1,
      contentOptions: { alignItems: "center", paddingX: 2, paddingTop: 1 },
    })
    // The page number in the footer says where you are; a scrollbar would only add noise.
    scroll.verticalScrollBar.visible = false

    const footer = new TextRenderable(renderer, {
      id: "footer",
      fg: colors.dim,
      wrapMode: "none",
      alignSelf: "center",
      marginY: 1,
      selectable: false,
    })

    const updateFooter = () => {
      if (doc.pageCount === undefined) {
        const max = Math.max(0, scroll.scrollHeight - scroll.viewport.height)
        footer.content = `${max === 0 ? 100 : Math.round((scroll.scrollTop / max) * 100)}%`
      } else {
        const { page, pageCount, percent } = Progress.of(current + 1, doc.pageCount)
        footer.content = `${page} / ${pageCount} · ${percent}%`
      }
    }

    const root = new BoxRenderable(renderer, { flexDirection: "column", width: "100%", height: "100%" })
    root.add(scroll)
    root.add(footer)
    renderer.root.add(root)

    // Dialogs float in a small bordered panel over the text.
    const panel = new BoxRenderable(renderer, {
      border: true,
      borderStyle: "rounded",
      borderColor: colors.dim,
      titleColor: colors.dim,
      titleAlignment: "center",
      backgroundColor: colors.bg,
      flexDirection: "column",
      paddingX: 3,
      paddingY: 1,
    })
    const overlay = new BoxRenderable(renderer, {
      position: "absolute",
      top: 0,
      left: 0,
      width: "100%",
      height: "100%",
      alignItems: "center",
      justifyContent: "center",
      zIndex: 10,
      visible: false,
    })
    overlay.add(panel)
    renderer.root.add(overlay)

    const fillPanel = (title: string, rows: Renderable[]) => {
      for (const child of panel.getChildren()) child.destroyRecursively()
      panel.title = ` ${title} `
      for (const row of rows) panel.add(row)
      overlay.visible = true
    }

    const helpRows = () => {
      const paged = doc.pageCount !== undefined
      const keys: (readonly [string, string])[] = [
        ...(paged ? [["←/→  h/l", "previous / next page"] as const] : []),
        ["j/k  ↓/↑", "scroll"],
        ["d/u", "half screen down / up"],
        ["space/b", "next / previous screen"],
        ["g/G", paged ? "first / last page" : "top / bottom"],
        ...(paged ? [["t  :", "go to page"] as const] : []),
        ["i", imageMode === "color" ? "paint images" : "show original images"],
        ["drag", "select and copy text"],
        ["y", "highlight selection, or clear one"],
        ["?", "this help"],
        ["q", "quit"],
      ]
      return keys.map(([keys, action]) => {
        const row = new BoxRenderable(renderer, { flexDirection: "row" })
        row.add(new TextRenderable(renderer, { content: keys, fg: colors.fg, attributes: TextAttributes.BOLD, width: 12 }))
        row.add(new TextRenderable(renderer, { content: action, fg: colors.dim }))
        return row
      })
    }

    const gotoRows = (digits: string, pageCount: number) => [
      new TextRenderable(renderer, {
        content: t`${bold(fg(colors.fg)(digits))}${fg(colors.fg)("▏")}${fg(colors.dim)(` / ${pageCount}`)}`,
        alignSelf: "center",
      }),
      new TextRenderable(renderer, { content: "enter go · esc cancel", fg: colors.dim, alignSelf: "center", marginTop: 1 }),
    ]

    let dialog: Dialog = { kind: "none" }
    const openDialog = (next: Dialog) => {
      dialog = next
      if (next.kind === "none") overlay.visible = false
      else if (next.kind === "help") fillPanel("Keys", helpRows())
      else fillPanel("Go to page", gotoRows(next.digits, next.pageCount))
    }

    /** Typing in the go-to box: only digits that keep it a page in the book are taken. */
    const gotoKey = (key: KeyEvent, digits: string, pageCount: number) => {
      if (key.name === "escape") return openDialog({ kind: "none" })
      if (key.name === "return" || key.name === "enter") {
        openDialog({ kind: "none" })
        if (digits !== "") goToPage(Number(digits) - 1)
        return
      }
      if (key.name === "backspace") return openDialog({ kind: "goto", digits: digits.slice(0, -1), pageCount })
      const next = digits + key.sequence
      if (/^[1-9][0-9]*$/.test(next) && Number(next) <= pageCount) openDialog({ kind: "goto", digits: next, pageCount })
    }

    const screen = () => Math.max(1, scroll.viewport.height - 2)
    const atBottom = () => scroll.scrollTop >= scroll.scrollHeight - scroll.viewport.height
    renderer.keyInput.on("keypress", (key: KeyEvent) => {
      if (dialog.kind === "goto") {
        gotoKey(key, dialog.digits, dialog.pageCount)
        updateFooter()
        return
      }
      if (dialog.kind === "help") {
        if (key.name === "escape" || key.name === "q" || typed(key) === "?") openDialog({ kind: "none" })
        return
      }
      const char = typed(key)
      if (char === "?" || char === "/") {
        openDialog({ kind: "help" })
        return
      }
      if (char === "t" || char === ":") {
        if (doc.pageCount !== undefined) openDialog({ kind: "goto", digits: "", pageCount: doc.pageCount })
        return
      }
      const paged = layout === "paged"
      switch (key.name) {
        case "escape":
          if (renderer.hasSelection) {
            renderer.clearSelection()
            return
          }
          renderer.destroy()
          return
        case "q":
          renderer.destroy()
          return
        case "y":
          toggleHighlight()
          return
        case "j":
        case "down":
          scroll.scrollBy(1)
          break
        case "k":
        case "up":
          scroll.scrollBy(-1)
          break
        case "d":
          scroll.scrollBy(Math.floor(screen() / 2))
          break
        case "u":
          scroll.scrollBy(-Math.floor(screen() / 2))
          break
        case "space":
        case "pagedown":
          // Paged: finish the page on screen, then turn it.
          if (paged && atBottom()) goToPage(current + 1)
          else scroll.scrollBy(screen())
          break
        case "b":
        case "pageup":
          if (paged && scroll.scrollTop <= 0 && current > 0) goToPage(current - 1, "bottom")
          else scroll.scrollBy(-screen())
          break
        case "l":
        case "n":
        case "right":
          goToPage(current + 1)
          break
        case "h":
        case "p":
        case "left":
          goToPage(current - 1)
          break
        case "g":
        case "home":
        case "end": {
          const toEnd = key.name === "end" || key.shift
          if (doc.pageCount === undefined) scroll.scrollTo(toEnd ? scroll.scrollHeight : 0)
          else goToPage(toEnd ? pageTotal - 1 : 0, toEnd && !paged ? "bottom" : "top")
          break
        }
        case "i":
          toggleImages()
          break
        default:
          return
      }
      updateFooter()
    })

    // Taking over the mouse turns off the terminal's own selection, so copy what's selected here.
    renderer.on("selection", (selection: Selection) => {
      const text = selection.getSelectedText()
      if (text === "") return
      try {
        renderer.copyToClipboardOSC52(text)
      } catch {
        // A terminal that refuses the clipboard write only loses the copy; the selection stays and can be highlighted.
      }
    })
    renderer.addPostProcessFn(paintHighlights)

    // Resizes and mouse wheel scrolling bypass the key handler; keep everything current.
    renderer.setFrameCallback(async () => {
      syncLayout()
      settleBottom()
      fillScroll()
      updateFooter()
      reportPage()
      showNearbyFigures()
    })
    goToPage(current)
    updateFooter()
  },
} as const
