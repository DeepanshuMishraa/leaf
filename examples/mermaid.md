# Mermaid diagrams

Open with `cargo run -- open examples/mermaid.md`. Each section says what you should see.

## Flowchart

Should draw as a diagram.

```mermaid
flowchart LR
  A[Open file] --> B{Has mermaid?}
  B -- yes --> C[Run Pele under bun/node]
  B -- no --> D[Plain markdown]
  C --> E[Rasterize with resvg]
  E --> F[Show as figure]
```

## Sequence diagram

```mermaid
sequenceDiagram
  participant L as leaf
  participant R as runtime
  L->>R: script on stdin
  R-->>L: SVGs split by NUL
  Note over L: rasterize each one
```

## Class diagram

```mermaid
classDiagram
  class Diagram {
    +String svg
    +size (u32, u32)
    +rasterize(width, ink, paper)
  }
  Diagram <|-- Figure
```

## State diagram

```mermaid
stateDiagram-v2
  [*] --> Loading
  Loading --> Drawn: Pele ok
  Loading --> CodeBlock: no runtime / bad source
  Drawn --> [*]
  CodeBlock --> [*]
```

## Pie chart

```mermaid
pie title Binary size
  "leaf" : 80
  "pele" : 20
```

## Gantt

```mermaid
gantt
  title Release
  dateFormat YYYY-MM-DD
  section Work
  Mermaid support :a1, 2026-10-01, 7d
  Review          :after a1, 3d
```

## Quotes, unicode and escapes

Labels with quotes, backslashes and non-ASCII should survive. Known gap: the emoji draws as blank space (resvg doesn't draw color emoji fonts).

```mermaid
flowchart TD
  A["say #quot;hi#quot;"] --> B["path C:\leaf"]
  B --> C["héllo 🌿 世界"]
```

## Diagram followed directly by text

```mermaid
graph LR
  X --> Y
```
This line must still show, as its own paragraph below the diagram.

## Invalid source

Should stay a code block showing its source.

```mermaid
this is not a diagram at all
```

## Other languages are untouched

```rust
fn main() {
    println!("still a code block");
}
```

## Inside a blockquote

Known limitation: the diagram shows after the quote, not inside it.

> Quoted text
>
> ```mermaid
> graph LR
>   Q --> R
> ```
