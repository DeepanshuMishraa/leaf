#!/usr/bin/env bun
import { Result, TaggedError } from "better-result"
import { Article } from "./article"
import { Pdf } from "./pdf"
import { Progress, type ProgressStoreFailed } from "./progress"
import { Viewer, type ViewerOptions } from "./viewer"
import { realpathSync } from "node:fs"

const USAGE = `Usage: leaf open <file.pdf | url>

Keys: ←/→ page, j/k scroll, d/u half, space/b screen, g/G ends, t go to page, i painted images, drag select + copy, y highlight, ? keys, q quit`

class UnsupportedTarget extends TaggedError("UnsupportedTarget")<{ target: string; message: string }> {}

const isUrl = (target: string) => /^https?:\/\//i.test(target)

const load = async (target: string) => {
  if (isUrl(target)) return Article.open(target)
  if (target.toLowerCase().endsWith(".pdf")) return Pdf.open(target)
  return Result.err(
    new UnsupportedTarget({
      target,
      message: `Can't open "${target}". leaf reads PDF files (.pdf) and web pages (http:// or https:// URLs).`,
    }),
  )
}

const [command, target] = process.argv.slice(2)
if (command !== "open" || target === undefined) {
  console.error(USAGE)
  process.exitCode = 2
} else {
  const doc = await load(target)
  if (doc.isErr()) {
    console.error(`leaf: ${doc.error.message}`)
    process.exitCode = 1
  } else {
    // Den supports OSC 8 links but isn't on OpenTUI's list of terminals that do.
    if (process.env.TERM_PROGRAM === "Den") process.env.FORCE_HYPERLINK ??= "1"
    await Viewer.open(doc.value, resume(target, doc.value.pageCount))
  }
}

/**
 * Remembers the image mode, each document's highlights, and where each PDF
 * (file or link) was left, so they come back next time. Progress that can't be read or saved is reported after
 * the reader closes; reading goes on.
 */
function resume(target: string, pageCount: number | undefined): ViewerOptions {
  const store = Progress.open(Progress.defaultPath())
  if (store.isErr()) {
    console.error(`leaf: ${store.error.message}`)
    return {}
  }
  let failure: string | undefined
  process.on("exit", () => {
    if (failure) console.error(`leaf: ${failure}`)
  })
  const recordFailure = (result: Result<unknown, ProgressStoreFailed>) => {
    if (result.isErr()) failure = result.error.message
  }

  const key = isUrl(target) ? target : realpathSync(target)
  const imageMode = store.value.loadImageMode()
  recordFailure(imageMode)
  const highlights = store.value.loadHighlights(key)
  recordFailure(highlights)
  const everyDoc: ViewerOptions = {
    imageMode: imageMode.isOk() ? imageMode.value : undefined,
    onImageMode: (mode) => recordFailure(store.value.saveImageMode(mode)),
    highlights: highlights.isOk() ? highlights.value : undefined,
    onHighlights: (next) => recordFailure(store.value.saveHighlights(key, next)),
  }
  if (pageCount === undefined) return everyDoc

  const saved = store.value.load(key)
  recordFailure(saved)
  return {
    ...everyDoc,
    startPage: saved.isOk() ? saved.value?.page : undefined,
    onPage: (page) => recordFailure(store.value.save(key, Progress.of(page, pageCount))),
  }
}
