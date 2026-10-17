import { Readability } from "@mozilla/readability"
import { parseHTML } from "linkedom"
import TurndownService from "turndown"
import { Result, TaggedError } from "better-result"
import { Doc } from "./doc"
import { Pdf } from "./pdf"

export class ArticleFetchFailed extends TaggedError("ArticleFetchFailed")<{
  url: string
  cause: unknown
  message: string
}> {}

export class ArticleHttpError extends TaggedError("ArticleHttpError")<{
  url: string
  status: number
  message: string
}> {}

export class ArticleNotReadable extends TaggedError("ArticleNotReadable")<{
  url: string
  message: string
}> {}

/** Rewrites relative src/href attributes to absolute URLs so images load and links open. */
const absolutize = (root: ReturnType<typeof parseHTML>["document"], base: string) => {
  for (const [selector, attr] of [
    ["img[src]", "src"],
    ["a[href]", "href"],
  ] as const) {
    for (const el of root.querySelectorAll(selector)) {
      const value = el.getAttribute(attr)
      if (value === null) continue
      const url = URL.parse(value, base)
      if (url) el.setAttribute(attr, url.href)
    }
  }
}

const isPdf = (response: Response, url: string) => {
  const type = response.headers.get("content-type")?.split(";")[0]?.trim().toLowerCase()
  if (type === "application/pdf") return true
  // Some servers label PDFs as generic binary; fall back to the extension.
  return type === "application/octet-stream" && new URL(url).pathname.toLowerCase().endsWith(".pdf")
}

/** A title for a PDF without one: the file name from its URL, or the host. */
const pdfTitle = (url: string) => {
  const { pathname, hostname } = new URL(url)
  const last = decodeURIComponent(pathname.split("/").filter(Boolean).at(-1) ?? "")
  return last.replace(/\.pdf$/i, "") || hostname
}

const turndown = new TurndownService({ headingStyle: "atx", codeBlockStyle: "fenced", bulletListMarker: "-" })

// Heading permalinks ("#", "¶", "🔗") are page chrome, not content.
turndown.addRule("permalink", {
  filter: (node) => node.nodeName === "A" && /^\s*[#¶🔗]?\s*$/u.test(node.textContent ?? ""),
  replacement: () => "",
})

export const Article = {
  /** Converts article HTML to a Doc. Exposed separately from `open` so it can be tested without a network. */
  fromHtml: (html: string, url: string) => {
    const { document } = parseHTML(html)
    absolutize(document, url)
    const parsed = new Readability(document).parse()
    if (!parsed?.content) {
      return Result.err(
        new ArticleNotReadable({
          url,
          message: `Found no article text at ${url}. The page may build its content with JavaScript or sit behind a login. Try opening it in a browser.`,
        }),
      )
    }
    const title = parsed.title?.trim() || new URL(url).hostname
    // Removing permalinks leaves trailing spaces on headings.
    const markdown = turndown.turndown(parsed.content).replace(/^(#{1,6} .*?)[ \t]+$/gm, "$1")
    return Result.ok({ doc: Doc.whole(title, markdown), markdown })
  },

  /** Abstract pages hold only the summary; point them at the full paper. */
  paperUrl: (url: string) => {
    const parsed = URL.parse(url)
    if (parsed?.hostname !== "arxiv.org" || !parsed.pathname.startsWith("/abs/")) return url
    parsed.pathname = parsed.pathname.replace(/^\/abs\//, "/pdf/")
    return parsed.href
  },

  open: async (link: string) => {
    const url = Article.paperUrl(link)
    const response = await Result.tryPromise({
      try: () => fetch(url, { headers: { "user-agent": "Mozilla/5.0 (leaf reader)" }, redirect: "follow" }),
      catch: (cause) =>
        new ArticleFetchFailed({
          url,
          cause,
          message: `Could not reach ${url}. Check your connection and the URL, then try again.`,
        }),
    })
    if (response.isErr()) return response
    if (!response.value.ok) {
      return Result.err(
        new ArticleHttpError({
          url,
          status: response.value.status,
          message: `${url} returned HTTP ${response.value.status}. The page may have moved, need a login or block automated requests.`,
        }),
      )
    }
    const finalUrl = response.value.url || url
    // Paper links often serve a PDF from a URL without ".pdf"; trust the content type.
    if (isPdf(response.value, finalUrl)) {
      const bytes = await Result.tryPromise({
        try: async () => new Uint8Array(await response.value.arrayBuffer()),
        catch: (cause) =>
          new ArticleFetchFailed({
            url,
            cause,
            message: `The connection to ${url} dropped while downloading the PDF. Try again.`,
          }),
      })
      return bytes.andThen((pdf) => Pdf.fromBytes(pdf, url, pdfTitle(finalUrl)))
    }
    const html = await Result.tryPromise({
      try: () => response.value.text(),
      catch: (cause) =>
        new ArticleFetchFailed({
          url,
          cause,
          message: `The connection to ${url} dropped while reading the page. Try again.`,
        }),
    })
    return html.andThen((text) => Article.fromHtml(text, finalUrl)).map(({ doc }) => doc)
  },
} as const
