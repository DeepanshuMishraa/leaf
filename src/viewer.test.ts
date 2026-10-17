import { afterEach, expect, test } from "bun:test"
import { createTestRenderer } from "@opentui/core/testing"
import { Doc } from "./doc"
import { Viewer } from "./viewer"

let setup: Awaited<ReturnType<typeof createTestRenderer>> | undefined

afterEach(() => {
  setup?.renderer.destroy()
  setup = undefined
})

const mount = async (markdown: string) => {
  setup = await createTestRenderer({ width: 80, height: 20 })
  const { loaded } = Viewer.mount(setup.renderer, Doc.whole("Sample", markdown))
  await loaded
  // Markdown lays out across frames; give it a beat before capturing.
  await setup.renderOnce()
  await Bun.sleep(50)
  await setup.renderOnce()
  return setup
}

test("shows rendered markdown without a title line, with the keys only on ?", async () => {
  const s = await mount("## Intro\n\nSome **bold** words and a [link](https://example.com).")
  const frame = s.captureCharFrame()
  // The title goes to the terminal tab, not the screen.
  expect(frame).not.toContain("Sample")
  expect(frame).toContain("Intro")
  expect(frame).toContain("bold")
  expect(frame).not.toContain("**")
  expect(frame).not.toContain("quit")
  s.mockInput.pressKey("?")
  await s.renderOnce()
  expect(s.captureCharFrame()).toContain("quit")
})

test("? lists every key in a panel, and closes on escape, ? or q without quitting", async () => {
  const s = await mountBook(["Alpha text", "Bravo text"], 120, 30)
  for (const close of ["escape", "?", "q"]) {
    s.mockInput.pressKey("?")
    await s.renderOnce()
    const frame = s.captureCharFrame()
    for (const action of ["next page", "scroll", "go to page", "first / last page", "paint images", "this help", "quit"]) {
      expect(frame).toContain(action)
    }
    if (close === "escape") s.mockInput.pressEscape()
    else s.mockInput.pressKey(close)
    await settle(s)
    expect(s.renderer.isDestroyed).toBe(false)
    expect(s.captureCharFrame()).not.toContain("this help")
  }
})

test("a numbered list item from a PDF keeps its number and no stray backslash", async () => {
  const frame = (await mount("\u206010. Batch Processing · 389")).captureCharFrame()
  expect(frame).toContain("10. Batch Processing · 389")
  expect(frame).not.toContain("\\")
})

test("streamed chunks all show up, in order", async () => {
  setup = await createTestRenderer({ width: 80, height: 20 })
  const doc: Doc = {
    title: "Book",
    pageCount: 3,
    images: new Map(),
    chunks: (async function* () {
      yield { markdown: "First page", page: 1 }
      yield { markdown: "Second page", page: 2 }
      yield { markdown: "Third page", page: 3 }
    })(),
  }
  const { loaded } = Viewer.mount(setup.renderer, doc)
  await loaded
  await setup.renderOnce()
  await Bun.sleep(50)
  await setup.renderOnce()
  const frame = setup.captureCharFrame()
  expect(frame.indexOf("First page")).toBeLessThan(frame.indexOf("Second page"))
  expect(frame.indexOf("Second page")).toBeLessThan(frame.indexOf("Third page"))
  expect(frame).not.toContain("loading")
})

test("j scrolls the document down", async () => {
  const lines = Array.from({ length: 60 }, (_, i) => `Paragraph ${i}`).join("\n\n")
  const s = await mount(lines)
  expect(s.captureCharFrame()).toContain("Paragraph 0")
  for (let i = 0; i < 10; i++) s.mockInput.pressKey("j")
  await s.renderOnce()
  expect(s.captureCharFrame()).not.toContain("Paragraph 0\n")
})

const book = (pages: string[]): Doc => ({
  title: "Book",
  pageCount: pages.length,
  images: new Map(),
  chunks: (async function* () {
    for (const [i, markdown] of pages.entries()) yield { markdown, page: i + 1 }
  })(),
})

const mountBook = async (pages: string[], width: number, height: number, options?: Parameters<typeof Viewer.mount>[2]) => {
  setup = await createTestRenderer({ width, height })
  const { loaded } = Viewer.mount(setup.renderer, book(pages), options)
  await loaded
  await setup.renderOnce()
  await Bun.sleep(50)
  await setup.renderOnce()
  return setup
}

test("a wide window shows one page at a time, and → turns the page", async () => {
  const s = await mountBook(["Alpha text", "Bravo text", "Charlie text"], 120, 30)
  let frame = s.captureCharFrame()
  expect(frame).toContain("Alpha text")
  expect(frame).not.toContain("Bravo text")
  expect(frame).toContain("1 / 3")
  s.mockInput.pressArrow("right")
  // The page that just appeared lays out across frames.
  await s.renderOnce()
  await Bun.sleep(50)
  await s.renderOnce()
  frame = s.captureCharFrame()
  expect(frame).not.toContain("Alpha text")
  expect(frame).toContain("Bravo text")
  expect(frame).toContain("2 / 3")
})

test("the footer shows how much of the book is read", async () => {
  const s = await mountBook(["A", "B", "C", "D"], 120, 30)
  expect(s.captureCharFrame()).toContain("1 / 4 · 25%")
})

test("opens at a saved page and reports each page it lands on", async () => {
  const seen: number[] = []
  const s = await mountBook(["Alpha text", "Bravo text", "Charlie text"], 120, 30, {
    startPage: 2,
    onPage: (page) => seen.push(page),
  })
  let frame = s.captureCharFrame()
  expect(frame).toContain("Bravo text")
  expect(frame).not.toContain("Alpha text")
  expect(frame).toContain("2 / 3")
  s.mockInput.pressArrow("right")
  await s.renderOnce()
  frame = s.captureCharFrame()
  expect(frame).toContain("3 / 3 · 100%")
  expect(seen).toEqual([2, 3])
})

test("a narrow window scrolls to the saved page once it loads", async () => {
  const filler = (name: string) => Array.from({ length: 20 }, (_, i) => `${name} line ${i}`).join("\n\n")
  const s = await mountBook([filler("Alpha"), filler("Bravo"), filler("Charlie")], 70, 30, { startPage: 3 })
  await s.renderOnce()
  await Bun.sleep(50)
  await s.renderOnce()
  const frame = s.captureCharFrame()
  expect(frame).toContain("Charlie line 0")
  expect(frame).toContain("3 / 3")
})

test("a narrow window stacks the pages with the page number between them", async () => {
  const s = await mountBook(["Alpha text", "Bravo text"], 70, 30)
  const frame = s.captureCharFrame()
  expect(frame).toContain("Alpha text")
  expect(frame).toContain("Bravo text")
  expect(frame).toContain("·  2  ·")
})

test("consecutive paragraphs are separated by a blank line", async () => {
  const { captureCharFrame } = await mount("First paragraph.\n\nSecond paragraph.")
  const lines = captureCharFrame().split("\n")
  const first = lines.findIndex((l) => l.includes("First paragraph."))
  const second = lines.findIndex((l) => l.includes("Second paragraph."))
  expect(second - first).toBe(2)
})

test("a short top-level heading is drawn in large letters", async () => {
  const { captureCharFrame } = await mount("# Intro\n\nBody.")
  const lines = captureCharFrame().split("\n")
  // The large font spans more than one row of block glyphs.
  expect(lines.filter((l) => /[█▀▄]/.test(l)).length).toBeGreaterThan(1)
})

test("i switches images between original and painted", async () => {
  const s = await mount("Text")
  s.mockInput.pressKey("?")
  await s.renderOnce()
  expect(s.captureCharFrame()).toContain("paint images")
  s.mockInput.pressEscape()
  await settle(s)
  s.mockInput.pressKey("i")
  s.mockInput.pressKey("?")
  await s.renderOnce()
  expect(s.captureCharFrame()).toContain("show original images")
})

test("starts in the saved image mode and reports when i changes it", async () => {
  const modes: string[] = []
  setup = await createTestRenderer({ width: 80, height: 20 })
  Viewer.mount(setup.renderer, Doc.whole("Sample", "Text"), { imageMode: "mono", onImageMode: (mode) => modes.push(mode) })
  setup.mockInput.pressKey("?")
  await setup.renderOnce()
  expect(setup.captureCharFrame()).toContain("show original images")
  setup.mockInput.pressEscape()
  await settle(setup)
  setup.mockInput.pressKey("i")
  expect(modes).toEqual(["color"])
})

const settle = async (s: NonNullable<typeof setup>) => {
  await s.renderOnce()
  await Bun.sleep(50)
  await s.renderOnce()
}

test(": opens a go-to-page box; typing a number and enter jumps there", async () => {
  const s = await mountBook(["Alpha text", "Bravo text", "Charlie text"], 120, 30)
  s.mockInput.pressKey(":")
  await s.renderOnce()
  expect(s.captureCharFrame()).toContain("Go to page")
  s.mockInput.pressKey("3")
  await s.renderOnce()
  expect(s.captureCharFrame()).toContain("3▏ / 3")
  s.mockInput.pressEnter()
  await settle(s)
  const frame = s.captureCharFrame()
  expect(frame).not.toContain("Go to page")
  expect(frame).toContain("Charlie text")
  expect(frame).toContain("3 / 3 · 100%")
})

test("the go-to box ignores keys that aren't a page in the book, and escape closes it in place", async () => {
  const s = await mountBook(["Alpha text", "Bravo text", "Charlie text"], 120, 30)
  s.mockInput.pressKey(":")
  // 9 is past the last page, 0 can't start a page number, q must not quit.
  for (const key of ["9", "0", "q", "j"]) s.mockInput.pressKey(key)
  await s.renderOnce()
  expect(s.renderer.isDestroyed).toBe(false)
  expect(s.captureCharFrame()).toContain("Go to page")
  s.mockInput.pressEscape()
  await settle(s)
  const frame = s.captureCharFrame()
  expect(frame).not.toContain("Go to page")
  expect(frame).toContain("Alpha text")
})

test("a code block keeps its indentation and sits beside a rule, one blank line from the text", async () => {
  const s = await mount("Like this:\n\n```\nli.selected > p {\n    color: blue;\n}\n```\n\nAfter.")
  const lines = s.captureCharFrame().split("\n")
  const at = (text: string) => lines.findIndex((l) => l.includes(text))
  expect(at("li.selected > p {") - at("Like this:")).toBe(2)
  expect(lines[at("li.selected")]).toMatch(/┃ li\.selected > p \{/)
  expect(lines[at("color: blue;")]).toMatch(/┃     color: blue;/)
  expect(at("After.") - at("}")).toBe(2)
})

test("with the kitty keyboard protocol, shift+; opens go-to and shift+/ toggles help", async () => {
  setup = await createTestRenderer({ width: 120, height: 30, kittyKeyboard: true })
  const { loaded } = Viewer.mount(setup.renderer, book(["Alpha text", "Bravo text"]))
  await loaded
  await settle(setup)
  setup.mockInput.pressKey(";", { shift: true })
  await setup.renderOnce()
  expect(setup.captureCharFrame()).toContain("Go to page")
  setup.mockInput.pressEscape()
  await settle(setup)
  setup.mockInput.pressKey("/", { shift: true })
  await setup.renderOnce()
  expect(setup.captureCharFrame()).toContain("this help")
  setup.mockInput.pressKey("/", { shift: true })
  await setup.renderOnce()
  expect(setup.captureCharFrame()).not.toContain("this help")
})

test("t also opens the go-to-page box", async () => {
  const s = await mountBook(["Alpha text", "Bravo text"], 120, 30)
  s.mockInput.pressKey("t")
  await s.renderOnce()
  expect(s.captureCharFrame()).toContain("Go to page")
})
