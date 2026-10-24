# Code blocks

Open with `cargo run -- open examples/code.md`. Each section says what you should see.

## Fenced languages

Each should be colored: keywords, strings, comments, numbers, function names.

```rust
use std::collections::HashMap;

/// Counts words in a line.
fn count(line: &str) -> HashMap<&str, usize> {
    let mut seen = HashMap::new();
    for word in line.split_whitespace() {
        *seen.entry(word).or_insert(0) += 1;
    }
    seen
}

fn main() {
    println!("{:?}", count("a leaf is a leaf"));
}
```

```python
from dataclasses import dataclass

@dataclass
class Page:
    number: int
    text: str = ""

def longest(pages: list[Page]) -> Page | None:
    # The page with the most text.
    return max(pages, key=lambda p: len(p.text), default=None)
```

```typescript
interface Book {
  title: string;
  pages: number;
}

export function progress(book: Book, page: number): string {
  return `${Math.round((page / book.pages) * 100)}%`;
}
```

```go
package main

import "fmt"

func main() {
	pages := []int{1, 2, 3}
	for i, p := range pages {
		fmt.Printf("%d: %d\n", i, p)
	}
}
```

```c
#include <stdio.h>

int main(void) {
    const char *name = "leaf";
    printf("hello, %s\n", name);
    return 0;
}
```

```sql
SELECT title, COUNT(*) AS highlights
FROM books
JOIN highlights ON highlights.book_id = books.id
WHERE books.format = 'pdf'
GROUP BY title
ORDER BY highlights DESC;
```

```json
{
  "name": "leaf",
  "version": "0.3.0",
  "formats": ["pdf", "epub", "md"],
  "open": true
}
```

```html
<!doctype html>
<html>
  <body class="page">
    <h1 id="title">Hello</h1>
  </body>
</html>
```

```css
.page {
  color: #222;
  margin: 0 auto;
  max-width: 72ch;
}
```

```bash
#!/usr/bin/env bash
for file in *.pdf; do
  if [ -s "$file" ]; then
    echo "opening $file"
  fi
done
```

```yaml
reader:
  mode: scroll
  formats: [pdf, epub]
```

```toml
[package]
name = "leaf-reader"
version = "0.3.0"
```

```diff
- let width = 80;
+ let width = terminal.width();
```

## Fence aliases

Short names map to the same lexer: `rs`, `py`, `ts`, `sh`, `yml`. All should be colored.

```rs
let x: u8 = 1;
```

```py
print("hi")
```

```sh
echo "$HOME"
```

## No language, guessed

PDF and EPUB code arrives like this, with no fence language. Each should be colored as the language in its heading.

### Rust

```
fn main() {
    let mut total = 0;
    println!("{total}");
}
```

### Python

```
def greet(name):
    print(f"hello {name}")
```

### C-style, no telling keyword

Has braces, `;`, `//` and `&&` but no `function` or `let`. Should be colored with the JS lexer.

```
if (user && user.name && !user.first_name) {
    // Documents written before Dec 8, 2013 don't have first_name
    user.first_name = user.name.split(" ")[0];
}
```

### Shebang

```
#!/usr/bin/env python
import sys
```

## Should stay plain

### Unclear

Too little to tell. Should stay plain.

```
a = 1
  b
```

### Plain text fence

```text
fn main() { this is not colored }
```

### Unknown language

The name is kept, so no guess replaces it. Should stay plain.

```brainfuck
++++++++[>++++[>++>+++<<-]>-]>.
```

## Layout

### Long line

Should wrap, not vanish off the edge.

```js
const message = "this line is long enough that it has to wrap somewhere in a normal terminal window, and the color should carry over to the wrapped part";
```

### Wide characters

Colors should line up after non-ASCII text.

```js
const café = "naïve 漢字 😀"; // still a comment
```
