//! Render a gallery of Markdown features — Mermaid flowchart/sequence/pie diagrams
//! and LaTeX math — to a self-contained HTML page, so the engine's output can be
//! viewed in a browser (or rasterised to an image, see `examples/render.mjs`).
//!
//! Run:
//!   cargo run -p catalerum-markdown --example gallery -- /tmp/gallery.html
//!   node crates/catalerum-markdown/examples/render.mjs /tmp/gallery.html /tmp/gallery.png

use std::fs;

const DEMO: &str = r####"# catalerum-markdown gallery

All rendering below is pure Rust — Mermaid diagrams become SVG and LaTeX math
becomes MathML, with **no JavaScript** and no runtime library.

## LaTeX math

Inline: the quadratic formula $x = \frac{-b \pm \sqrt{b^2 - 4ac}}{2a}$ and
Euler's identity $e^{i\pi} + 1 = 0$.

A display equation:

$$\sum_{i=1}^{n} i = \frac{n(n+1)}{2}$$

Greek letters, integrals and limits:

$$\int_0^\infty e^{-x}\,dx = 1 \qquad \lim_{x \to 0} \frac{\sin x}{x} = 1 \qquad \alpha\beta\gamma\,\Omega$$

A matrix product:

$$\begin{pmatrix} a & b \\ c & d \end{pmatrix}\begin{pmatrix} x \\ y \end{pmatrix} = \begin{pmatrix} ax + by \\ cx + dy \end{pmatrix}$$

Accents and binomials:

$$\hat{a} \quad \vec{v} \quad \bar{x} \quad \dot{q} \quad \ddot{q} \quad \tilde{n} \quad \overline{AB} \quad \underline{xy} \quad \binom{n}{k}$$

Stacks and braces:

$$\overset{!}{=} \quad \underset{x \to 0}{\lim} \quad \overbrace{a + b + c} \quad \underbrace{x_1 + x_2}$$

An aligned derivation (columns line up at the `=`):

$$\begin{aligned} f(x) &= (x+1)^2 \\ &= x^2 + 2x + 1 \end{aligned}$$

## Mermaid flowchart

```mermaid
graph TD
  A[Start] --> B{Valid?}
  B -->|yes| C([Process])
  B -->|no| D[Reject]
  C & D --> E((Done))
```

## Mermaid flowchart (left→right, shapes)

```mermaid
flowchart LR
  U[/User input/] --> API(API gateway)
  API --> Auth{Authorized?}
  Auth -->|yes| Job[[Process]]
  Job --> DB[(Database)]
  Auth -->|no| Err[/Error\]
  Err --> Log[\Audit/]
```

## Mermaid sequence diagram

```mermaid
sequenceDiagram
  autonumber
  participant U as User
  participant S as Server
  participant D as DB
  U->>S: GET /data
  activate S
  S->>D: query
  activate D
  D-->>S: rows
  deactivate D
  S-->>U: 200 OK
  deactivate S
  Note over U,S: session established
  loop every 30s
    U->>S: ping
    S-->>U: pong
  end
  alt cache hit
    S->>U: cached
  else cache miss
    S->>D: fetch
    D-->>S: value
  end
  S->>S: log
```

## Mermaid state diagram

```mermaid
stateDiagram-v2
  [*] --> Idle
  Idle --> Running : start
  Running --> Idle : stop
  Running --> Failed : error
  Failed --> [*]
```

## Mermaid ER diagram

```mermaid
erDiagram
  CUSTOMER ||--o{ ORDER : places
  ORDER ||--|{ LINE_ITEM : contains
```

## Mermaid class diagram

```mermaid
classDiagram
  class Animal {
    +String name
    +int age
    +makeSound() void
  }
  class Pet {
    <<interface>>
    +play() void
  }
  Animal <|-- Dog
  Animal <|-- Cat
  Dog ..|> Pet
  Owner "1" *-- "*" Dog : owns
```

## Mermaid gantt chart

```mermaid
gantt
  title Release plan
  dateFormat YYYY-MM-DD
  section Design
    Research      :done, r1, 2014-01-01, 6d
    Mockups       :active, m1, after r1, 8d
  section Build
    Backend       :b1, after m1, 12d
    Frontend      :after m1, 10d
  section Ship
    QA            :crit, 2014-01-29, 5d
    Launch        :milestone, 2014-02-03, 0d
```

## Mermaid timeline

```mermaid
timeline
  title History of the web
  section Early
    1991 : First website
    1994 : W3C founded
  section Modern
    2008 : Chrome released
    2015 : Rust 1.0 : WebAssembly
```

## Mermaid pie chart

```mermaid
pie title Languages in this repo
  "Rust" : 72
  "JavaScript" : 14
  "TOML" : 9
  "Other" : 5
```

## Mermaid user journey

```mermaid
journey
  title My working day
  section Work
    Make tea: 5: Me
    Write code: 3: Me, Cat
    Deploy: 1: Me
  section Home
    Relax: 5: Me
```

## Mermaid quadrant chart

```mermaid
quadrantChart
  title Reach vs engagement
  x-axis Low Reach --> High Reach
  y-axis Low Engagement --> High Engagement
  quadrant-1 Expand
  quadrant-2 Promote
  quadrant-3 Re-evaluate
  quadrant-4 Improve
  Campaign A: [0.3, 0.6]
  Campaign B: [0.45, 0.23]
  Campaign C: [0.75, 0.8]
```

## Mermaid mindmap

```mermaid
mindmap
  root((Mindmap))
    Origins
      Long history
      Popularisation
    Research
      On effectiveness
      Automatic creation
    Tools
      Pen and paper
      Mermaid
```

## Mermaid git graph

```mermaid
gitGraph
  commit id: "init"
  commit tag: "v1.0"
  branch develop
  commit
  commit type: HIGHLIGHT
  checkout main
  merge develop tag: "release"
  commit type: REVERSE
  cherry-pick id: "abc"
```

## Mermaid sankey diagram

```mermaid
sankey-beta
  Coal,Electricity,25
  Gas,Electricity,18
  Solar,Electricity,7
  Electricity,Homes,30
  Electricity,Industry,20
  Gas,Homes,10
```

## Mermaid C4 context diagram

```mermaid
C4Context
  Person(user, "Customer", "A user of the shop")
  System_Boundary(shop, "Online Shop") {
    System(web, "Web App", "Serves the storefront")
    SystemDb(db, "Database", "Stores orders")
  }
  System_Ext(pay, "Payment Gateway", "Processes card payments")
  Rel(user, web, "Browses & buys", "HTTPS")
  Rel(web, db, "Reads/writes", "SQL")
  Rel(web, pay, "Charges card", "HTTPS")
```

## A regular table, for good measure

| Feature | Output | Pure Rust |
|:--------|:------:|----------:|
| Flowchart | SVG | yes |
| Sequence | SVG | yes |
| Pie | SVG | yes |
| Journey | SVG | yes |
| Quadrant | SVG | yes |
| Mindmap | SVG | yes |
| Git graph | SVG | yes |
| Sankey | SVG | yes |
| C4 | SVG | yes |
| Math | MathML | yes |
"####;

const CSS: &str = r#"
  :root { color-scheme: light; }
  body { font-family: system-ui, -apple-system, "Segoe UI", Roboto, sans-serif;
         max-width: 860px; margin: 2rem auto; padding: 0 1.25rem; color: #1e293b;
         line-height: 1.55; background: #ffffff; }
  h1 { font-size: 1.9rem; }
  h1, h2 { border-bottom: 1px solid #e2e8f0; padding-bottom: .25rem; margin-top: 2rem; }
  figure.catalerum-mermaid { margin: 1.25rem 0; text-align: center; }
  .catalerum-mermaid svg { max-width: 100%; height: auto;
                           border: 1px solid #e2e8f0; border-radius: 8px; padding: 8px; background:#fff; }
  .catalerum-math-block { margin: 1.1rem 0; text-align: center; overflow-x: auto; }
  math { font-size: 1.2em; }
  pre { background: #f1f5f9; padding: .75rem 1rem; border-radius: 8px; overflow: auto; }
  code { font-family: ui-monospace, "SF Mono", Menlo, monospace; font-size: .92em; }
  a { color: #2563eb; }
  table { border-collapse: collapse; margin: 1rem 0; }
  td, th { border: 1px solid #cbd5e1; padding: .35rem .7rem; }
  th { background: #f8fafc; }
"#;

fn main() {
    let body = catalerum_markdown::to_html(DEMO);
    let html = format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>catalerum-markdown gallery</title>\n<style>{CSS}</style>\n</head>\n\
         <body>\n{body}\n</body>\n</html>\n"
    );
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "gallery.html".to_string());
    fs::write(&path, &html).expect("write html");
    println!("wrote {} ({} bytes)", path, html.len());
}
