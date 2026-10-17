import {
  ASCIIFontRenderable,
  BoxRenderable,
  createCliRenderer,
  ImageRenderable,
  MarkdownRenderable,
  measureText,
  NativeImage,
  RGBA,
  ScrollBoxRenderable,
  SyntaxStyle,
  TextAttributes,
  TextRenderable,
  type CliRenderer,
  type KeyEvent,
  type MarkdownOptions,
  type Renderable,
} from "@opentui/core"
import { Doc, type LazyImage, type RawImage } from "./doc"
import { Mono, type Rgb } from "./mono"

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
/** How figures draw: as the image itself, or painted in the terminal's text color on a transparent ground. */
type ImageMode = "color" | "mono"

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
  open: async (doc: Doc) => Viewer.mount(await createCliRenderer({ exitOnCtrlC: true, backgroundColor: colors.bg }), doc),

  /** Builds the reader UI on an existing renderer (a real terminal or a test renderer). */
  mount: (renderer: CliRenderer, doc: Doc) => {
    const textWidth = () => Math.max(1, Math.min(MAX_TEXT_WIDTH, renderer.width - 4))
    const layoutFor = (): Layout =>
      doc.pageCount !== undefined && renderer.width >= PAGED_MIN_WIDTH && renderer.height >= PAGED_MIN_HEIGHT
        ? "paged"
        : "scroll"

    let layout = layoutFor()
    let imageMode: ImageMode = "color"
    let showKeys = false
    /** Index into `pages` of the page being read. */
    let current = 0
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
          const hrefs = imageHrefs(token)
          if (hrefs.length === 0) {
            if (token.type !== "paragraph") return null
            // Markdown runs consecutive paragraphs together; a blank line keeps them apart.
            const paragraph = context.defaultRender()
            if (!paragraph) return null
            const box = new BoxRenderable(renderer, { width: "100%", marginBottom: 1 })
            box.add(paragraph)
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

    const goToPage = (index: number, at: "top" | "bottom" = "top") => {
      const target = Math.max(0, Math.min(pages.length - 1, index))
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

    // Switching from paged to scroll jumps to the page being read once the stack has laid out.
    let pendingJump: number | undefined
    const syncLayout = () => {
      if (layout === "scroll" && pendingJump !== undefined) {
        goToPage(pendingJump)
        pendingJump = undefined
      }
      const next = layoutFor()
      if (next === layout) return
      if (next === "paged") current = pageAtTop()
      else pendingJump = current
      layout = next
      // Pages that just appeared (or reappeared) let their figures find their position before loading.
      for (const figure of figures) figure.laidOut = false
      pages.forEach(stylePage)
      if (layout === "paged") scroll.scrollTo(0)
    }

    const toggleImages = () => {
      imageMode = imageMode === "color" ? "mono" : "color"
      for (const figure of figures) {
        if (figure.shown) showFigure(figure)
      }
    }

    const title = new TextRenderable(renderer, {
      id: "title",
      content: doc.title,
      fg: colors.dim,
      wrapMode: "none",
      alignSelf: "center",
      marginY: 1,
    })

    const scroll = new ScrollBoxRenderable(renderer, {
      id: "scroll",
      flexGrow: 1,
      contentOptions: { alignItems: "center", paddingX: 2 },
    })
    // The page number in the footer says where you are; a scrollbar would only add noise.
    scroll.verticalScrollBar.visible = false

    const footer = new TextRenderable(renderer, { id: "footer", fg: colors.dim, wrapMode: "none", alignSelf: "center", marginY: 1 })

    const updateFooter = () => {
      if (showKeys) {
        const images = imageMode === "color" ? "i painted" : "i original"
        footer.content =
          layout === "paged"
            ? `←/→ page   j/k scroll   space next   ${images}   q quit`
            : `j/k scroll   space page   ${images}   q quit`
        return
      }
      if (doc.pageCount === undefined) {
        const max = Math.max(0, scroll.scrollHeight - scroll.viewport.height)
        footer.content = `${max === 0 ? 100 : Math.round((scroll.scrollTop / max) * 100)}%`
      } else {
        footer.content = `${(layout === "scroll" ? pageAtTop() : current) + 1} / ${doc.pageCount}`
      }
    }

    const loaded = (async () => {
      for await (const chunk of doc.chunks) {
        if (renderer.isDestroyed) return
        addPage(chunk.markdown, chunk.page)
      }
    })()

    const root = new BoxRenderable(renderer, { flexDirection: "column", width: "100%", height: "100%" })
    root.add(title)
    root.add(scroll)
    root.add(footer)
    renderer.root.add(root)

    const screen = () => Math.max(1, scroll.viewport.height - 2)
    const atBottom = () => scroll.scrollTop >= scroll.scrollHeight - scroll.viewport.height
    renderer.keyInput.on("keypress", (key: KeyEvent) => {
      const paged = layout === "paged"
      const here = paged ? current : pageAtTop()
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
          showKeys = !showKeys
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
      showNearbyFigures()
    })
    updateFooter()
    return { loaded }
  },
} as const
