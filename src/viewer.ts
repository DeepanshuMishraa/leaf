import {
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
  TextRenderable,
  type CliRenderer,
  type KeyEvent,
  type MarkdownOptions,
  type Renderable,
} from "@opentui/core"
import { Doc, type LazyImage, type RawImage } from "./doc"
import { Mono, type Rgb } from "./mono"
import { Progress, type ImageMode } from "./progress"

type MarkdownToken = Parameters<NonNullable<MarkdownOptions["renderNode"]>>[0]

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
/** What floats over the text: nothing, the key list, or the go-to-page box with what's typed so far. */
type Dialog = { kind: "none" } | { kind: "help" } | { kind: "goto"; digits: string; pageCount: number }

export type ViewerOptions = {
  startPage?: number
  onPage?: (page: number) => void
  imageMode?: ImageMode
  onImageMode?: (mode: ImageMode) => void
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

/** A source page (or the whole document, for sources without pages). */
type Page = { number: number | undefined; box: BoxRenderable; separator: TextRenderable | undefined }

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
    // `imageMode` starts figures painted or not; `onImageMode` hears each switch.
    const textWidth = () => Math.max(1, Math.min(MAX_TEXT_WIDTH, renderer.width - 4))
    const layoutFor = (): Layout =>
      doc.pageCount !== undefined && renderer.width >= PAGED_MIN_WIDTH && renderer.height >= PAGED_MIN_HEIGHT
        ? "paged"
        : "scroll"

    let layout = layoutFor()
    let imageMode: ImageMode = options.imageMode ?? "color"
    /** Index into `pages` of the page being read. */
    let current =
      doc.pageCount === undefined || options.startPage === undefined
        ? 0
        : Math.max(0, Math.min(doc.pageCount, options.startPage) - 1)
    const pages: Page[] = []
    const figures: Figure[] = []

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
    const heading = (depth: number, text: string) => {
      const box = new BoxRenderable(renderer, { flexDirection: "column", width: "100%", marginTop: 1, marginBottom: 1 })
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
        }),
      )
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
          const hrefs = imageHrefs(token)
          if (hrefs.length === 0) {
            // Markdown spaces blocks unevenly (none after a list or code, two before code),
            // so every block owns exactly one blank line below it.
            const block = context.defaultRender()
            if (!block) return null
            block.marginBottom = 0
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

    /** Shows only the current page in paged mode; when scrolling, every page with its number above it. */
    const stylePage = (page: Page, index: number) => {
      page.box.visible = layout === "scroll" || index === current
      if (page.separator) page.separator.visible = layout === "scroll"
    }

    const addPage = (markdown: string, number: number | undefined) => {
      const index = pages.length
      const box = new BoxRenderable(renderer, { id: `page-${index}`, flexDirection: "column", width: "100%", maxWidth: MAX_TEXT_WIDTH })
      const separator =
        number === undefined || index === 0
          ? undefined
          : new TextRenderable(renderer, { content: `·  ${number}  ·`, fg: colors.dim, alignSelf: "center", marginY: 1 })
      if (separator) box.add(separator)
      if (markdown === "") {
        box.add(new TextRenderable(renderer, { content: "blank page", fg: colors.dim, alignSelf: "center", marginY: 1 }))
      } else {
        box.add(markdownView(markdown, index))
      }
      const page = { number, box, separator }
      pages.push(page)
      stylePage(page, index)
      scroll.add(box)
    }

    /** Pages that haven't streamed in yet show once they arrive. */
    const goToPage = (index: number, at: "top" | "bottom" = "top") => {
      const target = Math.max(0, Math.min((doc.pageCount ?? pages.length) - 1, index))
      if (layout === "paged") {
        const previous = pages[current]
        if (previous) previous.box.visible = false
        current = target
        const next = pages[current]
        if (next) next.box.visible = true
        scroll.scrollTo(at === "top" ? 0 : scroll.scrollHeight)
        return
      }
      const page = pages[target]
      if (page) scroll.scrollTo(scroll.scrollTop + page.box.y - scroll.viewport.y)
      else pendingJump = target
    }

    /** In scroll mode, the page under the top of the screen. */
    const pageAtTop = () => {
      let index = 0
      for (let i = 0; i < pages.length; i++) {
        const page = pages[i]
        if (page && page.box.y <= scroll.viewport.y + 1) index = i
        else break
      }
      return index
    }

    /** Index of the page being read; while a jump waits, the page it will land on. */
    const pageHere = () => (layout === "paged" ? current : (pendingJump ?? pageAtTop()))

    let reported: number | undefined
    const reportPage = () => {
      if (doc.pageCount === undefined || !options.onPage) return
      const page = pageHere() + 1
      if (page === reported) return
      reported = page
      options.onPage(page)
    }

    // Switching from paged to scroll jumps to the page being read once the stack has laid out.
    // Opening at a saved page while scrolling waits the same way, for that page to arrive.
    let pendingJump: number | undefined = layout === "scroll" && current > 0 ? current : undefined
    /** Pages present at the previous frame, and so laid out by now. */
    let laidOutPages = 0
    const syncLayout = () => {
      if (layout === "scroll" && pendingJump !== undefined && pendingJump < laidOutPages) {
        goToPage(pendingJump)
        pendingJump = undefined
      }
      laidOutPages = pages.length
      const next = layoutFor()
      if (next === layout) return
      if (next === "paged") {
        current = pageHere()
        pendingJump = undefined
      } else pendingJump = current
      layout = next
      // Pages that just appeared (or reappeared) let their figures find their position before loading.
      for (const figure of figures) figure.laidOut = false
      pages.forEach(stylePage)
      if (layout === "paged") scroll.scrollTo(0)
    }

    const toggleImages = () => {
      imageMode = imageMode === "color" ? "mono" : "color"
      options.onImageMode?.(imageMode)
      for (const figure of figures) {
        if (figure.shown) showFigure(figure)
      }
    }

    const scroll = new ScrollBoxRenderable(renderer, {
      id: "scroll",
      flexGrow: 1,
      contentOptions: { alignItems: "center", paddingX: 2, paddingTop: 1 },
    })
    // The page number in the footer says where you are; a scrollbar would only add noise.
    scroll.verticalScrollBar.visible = false

    const footer = new TextRenderable(renderer, { id: "footer", fg: colors.dim, wrapMode: "none", alignSelf: "center", marginY: 1 })

    const updateFooter = () => {
      if (doc.pageCount === undefined) {
        const max = Math.max(0, scroll.scrollHeight - scroll.viewport.height)
        footer.content = `${max === 0 ? 100 : Math.round((scroll.scrollTop / max) * 100)}%`
      } else {
        const { page, pageCount, percent } = Progress.of(pageHere() + 1, doc.pageCount)
        footer.content = `${page} / ${pageCount} · ${percent}%`
      }
    }

    const loaded = (async () => {
      for await (const chunk of doc.chunks) {
        if (renderer.isDestroyed) return
        addPage(chunk.markdown, chunk.page)
      }
    })()

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
        ...(paged ? [[":", "go to page"] as const] : []),
        ["i", imageMode === "color" ? "paint images" : "show original images"],
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
        if (key.name === "escape" || key.name === "q" || key.sequence === "?") openDialog({ kind: "none" })
        return
      }
      const paged = layout === "paged"
      const here = pageHere()
      switch (key.name) {
        case "q":
        case "escape":
          renderer.destroy()
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
          goToPage(here + 1)
          break
        case "h":
        case "p":
        case "left":
          goToPage(here - 1)
          break
        case "g":
        case "home":
        case "end": {
          const toEnd = key.name === "end" || key.shift
          if (paged) goToPage(toEnd ? pages.length - 1 : 0)
          else scroll.scrollTo(toEnd ? scroll.scrollHeight : 0)
          break
        }
        case "i":
          toggleImages()
          break
        case "?":
        case "/":
          openDialog({ kind: "help" })
          break
        case ":":
          if (doc.pageCount !== undefined) openDialog({ kind: "goto", digits: "", pageCount: doc.pageCount })
          break
        default:
          return
      }
      updateFooter()
    })

    // Resizes and mouse wheel scrolling bypass the key handler; keep everything current.
    renderer.setFrameCallback(async () => {
      syncLayout()
      updateFooter()
      reportPage()
      showNearbyFigures()
    })
    updateFooter()
    return { loaded }
  },
} as const
