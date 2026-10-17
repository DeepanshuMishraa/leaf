import { afterEach, expect, test } from "bun:test"
import { CodeRenderable, type Renderable } from "@opentui/core"
import { createTestRenderer } from "@opentui/core/testing"
import { Doc } from "./doc"
import { Viewer } from "./viewer"

let setup: Awaited<ReturnType<typeof createTestRenderer>> | undefined

afterEach(() => {
  setup?.renderer.destroy()
  setup = undefined
})

/**
 * Markdown lays out across frames, and paragraphs draw only once syntax
 * highlighting returns from its worker; waits until the text is up and still.
 */
const drawn = async (s: NonNullable<typeof setup>) => {
  const highlighting = (renderable: Renderable): CodeRenderable[] => [
    ...(renderable instanceof CodeRenderable && renderable.isHighlighting ? [renderable] : []),
    ...renderable.getChildren().flatMap(highlighting),
  ]
  let previous = ""
  for (let tries = 0; tries < 100; tries++) {
    await s.renderOnce()
    const pending = highlighting(s.renderer.root)
    if (pending.length > 0) {
      await Promise.all(pending.map((code) => code.highlightingDone))
      continue
    }
    const frame = s.captureCharFrame()
    if (frame === previous) return s
    previous = frame
  }
  return s
}

const mount = async (markdown: string) => {
  setup = await createTestRenderer({ width: 80, height: 20 })
  Viewer.mount(setup.renderer, Doc.whole("Sample", markdown))
  return drawn(setup)
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

test("in a small window, the pages below fill in, in order", async () => {
  const s = await mountBook(["First page", "Second page", "Third page"], 80, 20)
  await settle(s)
  await settle(s)
  const frame = s.captureCharFrame()
  expect(frame.indexOf("First page")).toBeLessThan(frame.indexOf("Second page"))
  expect(frame.indexOf("Second page")).toBeLessThan(frame.indexOf("Third page"))
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
  page: (index) => pages[index] ?? "",
})

const mountBook = async (pages: string[], width: number, height: number, options?: Parameters<typeof Viewer.mount>[2]) => {
  setup = await createTestRenderer({ width, height })
  Viewer.mount(setup.renderer, book(pages), options)
  return drawn(setup)
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
  Viewer.mount(setup.renderer, book(["Alpha text", "Bravo text"]))
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

/** The text on screen with the highlighter's yellow behind it. */
const highlighted = (s: NonNullable<typeof setup>) =>
  s
    .captureSpans()
    .lines.flatMap((line) => line.spans)
    .filter((span) => {
      const [r, g, b] = span.bg.toInts()
      return r === 250 && g === 214 && b === 64
    })
    .map((span) => span.text)
    .join("")

/** Screen position of the first appearance of `text`, once it has drawn. */
const find = async (s: NonNullable<typeof setup>, text: string) => {
  for (let tries = 0; tries < 20 && !s.captureCharFrame().includes(text); tries++) await settle(s)
  const lines = s.captureCharFrame().split("\n")
  const y = lines.findIndex((line) => line.includes(text))
  return { x: lines[y]?.indexOf(text) ?? -1, y }
}

test("a dragged selection is copied, and y highlights it in yellow and reports it", async () => {
  const saved: unknown[] = []
  const s = await mountBook(["Alpha text here", "Bravo text"], 120, 30, { onHighlights: (h) => saved.push(h) })
  const copied: string[] = []
  s.renderer.copyToClipboardOSC52 = (text) => copied.push(text) > 0
  const { x, y } = await find(s, "text here")
  await s.mockMouse.drag(x, y, x + 3, y)
  expect(copied).toEqual(["text"])
  s.mockInput.pressKey("y")
  await settle(s)
  expect(highlighted(s)).toBe("text")
  expect(saved).toEqual([[{ page: 0, start: 6, end: 10, text: "text" }]])
  // The highlight belongs to its page: turning the page leaves it behind.
  s.mockInput.pressArrow("right")
  await settle(s)
  expect(highlighted(s)).toBe("")
})

test("saved highlights come back, and y on a selection touching one clears it", async () => {
  const saved: unknown[] = []
  const s = await mountBook(["Alpha text here", "Bravo text"], 120, 30, {
    highlights: [{ page: 0, start: 0, end: 5, text: "Alpha" }],
    onHighlights: (h) => saved.push(h),
  })
  const { x, y } = await find(s, "Alpha")
  expect(highlighted(s)).toBe("Alpha")
  await s.mockMouse.drag(x + 2, y, x + 7, y)
  s.mockInput.pressKey("y")
  await settle(s)
  expect(highlighted(s)).toBe("")
  expect(saved).toEqual([[]])
})

test("escape clears a selection before it quits", async () => {
  const s = await mountBook(["Alpha text here"], 120, 30)
  const { x, y } = await find(s, "Alpha")
  await s.mockMouse.drag(x, y, x + 3, y)
  s.mockInput.pressEscape()
  await settle(s)
  expect(s.renderer.isDestroyed).toBe(false)
  expect(s.renderer.hasSelection).toBe(false)
  s.mockInput.pressEscape()
  await settle(s)
  expect(s.renderer.isDestroyed).toBe(true)
})

/** A long book whose pages are a few paragraphs each, with a count of the pages read. */
const longBook = (pageCount: number) => {
  const read = new Set<number>()
  const doc: Doc = {
    title: "Long",
    pageCount,
    images: new Map(),
    page: (index) => {
      read.add(index)
      return Array.from({ length: 6 }, (_, p) => `Page ${index + 1} paragraph ${p + 1}.`).join("\n\n")
    },
  }
  return { doc, read }
}

test("opening deep in a huge book reads only the pages near the screen", async () => {
  const { doc, read } = longBook(10_000)
  setup = await createTestRenderer({ width: 80, height: 24 })
  Viewer.mount(setup.renderer, doc, { startPage: 7000 })
  for (let i = 0; i < 10; i++) await settle(setup)
  const frame = setup.captureCharFrame()
  expect(frame).toContain("Page 7000 paragraph 1.")
  expect(frame).toContain("7000 / 10000")
  expect(read.size).toBeLessThan(12)
})

test("pages built above the reader don't move the text on screen", async () => {
  const { doc } = longBook(50)
  setup = await createTestRenderer({ width: 80, height: 24 })
  Viewer.mount(setup.renderer, doc, { startPage: 20 })
  await settle(setup)
  const row = (text: string) => setup?.captureCharFrame().split("\n").findIndex((line) => line.includes(text))
  const before = (await find(setup, "Page 20 paragraph 2.")).y
  for (let i = 0; i < 10; i++) await settle(setup)
  expect(row("Page 20 paragraph 2.")).toBe(before)
  // Scrolling up reaches the page before, one row at a time.
  for (let i = 0; i < 6; i++) setup.mockInput.pressKey("k")
  await settle(setup)
  expect(row("Page 20 paragraph 2.")).toBe(before + 6)
  expect(setup.captureCharFrame()).toContain("Page 19 paragraph 6.")
})

test("G in a scrolling book goes to the end of the last page, and g back to the start", async () => {
  const { doc } = longBook(3000)
  setup = await createTestRenderer({ width: 80, height: 24 })
  Viewer.mount(setup.renderer, doc)
  await settle(setup)
  setup.mockInput.pressKey("G")
  for (let i = 0; i < 4; i++) await settle(setup)
  expect(setup.captureCharFrame()).toContain("Page 3000 paragraph 6.")
  expect(setup.captureCharFrame()).toContain("3000 / 3000 · 100%")
  setup.mockInput.pressKey("g")
  for (let i = 0; i < 4; i++) await settle(setup)
  expect(setup.captureCharFrame()).toContain("Page 1 paragraph 1.")
})

test("scrolling down a long way keeps only a few pages built", async () => {
  const { doc } = longBook(200)
  setup = await createTestRenderer({ width: 80, height: 24 })
  Viewer.mount(setup.renderer, doc)
  await settle(setup)
  for (let i = 0; i < 40; i++) {
    setup.mockInput.pressKey("d")
    await settle(setup)
  }
  const built = setup.renderer.root.getChildren()[0]?.getChildren()[0]
  expect(setup.captureCharFrame()).not.toContain("Page 1 paragraph")
  expect(setup.captureCharFrame()).toMatch(/Page \d+ paragraph/)
  expect(built).toBeDefined()
})
