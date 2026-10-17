import { expect, test } from "bun:test"
import { Highlight } from "./highlight"

const at = (page: number, start: number, end: number, text = "x") => ({ page, start, end, text })

test("ranges that meet across a block break join; other pages stay apart", () => {
  const merged = Highlight.merge([at(0, 10, 20, "b"), at(0, 0, 9, "a"), at(1, 0, 5, "c")])
  expect(merged).toEqual([at(0, 0, 20, "a\nb"), at(1, 0, 5, "c")])
  expect(Highlight.merge([at(0, 0, 5), at(0, 8, 9)])).toHaveLength(2)
})

test("a selection adds a highlight, and selecting into it again clears it", () => {
  const added = Highlight.toggle([at(0, 0, 5)], [at(0, 10, 12)])
  expect(added).toEqual([at(0, 0, 5), at(0, 10, 12)])
  expect(Highlight.toggle(added, [at(0, 11, 30)])).toEqual([at(0, 0, 5)])
  // Same offsets on another page are another place.
  expect(Highlight.toggle(added, [at(1, 0, 5)])).toHaveLength(3)
})

test("an empty selection adds nothing", () => {
  expect(Highlight.toggle([], [at(0, 4, 4)])).toEqual([])
})
