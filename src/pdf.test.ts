import { expect, test } from "bun:test"
import { Pdf, type PdfBlock, type PdfLine } from "./pdf"

const line = (text: string, top: number, size = 11, bold = false): PdfLine => ({ text, top, size, bold })
const text = (...lines: PdfLine[]): PdfBlock => ({ kind: "text", lines })

test("larger font sizes become headings ranked by size", () => {
  const md = Pdf.toMarkdown([
    [
      text(line("Big Title", 10, 24)),
      text(line("Section", 50, 16)),
      text(line("Body text that is long enough to be the most common size.", 80)),
      text(line("More body text so body clearly dominates the weights.", 120)),
    ],
  ])
  expect(md).toStartWith("# Big Title\n\n## Section\n\n")
})

test("one-line blocks one pitch apart rejoin into a paragraph, undoing hyphenation", () => {
  const md = Pdf.toMarkdown([
    [
      text(line("The quick brown fox jumps over the lazy dog and keeps run-", 0)),
      text(line("ning until the end.", 20)),
      text(line("A new paragraph starts after a bigger gap here.", 50)),
      text(line("It wraps onto a second line at the normal pitch.", 70)),
    ],
  ])
  expect(md).toBe(
    "The quick brown fox jumps over the lazy dog and keeps running until the end.\n\n" +
      "A new paragraph starts after a bigger gap here. It wraps onto a second line at the normal pitch.",
  )
})

test("a bold line is not merged into the body text below it", () => {
  const md = Pdf.toMarkdown([
    [
      text(line("Part 1: The Crash", 0, 11, true)),
      text(line("The phone didn't just ring; it screamed loudly at night.", 20)),
      text(line("Another line of body copy at the same pitch as before.", 40)),
    ],
  ])
  expect(md).toStartWith("#### Part 1: The Crash\n\n")
})

test("bullet glyphs become markdown list items", () => {
  const md = Pdf.toMarkdown([[text(line("●First item", 0), line("●Second item", 20), line("continues here", 40))]])
  expect(md).toBe("- First item\n- Second item continues here")
})

test("text repeated on most pages is dropped as a running header", () => {
  const bodies = ["Alpha starts the story.", "Bravo raises the stakes.", "Charlie turns it.", "Delta ends it."]
  const pages = bodies.map((body, i) => [
    text(line(`Page ${i + 1}`, 0)),
    text(line(body, 100), line("It wraps onto a second line.", 120), line("And a third one.", 140)),
  ])
  const md = Pdf.toMarkdown(pages)
  expect(md).not.toContain("Page ")
  expect(md).toContain("Charlie turns it.")
})

test("image blocks become markdown image references", () => {
  const md = Pdf.toMarkdown([[{ kind: "image", id: "p1-0" }]])
  expect(md).toBe("![](leaf-image:p1-0)")
})

test("page labels like \"xiv | Preface\" and bare page numbers are dropped", () => {
  const md = Pdf.toMarkdown([
    [
      text(line("Body text that is long enough to be the most common size here.", 0)),
      text(line("xiv | Preface", 100, 9)),
      text(line("Table of Contents | ix", 120, 9)),
      text(line("57", 140, 9)),
    ],
  ])
  expect(md).toBe("Body text that is long enough to be the most common size here.")
})

test("a hyphen glyph mapped to \"!\" is undone like a hyphen", () => {
  const md = Pdf.toMarkdown([[text(line("bombarded with buzz!", 0), line("words relating to storage.", 20))]])
  expect(md).toBe("bombarded with buzzwords relating to storage.")
})

test("table-of-contents leaders collapse and chapter numbers stay unescaped digits", () => {
  const md = Pdf.toMarkdown([[text(line("10. Batch Processing. . . . . . . . . . . . . .  389", 0))]])
  expect(md).toBe("\u206010. Batch Processing · 389")
})

test("table-of-contents entries set apart by wide gaps get a line each", () => {
  const md = Pdf.toMarkdown([[text(line("Total Order Broadcast        348 Distributed Transactions        352", 0))]])
  expect(md).toBe("Total Order Broadcast · 348\nDistributed Transactions · 352")
})
