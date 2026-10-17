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
