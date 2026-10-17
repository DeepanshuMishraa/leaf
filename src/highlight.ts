/**
 * A highlighted stretch of a page. `page` is 0-based (0 for documents without
 * pages); `start` and `end` are display-column offsets into the page's text,
 * which is its text blocks in order, each followed by a newline. `text` is
 * what was highlighted.
 */
export type Highlight = { page: number; start: number; end: number; text: string }

const overlaps = (a: Highlight, b: Highlight) => a.page === b.page && a.start < b.end && b.start < a.end

export const Highlight = {
  /** Joins ranges on the same page that meet across a block break, so one selection makes one highlight per page. */
  merge: (ranges: readonly Highlight[]) => {
    const sorted = [...ranges].sort((a, b) => a.page - b.page || a.start - b.start)
    const merged: Highlight[] = []
    for (const range of sorted) {
      const last = merged.at(-1)
      if (last && last.page === range.page && range.start <= last.end + 1) {
        merged[merged.length - 1] = { ...last, end: Math.max(last.end, range.end), text: `${last.text}\n${range.text}` }
      } else merged.push(range)
    }
    return merged
  },

  /**
   * Highlights the selection, or, when it touches highlights already there,
   * clears those instead.
   */
  toggle: (existing: readonly Highlight[], selection: readonly Highlight[]) => {
    const touched = existing.filter((highlight) => selection.some((range) => overlaps(highlight, range)))
    if (touched.length > 0) return existing.filter((highlight) => !touched.includes(highlight))
    return [...existing, ...selection.filter((range) => range.end > range.start)]
  },
} as const
