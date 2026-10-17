import { expect, test } from "bun:test"
import { Progress } from "./progress"

const memoryStore = () => {
  const store = Progress.open(":memory:")
  if (store.isErr()) throw store.error
  return store.value
}

test("percent is how many pages are done, out of the whole", () => {
  expect(Progress.of(1, 613).percent).toBe(0)
  expect(Progress.of(15, 613).percent).toBe(2)
  expect(Progress.of(613, 613).percent).toBe(100)
})

test("a saved place comes back for the same file, and only that file", () => {
  const store = memoryStore()
  expect(store.load("/books/a.pdf").unwrap()).toBeUndefined()
  store.save("/books/a.pdf", Progress.of(15, 613)).unwrap()
  store.save("/books/b.pdf", Progress.of(3, 10)).unwrap()
  expect(store.load("/books/a.pdf").unwrap()).toEqual({ page: 15, pageCount: 613, percent: 2 })
})

test("saving again replaces the old place", () => {
  const store = memoryStore()
  store.save("/books/a.pdf", Progress.of(15, 613)).unwrap()
  store.save("/books/a.pdf", Progress.of(40, 613)).unwrap()
  expect(store.load("/books/a.pdf").unwrap()?.page).toBe(40)
})

test("a store that can't be created is an error value, not a throw", () => {
  const store = Progress.open("/nonexistent-dir/leaf.db")
  expect(store.isErr()).toBe(true)
})

test("the image mode is remembered across documents", () => {
  const store = memoryStore()
  expect(store.loadImageMode().unwrap()).toBeUndefined()
  store.saveImageMode("mono").unwrap()
  expect(store.loadImageMode().unwrap()).toBe("mono")
  store.saveImageMode("color").unwrap()
  expect(store.loadImageMode().unwrap()).toBe("color")
})
