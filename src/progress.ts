import { Database } from "bun:sqlite"
import { mkdirSync } from "node:fs"
import { homedir } from "node:os"
import { dirname, join } from "node:path"
import { Result, TaggedError } from "better-result"

/** Where a reader is in a paged document. `page` is 1-based. */
export type Progress = { page: number; pageCount: number; percent: number }

export class ProgressStoreFailed extends TaggedError("ProgressStoreFailed")<{
  operation: "open" | "load" | "save"
  path: string
  cause: unknown
  message: string
}> {}

/** How figures draw: as the image itself, or painted in the terminal's text color on a transparent ground. */
export type ImageMode = "color" | "mono"

/** Saved reading places, one per document key (a PDF's real path), plus the image mode for every document. */
export type ProgressStore = {
  load: (key: string) => Result<Progress | undefined, ProgressStoreFailed>
  save: (key: string, progress: Progress) => Result<void, ProgressStoreFailed>
  loadImageMode: () => Result<ImageMode | undefined, ProgressStoreFailed>
  saveImageMode: (mode: ImageMode) => Result<void, ProgressStoreFailed>
}

const errorMessage = (cause: unknown) => (cause instanceof Error ? cause.message : String(cause))

const SCHEMA = `CREATE TABLE IF NOT EXISTS progress (
  key TEXT PRIMARY KEY,
  page INTEGER NOT NULL,
  page_count INTEGER NOT NULL,
  percent INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS settings (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
)`

/** A stored image mode, or undefined if the row is missing or isn't one we wrote. */
const parseImageMode = (row: unknown): ImageMode | undefined => {
  if (typeof row !== "object" || row === null || !("value" in row)) return undefined
  return row.value === "color" || row.value === "mono" ? row.value : undefined
}

const isCount = (value: unknown): value is number => typeof value === "number" && Number.isInteger(value) && value >= 0

/** A row from the table, or undefined if it isn't one we wrote. */
const parseRow = (row: unknown): Progress | undefined => {
  if (typeof row !== "object" || row === null) return undefined
  const { page, page_count, percent } = row as Record<string, unknown>
  if (!isCount(page) || !isCount(page_count) || !isCount(percent) || page < 1) return undefined
  return { page, pageCount: page_count, percent }
}

export const Progress = {
  /** Progress after reading through `page`: the last page is 100%. */
  of: (page: number, pageCount: number): Progress => ({
    page,
    pageCount,
    percent: pageCount > 0 ? Math.floor((page / pageCount) * 100) : 0,
  }),

  /** `$XDG_DATA_HOME/leaf/leaf.db`, falling back to `~/.local/share`. */
  defaultPath: () => join(process.env.XDG_DATA_HOME || join(homedir(), ".local", "share"), "leaf", "leaf.db"),

  /** Opens (creating if needed) the SQLite store at `path`; `:memory:` keeps it in memory. */
  open: (path: string): Result<ProgressStore, ProgressStoreFailed> => {
    const failed = (operation: "open" | "load" | "save", what: string) => (cause: unknown) =>
      new ProgressStoreFailed({
        operation,
        path,
        cause,
        message: `Could not ${what} reading progress in ${path} (${errorMessage(cause)}). The document itself is unaffected. Check that the folder is writable, or delete the file to start fresh.`,
      })

    return Result.try({
      try: () => {
        if (path !== ":memory:") mkdirSync(dirname(path), { recursive: true })
        const db = new Database(path, { create: true })
        db.exec(SCHEMA)
        return db
      },
      catch: failed("open", "open"),
    }).map((db) => {
      const select = db.query("SELECT page, page_count, percent FROM progress WHERE key = ?")
      const upsert = db.query(
        `INSERT INTO progress (key, page, page_count, percent, updated_at) VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(key) DO UPDATE SET page = excluded.page, page_count = excluded.page_count,
           percent = excluded.percent, updated_at = excluded.updated_at`,
      )
      const selectSetting = db.query("SELECT value FROM settings WHERE key = ?")
      const upsertSetting = db.query(
        "INSERT INTO settings (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
      )
      return {
        loadImageMode: () =>
          Result.try({ try: () => parseImageMode(selectSetting.get("image_mode")), catch: failed("load", "read") }),
        saveImageMode: (mode) =>
          Result.try({ try: () => void upsertSetting.run("image_mode", mode), catch: failed("save", "save") }),
        load:(key) => Result.try({ try: () => parseRow(select.get(key)), catch: failed("load", "read") }),
        save: (key, { page, pageCount, percent }) =>
          Result.try({
            try: () => void upsert.run(key, page, pageCount, percent, Date.now()),
            catch: failed("save", "save"),
          }),
      }
    })
  },
} as const
