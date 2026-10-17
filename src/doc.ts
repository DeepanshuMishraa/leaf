/** Straight RGBA pixels, ready to hand to the terminal image renderer. */
export type RawImage = { pixels: Uint8Array; width: number; height: number; stride: number }

/**
 * An embedded image whose size is known without decoding it, so the viewer can
 * reserve its space and decode only when it scrolls into view.
 */
export type LazyImage = { width: number; height: number; decode: () => RawImage | undefined }

/**
 * A readable document: markdown plus the images it references.
 * Markdown arrives in chunks so long documents show their first page at once
 * and keep filling in. Images extracted from local files (PDF) are decoded
 * only when shown, and are referenced from markdown as `![alt](leaf-image:<id>)`.
 * Remote images keep their URL.
 */
export type Doc = {
  title: string
  /** Total pages, when the source has pages. Lets the viewer show load progress. */
  pageCount: number | undefined
  chunks: AsyncIterable<DocChunk>
  images: ReadonlyMap<string, LazyImage>
}

/**
 * A piece of markdown. Paged sources (PDF) yield one chunk per page, with its
 * 1-based page number; sources without pages yield chunks with no page.
 */
export type DocChunk = { markdown: string; page: number | undefined }

const IMAGE_SCHEME = "leaf-image:"

export const Doc = {
  imageRef: (id: string) => `${IMAGE_SCHEME}${id}`,

  /** A document whose markdown is known up front. */
  whole: (title: string, markdown: string): Doc => ({
    title,
    pageCount: undefined,
    images: new Map(),
    chunks: (async function* () {
      yield { markdown, page: undefined }
    })(),
  }),

  /**
   * Resolves a markdown image href: a URL for remote images, a lazy image for
   * embedded ones, or undefined when the embedded image is missing.
   */
  resolveImage: (doc: Doc, href: string) => {
    if (!href.startsWith(IMAGE_SCHEME)) return href
    return doc.images.get(href.slice(IMAGE_SCHEME.length))
  },
} as const
