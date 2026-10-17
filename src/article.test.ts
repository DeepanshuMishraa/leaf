import { expect, test } from "bun:test"
import { Article } from "./article"

const page = (body: string) =>
  `<html><head><title>Post</title></head><body><article><h1>Post</h1>${body}${"<p>Filler sentence to look like an article, with commas, and enough text.</p>".repeat(8)}</article></body></html>`

test("relative links and images become absolute, permalinks are dropped", () => {
  const doc = Article.fromHtml(
    page(`<h2>Setup <a href="#setup">#</a></h2><p>See <a href="/docs">the docs</a>.</p><img src="img/a.png" alt="diagram">`),
    "https://example.com/blog/post",
  )
  if (doc.isErr()) throw new Error(doc.error.message)
  expect(doc.value.markdown).toContain("## Setup\n")
  expect(doc.value.markdown).toContain("[the docs](https://example.com/docs)")
  expect(doc.value.markdown).toContain("![diagram](https://example.com/blog/img/a.png)")
})

test("a page with no readable content is an error, not an empty document", () => {
  const doc = Article.fromHtml("<html><body></body></html>", "https://example.com/")
  expect(doc.isErr()).toBe(true)
})

test("an arXiv abstract link reads the paper itself", () => {
  expect(Article.paperUrl("https://arxiv.org/abs/1706.03762v7")).toBe("https://arxiv.org/pdf/1706.03762v7")
  expect(Article.paperUrl("https://example.com/abs/1")).toBe("https://example.com/abs/1")
})

// A one-page PDF; mupdf rebuilds the missing cross-reference table.
const tinyPdf = `%PDF-1.4
1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj
2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj
3 0 obj << /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >> endobj
4 0 obj << /Length 44 >> stream
BT /F1 12 Tf 20 250 Td (Hello from a paper) Tj ET
endstream endobj
5 0 obj << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> endobj
trailer << /Root 1 0 R >>
%%EOF`

test("a link that serves a PDF opens as a paged PDF, whatever its URL looks like", async () => {
  using server = Bun.serve({
    port: 0,
    fetch: () => new Response(tinyPdf, { headers: { "content-type": "application/pdf" } }),
  })
  const doc = await Article.open(`${server.url}abs/2401.00001`)
  if (doc.isErr()) throw new Error(doc.error.message)
  expect(doc.value.pageCount).toBe(1)
  let text = ""
  for await (const chunk of doc.value.chunks) text += chunk.markdown
  expect(text).toContain("Hello from a paper")
})
