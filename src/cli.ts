#!/usr/bin/env bun
import { Result, TaggedError } from "better-result"
import { Article } from "./article"
import { Pdf } from "./pdf"
import { Viewer } from "./viewer"

const USAGE = `Usage: leaf open <file.pdf | url>

Keys: ←/→ page, j/k scroll, d/u half, space/b screen, g/G ends, i painted images, ? keys, q quit`

class UnsupportedTarget extends TaggedError("UnsupportedTarget")<{ target: string; message: string }> {}

const load = async (target: string) => {
  if (/^https?:\/\//i.test(target)) return Article.open(target)
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
    await Viewer.open(doc.value)
  }
}
