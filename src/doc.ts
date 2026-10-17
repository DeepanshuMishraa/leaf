/** Straight RGBA pixels, ready to hand to the terminal image renderer. */
export type RawImage = { pixels: Uint8Array; width: number; height: number; stride: number }

/**
 * An embedded image whose size is known without decoding it, so the viewer can
 * reserve its space and decode only when it scrolls into view.
 */
export type LazyImage = { width: number; height: number; decode: () => RawImage | undefined }

/**
 * A readable document: markdown plus the images it references.
 * Pages are read on demand, so a book of thousands of pages opens at once and
 * only the pages being read cost anything. Images extracted from local files
 * (PDF) are decoded only when shown, and are referenced from markdown as
 * `![alt](leaf-image:<id>)`. Remote images keep their URL.
 */
export type Doc = {
  title: string
  /** Total pages, when the source has pages; a source without pages is one page. */
  pageCount: number | undefined
  /** Markdown of the page at `index` (0-based). */
  page: (index: number) => string
  /** Embedded images of the pages read so far. */
  images: ReadonlyMap<string, LazyImage>
}

const IMAGE_SCHEME = "leaf-image:"

export const Doc = {
  imageRef: (id: string) => `${IMAGE_SCHEME}${id}`,

  /** A document whose markdown is known up front. */
  whole: (title: string, markdown: string): Doc => ({
    title,
    pageCount: undefined,
    images: new Map(),
    page: () => markdown,
  }),

  /** How many pages the viewer lays out. */
  pages: (doc: Doc) => doc.pageCount ?? 1,

  /**
   * Resolves a markdown image href: a URL for remote images, a lazy image for
   * embedded ones, or undefined when the embedded image is missing.
   */
  resolveImage: (doc: Doc, href: string) => {
    if (!href.startsWith(IMAGE_SCHEME)) return href
    return doc.images.get(href.slice(IMAGE_SCHEME.length))
  },
} as const
