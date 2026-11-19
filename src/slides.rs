//! A PowerPoint deck, read by leaf itself: one page per slide. Shown as
//! designed, each slide is drawn from its XML with its master, layout and
//! theme: backgrounds, shapes, pictures, text in the deck's own fonts,
//! tables, charts and SmartArt. Shown in the terminal's font, it's the
//! slide's words, tables and pictures as text. Speaker notes sit beneath
//! either way.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;
use std::rc::Rc;

use image::RgbaImage;

use crate::doc::{Doc, Look, Source};
use crate::drawing::{
    self, BodyProps, Canvas, Cascade, Paint, Part, Rect, Stroke, TextDefaults, fill_attrs,
    fill_node, geometry, stroke_attrs,
};
use crate::ooxml::{
    Color, Local, Package, PackageFailed, Rel, Theme, emu, md_escape, rel_by_id, rel_by_kind,
    roxmltree,
};
use roxmltree::{Document, Node};

/// Width slides are measured at before the screen picks the real one.
const MEASURE_WIDTH: u32 = crate::render::PAGE_PIXELS;
/// Widest a slide is drawn, in pixels.
const MAX_WIDTH: u32 = 3200;

const NS: &str = r#"xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main""#;

struct Deck {
    package: Package,
    /// Slide size in points.
    size: (f32, f32),
    /// Each shown slide's part name.
    slides: Vec<String>,
    /// The presentation's default text style, for text boxes.
    default_style: Option<String>,
    /// Each slide as text, for the terminal look.
    text: Vec<String>,
    /// Each slide's speaker notes, as markdown.
    notes: Vec<String>,
    look: Cell<Look>,
    pictures: RefCell<HashMap<String, (u32, u32)>>,
}

pub fn open(path: &Path) -> Result<Doc, PackageFailed> {
    let package = Package::open(path, "PowerPoint")?;
    let invalid = |detail: &str| PackageFailed::Invalid {
        path: path.display().to_string(),
        kind: "PowerPoint",
        detail: detail.to_owned(),
    };
    let presentation = package
        .text("ppt/presentation.xml")
        .ok_or_else(|| invalid("it has no ppt/presentation.xml"))?;
    let doc = Document::parse(&presentation)
        .map_err(|e| invalid(&format!("ppt/presentation.xml isn't valid XML ({e})")))?;
    let root = doc.root_element();
    let size = root
        .child("sldSz")
        .and_then(|s| {
            Some((
                s.attribute("cx").and_then(emu)?,
                s.attribute("cy").and_then(emu)?,
            ))
        })
        .unwrap_or((720.0, 405.0));
    let rels = package.rels("ppt/presentation.xml");
    let mut slides = Vec::new();
    if let Some(list) = root.child("sldIdLst") {
        for id in list.kids("sldId") {
            // `r:id`, not the slide's numeric `id`.
            let Some(rel) = id
                .attributes()
                .find(|a| a.name() == "id" && a.namespace().is_some())
                .and_then(|a| rel_by_id(&rels, a.value()))
            else {
                continue;
            };
            let hidden = package.text(&rel.target).is_some_and(|xml| {
                Document::parse(&xml).is_ok_and(|d| {
                    matches!(d.root_element().attribute("show"), Some("0" | "false"))
                })
            });
            if !hidden {
                slides.push(rel.target.clone());
            }
        }
    }
    let default_style = root
        .child("defaultTextStyle")
        .map(|n| presentation[n.range()].to_owned());
    let mut deck = Deck {
        package,
        size,
        slides,
        default_style,
        text: Vec::new(),
        notes: Vec::new(),
        look: Cell::new(Look::Original),
        pictures: RefCell::new(HashMap::new()),
    };
    deck.text = (0..deck.slides.len())
        .map(|i| deck.slide_text(i).unwrap_or_default())
        .collect();
    deck.notes = (0..deck.slides.len())
        .map(|i| deck.slide_notes(i).unwrap_or_default())
        .collect();
    let count = deck.slides.len();
    Ok(Doc {
        title: crate::office::stem(path),
        page_count: Some(count.max(1)),
        source: Box::new(deck),
    })
}

/// The XML a slide is drawn from: itself, its layout, master and theme.
struct Sources {
    slide: String,
    slide_rels: Vec<Rel>,
    layout: Option<(String, Vec<Rel>)>,
    master: Option<(String, Vec<Rel>)>,
    theme: Theme,
}

impl Deck {
    fn sources(&self, index: usize) -> Option<Sources> {
        let part = self.slides.get(index)?;
        let slide = self.package.text(part)?;
        let slide_rels = self.package.rels(part);
        let load = |rels: &[Rel], kind: &str| {
            let rel = rel_by_kind(rels, kind)?;
            Some((
                rel.target.clone(),
                self.package.text(&rel.target)?,
                self.package.rels(&rel.target),
            ))
        };
        let layout = load(&slide_rels, "slideLayout");
        let master = layout
            .as_ref()
            .and_then(|(_, _, rels)| load(rels, "slideMaster"));
        let theme = master
            .as_ref()
            .and_then(|(_, _, rels)| rel_by_kind(rels, "theme"))
            .and_then(|r| self.package.text(&r.target))
            .map(|xml| Theme::parse(&xml))
            .unwrap_or_else(|| Theme::parse(""));
        Some(Sources {
            slide,
            slide_rels,
            layout: layout.map(|(_, x, r)| (x, r)),
            master: master.map(|(_, x, r)| (x, r)),
            theme,
        })
    }

    fn picture_id(&self, part: &str) -> String {
        format!("pic:{part}")
    }

    fn slide_text(&self, index: usize) -> Option<String> {
        let src = self.sources(index)?;
        let slide = Document::parse(&src.slide).ok()?;
        let layout = src
            .layout
            .as_ref()
            .and_then(|(x, _)| Document::parse(x).ok());
        let master = src
            .master
            .as_ref()
            .and_then(|(x, _)| Document::parse(x).ok());
        let default_style = self
            .default_style
            .as_ref()
            .map(|x| format!("<w {NS}>{x}</w>"));
        let default_doc = default_style.as_ref().and_then(|x| Document::parse(x).ok());
        let mut theme = src.theme.clone();
        if let Some(map) = master
            .as_ref()
            .and_then(|m| m.root_element().child("clrMap"))
        {
            theme.map_colors(map);
        }
        let ctx = Ctx {
            theme: &theme,
            part: Part {
                package: &self.package,
                rels: src.slide_rels.clone(),
            },
            layout: layout.as_ref().and_then(|d| tree(d.root_element())),
            master: master.as_ref().and_then(|d| tree(d.root_element())),
            styles: master
                .as_ref()
                .and_then(|d| d.root_element().child("txStyles")),
            default_style: default_doc
                .as_ref()
                .and_then(|d| d.root_element().child("defaultTextStyle")),
            number: index + 1,
            map: Cell::new(NO_MAP),
        };
        let mut blocks = Vec::new();
        if let Some(t) = tree(slide.root_element()) {
            self.collect_text(&ctx, t, &mut blocks);
        }
        Some(slide_markdown(blocks, self.size))
    }

    fn collect_text(&self, ctx: &Ctx, tree: Node, out: &mut Vec<TextBlock>) {
        for node in shapes(tree) {
            // Where a shape sits on the slide, through the groups around it.
            let rect = |n: Node| {
                Ctx::props(n)
                    .and_then(|p| p.child("xfrm"))
                    .or_else(|| n.child("xfrm"))
                    .and_then(drawing::xfrm)
                    .map(|(r, ..)| ctx.mapped(r))
                    .unwrap_or_default()
            };
            match node.local() {
                "sp" => {
                    let Some(body) = node.child("txBody") else {
                        continue;
                    };
                    let ph = placeholder(node);
                    let (master_ph, layout_ph) = ctx.inherited(ph);
                    let rect = ctx
                        .xfrm(node, master_ph, layout_ph)
                        .map(|(r, ..)| r)
                        .unwrap_or_default();
                    let title = ph
                        .and_then(|(kind, _)| kind)
                        .is_some_and(|k| matches!(k, "title" | "ctrTitle"));
                    let cascade = ctx.cascade(node, ph, master_ph, layout_ph);
                    let md = drawing::body_markdown(body, title.then_some(2), &cascade, ctx.theme);
                    // Its largest size set in the text, else the size it inherits.
                    let size = body
                        .descendants()
                        .filter(|n| n.has_tag_name_local("rPr"))
                        .filter_map(|n| n.attribute("sz")?.parse::<f32>().ok())
                        .fold(0.0f32, f32::max)
                        / 100.0;
                    let size = if size > 0.0 {
                        size
                    } else {
                        cascade.level(0, ctx.theme, None).run.size.unwrap_or(0.0)
                    };
                    out.push(TextBlock {
                        rect,
                        kind: if title {
                            BlockKind::Title
                        } else {
                            BlockKind::Text { size }
                        },
                        md,
                    });
                }
                "grpSp" => match ctx.enter_group(node) {
                    Some((_, _, outer)) => {
                        self.collect_text(ctx, node, out);
                        ctx.map.set(outer);
                    }
                    None => self.collect_text(ctx, node, out),
                },
                "pic" => {
                    let Some(id) = node.child("blipFill").and_then(drawing::blip) else {
                        continue;
                    };
                    let Some(rel) = rel_by_id(&ctx.part.rels, id) else {
                        continue;
                    };
                    out.push(TextBlock {
                        rect: rect(node),
                        kind: BlockKind::Picture,
                        md: format!("![]({})", Doc::image_href(&self.picture_id(&rel.target))),
                    });
                }
                "graphicFrame" => {
                    let rect = rect(node);
                    let Some(data) = node.path(&["graphic", "graphicData"]) else {
                        continue;
                    };
                    let mut push = |md: String| {
                        out.push(TextBlock {
                            rect,
                            kind: BlockKind::Other,
                            md,
                        })
                    };
                    if let Some(tbl) = data.child("tbl") {
                        push(table_markdown(tbl));
                    } else if let Some(chart) = data.child("chart") {
                        let md = chart
                            .attr("id")
                            .and_then(|id| rel_by_id(&ctx.part.rels, id))
                            .and_then(|r| self.package.text(&r.target))
                            .map(|xml| crate::chart::markdown(&xml, ctx.theme))
                            .unwrap_or_default();
                        push(md);
                    } else if let Some(xml) = smartart(&self.package, &ctx.part.rels, data) {
                        let lines: Vec<String> = Document::parse(&xml.0)
                            .ok()
                            .map(|d| {
                                d.descendants()
                                    .filter(|n| n.has_tag_name_local("txBody"))
                                    .map(|b| {
                                        b.descendants()
                                            .filter(|n| n.has_tag_name_local("t"))
                                            .filter_map(|t| t.text())
                                            .collect::<String>()
                                    })
                                    .filter(|t| !t.trim().is_empty())
                                    .map(|t| format!("- {}", md_escape(t.trim())))
                                    .collect()
                            })
                            .unwrap_or_default();
                        push(lines.join("\n"));
                    }
                }
                _ => {}
            }
        }
    }

    fn slide_notes(&self, index: usize) -> Option<String> {
        let part = self.slides.get(index)?;
        let rels = self.package.rels(part);
        let notes = rel_by_kind(&rels, "notesSlide")?;
        let xml = self.package.text(&notes.target)?;
        let doc = Document::parse(&xml).ok()?;
        let mut paragraphs = Vec::new();
        for sp in doc.descendants().filter(|n| n.has_tag_name_local("sp")) {
            if placeholder(sp).and_then(|(kind, _)| kind) != Some("body") {
                continue;
            }
            for p in sp.descendants().filter(|n| n.has_tag_name_local("p")) {
                let text: String = p
                    .descendants()
                    .filter(|n| n.has_tag_name_local("t") || n.has_tag_name_local("br"))
                    .map(|n| {
                        if n.has_tag_name_local("br") {
                            "\n".to_owned()
                        } else {
                            n.text().unwrap_or("").to_owned()
                        }
                    })
                    .collect();
                for line in text.lines() {
                    let line = line.trim();
                    if !line.is_empty() {
                        paragraphs.push(md_escape(line));
                    }
                }
            }
        }
        Some(paragraphs.join("\n\n"))
    }

    fn render(&self, index: usize) -> Option<String> {
        let src = self.sources(index)?;
        let slide = Document::parse(&src.slide).ok()?;
        let layout = src
            .layout
            .as_ref()
            .and_then(|(x, _)| Document::parse(x).ok());
        let master = src
            .master
            .as_ref()
            .and_then(|(x, _)| Document::parse(x).ok());
        let default_style = self
            .default_style
            .as_ref()
            .map(|x| format!("<w {NS}>{x}</w>"));
        let default_doc = default_style.as_ref().and_then(|x| Document::parse(x).ok());
        let mut theme = src.theme.clone();
        if let Some(map) = master
            .as_ref()
            .and_then(|m| m.root_element().child("clrMap"))
        {
            theme.map_colors(map);
        }
        if let Some(over) = slide
            .root_element()
            .path(&["clrMapOvr", "overrideClrMapping"])
        {
            theme.map_colors(over);
        }
        let (w, h) = self.size;
        let mut canvas = Canvas::default();
        let rels = [
            Some(&src.slide_rels),
            src.layout.as_ref().map(|(_, r)| r),
            src.master.as_ref().map(|(_, r)| r),
        ];
        let docs = [Some(&slide), layout.as_ref(), master.as_ref()];
        let ctx_for = |rels: &Vec<Rel>| Ctx {
            theme: &theme,
            part: Part {
                package: &self.package,
                rels: rels.clone(),
            },
            layout: layout.as_ref().and_then(|d| tree(d.root_element())),
            master: master.as_ref().and_then(|d| tree(d.root_element())),
            styles: master
                .as_ref()
                .and_then(|d| d.root_element().child("txStyles")),
            default_style: default_doc
                .as_ref()
                .and_then(|d| d.root_element().child("defaultTextStyle")),
            number: index + 1,
            map: Cell::new(NO_MAP),
        };
        // The background: the slide's own, else its layout's, else its master's.
        let paper = docs.iter().zip(rels.iter()).find_map(|(doc, rels)| {
            let doc = (*doc)?;
            let bg = doc.root_element().path(&["cSld", "bg"])?;
            Some(ctx_for(rels.unwrap_or(&src.slide_rels)).background(bg))
        });
        let paper = paper
            .flatten()
            .unwrap_or(Paint::Solid(theme.scheme("bg1").unwrap_or(Color::WHITE)));
        let full = Rect {
            x: 0.0,
            y: 0.0,
            w,
            h,
        };
        paint_rect(&mut canvas, &paper, full);
        let shows = |doc: Option<&Document>| {
            doc.is_none_or(|d| {
                !matches!(
                    d.root_element().attribute("showMasterSp"),
                    Some("0" | "false")
                )
            })
        };
        let slide_shows = shows(Some(&slide));
        if slide_shows
            && shows(layout.as_ref())
            && let (Some(master), Some(rels)) = (&master, rels[2])
            && let Some(t) = tree(master.root_element())
        {
            ctx_for(rels).draw_tree(&mut canvas, t, true);
        }
        if slide_shows
            && let (Some(layout), Some(rels)) = (&layout, rels[1])
            && let Some(t) = tree(layout.root_element())
        {
            ctx_for(rels).draw_tree(&mut canvas, t, true);
        }
        if let Some(t) = tree(slide.root_element()) {
            ctx_for(&src.slide_rels).draw_tree(&mut canvas, t, false);
        }
        Some(canvas.finish(w, h))
    }

    fn picture(&self, part: &str) -> Option<Vec<u8>> {
        self.package.bytes(part)
    }
}

fn paint_rect(canvas: &mut Canvas, paint: &Paint, rect: Rect) {
    match paint {
        Paint::Picture(uri) => drawing::picture(
            canvas,
            uri,
            rect,
            None,
            &format!("M0 0 H{:.2} V{:.2} H0 Z", rect.w, rect.h),
        ),
        other => {
            let attrs = fill_attrs(canvas, Some(other));
            let _ = write!(
                canvas.body,
                r#"<rect x="{:.2}" y="{:.2}" width="{:.2}" height="{:.2}"{attrs}/>"#,
                rect.x, rect.y, rect.w, rect.h
            );
        }
    }
}

fn tree<'a, 'i>(root: Node<'a, 'i>) -> Option<Node<'a, 'i>> {
    root.path(&["cSld", "spTree"])
}

/// The drawable children of a shape tree, taking each `AlternateContent`'s
/// fallback.
fn shapes<'a, 'i>(tree: Node<'a, 'i>) -> Vec<Node<'a, 'i>> {
    let mut out = Vec::new();
    for child in tree.children().filter(|n| n.is_element()) {
        if child.local() == "AlternateContent" {
            let pick = child.child("Fallback").or_else(|| child.child("Choice"));
            if let Some(pick) = pick {
                out.extend(pick.children().filter(|n| n.is_element()));
            }
        } else {
            out.push(child);
        }
    }
    out
}

/// A shape's placeholder type and index, if it's a placeholder.
fn placeholder<'a>(sp: Node<'a, '_>) -> Option<(Option<&'a str>, Option<&'a str>)> {
    let nv = sp
        .children()
        .find(|n| n.is_element() && n.local().starts_with("nv"))?;
    let ph = nv.path(&["nvPr", "ph"])?;
    Some((ph.attribute("type"), ph.attribute("idx")))
}

fn same_type(a: Option<&str>, b: Option<&str>) -> bool {
    fn norm(t: Option<&str>) -> &str {
        match t {
            None | Some("obj") => "body",
            Some("ctrTitle") => "title",
            Some(t) => t,
        }
    }
    norm(a) == norm(b)
}

/// Finds the placeholder in `tree` that a slide's placeholder inherits from.
fn find_placeholder<'a, 'i>(
    tree: Option<Node<'a, 'i>>,
    kind: Option<&str>,
    idx: Option<&str>,
    by_index: bool,
) -> Option<Node<'a, 'i>> {
    let candidates: Vec<(Node, Option<&str>, Option<&str>)> = tree?
        .descendants()
        .filter(|n| n.has_tag_name_local("sp"))
        .filter_map(|n| placeholder(n).map(|(k, i)| (n, k, i)))
        .collect();
    if by_index
        && let Some(idx) = idx
        && let Some((n, ..)) = candidates.iter().find(|(_, _, i)| *i == Some(idx))
    {
        return Some(*n);
    }
    candidates
        .iter()
        .find(|(_, k, _)| *k == kind)
        .or_else(|| candidates.iter().find(|(_, k, _)| same_type(*k, kind)))
        .map(|(n, ..)| *n)
}

struct Ctx<'a, 'i> {
    theme: &'a Theme,
    part: Part<'a>,
    layout: Option<Node<'a, 'i>>,
    master: Option<Node<'a, 'i>>,
    styles: Option<Node<'a, 'i>>,
    default_style: Option<Node<'a, 'i>>,
    number: usize,
    /// How the group being drawn maps its children's coordinates onto the
    /// slide: scale x and y, then offset x and y.
    map: Cell<Map>,
}

/// How a group maps its children onto the slide: scale x and y, then shift x and y.
type Map = (f32, f32, f32, f32);

const NO_MAP: Map = (1.0, 1.0, 0.0, 0.0);

impl<'a, 'i> Ctx<'a, 'i> {
    fn mapped(&self, r: Rect) -> Rect {
        let (sx, sy, tx, ty) = self.map.get();
        Rect {
            x: tx + r.x * sx,
            y: ty + r.y * sy,
            w: r.w * sx,
            h: r.h * sy,
        }
    }

    /// The master and layout placeholders a placeholder inherits from.
    fn inherited(
        &self,
        ph: Option<(Option<&str>, Option<&str>)>,
    ) -> (Option<Node<'a, 'i>>, Option<Node<'a, 'i>>) {
        let Some((kind, idx)) = ph else {
            return (None, None);
        };
        let layout = find_placeholder(self.layout, kind, idx, true);
        let master_kind = layout.and_then(placeholder).and_then(|(k, _)| k).or(kind);
        let master = find_placeholder(self.master, master_kind, None, false);
        (master, layout)
    }

    fn props(node: Node<'a, 'i>) -> Option<Node<'a, 'i>> {
        node.children()
            .find(|n| n.is_element() && (n.local() == "spPr" || n.local() == "grpSpPr"))
    }

    fn xfrm(
        &self,
        node: Node<'a, 'i>,
        master: Option<Node<'a, 'i>>,
        layout: Option<Node<'a, 'i>>,
    ) -> Option<(Rect, f32, bool, bool)> {
        // A placeholder's inherited position is on the slide already; its own
        // is inside whatever group holds it.
        if let Some(own) = Self::props(node)
            .and_then(|p| p.child("xfrm"))
            .and_then(drawing::xfrm)
        {
            return Some((self.mapped(own.0), own.1, own.2, own.3));
        }
        [layout, master].into_iter().flatten().find_map(|n| {
            Self::props(n)
                .and_then(|p| p.child("xfrm"))
                .and_then(drawing::xfrm)
        })
    }

    fn background(&self, bg: Node) -> Option<Paint> {
        if let Some(fill) = bg.child("bgPr").and_then(fill_node) {
            return drawing::paint(fill, self.theme, &self.part, None).flatten();
        }
        let reference = bg.child("bgRef")?;
        let idx: usize = reference.attribute("idx")?.parse().ok()?;
        let color = self.theme.color(reference, None);
        let style = if idx >= 1001 {
            self.theme.backgrounds.get(idx - 1001)
        } else {
            self.theme.fills.get(idx.checked_sub(1)?)
        };
        let Some(style) = style else {
            return color.map(Paint::Solid);
        };
        let wrapped = format!("<w {NS}>{style}</w>");
        let doc = Document::parse(&wrapped).ok()?;
        let fill = doc.root_element().children().find(|n| n.is_element())?;
        match drawing::paint(fill, self.theme, &self.part, color).flatten() {
            // A picture named by the theme lives in the theme's part; fall back to the color.
            Some(Paint::Picture(_)) | None => color.map(Paint::Solid),
            some => some,
        }
    }

    /// The fill and line a shape's style names, by index into the theme.
    fn style_fill(&self, node: Node) -> Option<Paint> {
        let style = node.child("style")?;
        let reference = style.child("fillRef")?;
        let idx: usize = reference.attribute("idx")?.parse().ok()?;
        if idx == 0 {
            return None;
        }
        let color = self.theme.color(reference, None);
        let list = if idx >= 1001 {
            self.theme.backgrounds.get(idx - 1001)
        } else {
            self.theme.fills.get(idx - 1)
        };
        let from_theme = list.and_then(|xml| {
            let wrapped = format!("<w {NS}>{xml}</w>");
            let doc = Document::parse(&wrapped).ok()?;
            let fill = doc.root_element().children().find(|n| n.is_element())?;
            drawing::paint(fill, self.theme, &self.part, color).flatten()
        });
        match from_theme {
            Some(Paint::Picture(_)) | None => color.map(Paint::Solid),
            some => some,
        }
    }

    fn style_line(&self, node: Node) -> Option<Stroke> {
        let reference = node.path(&["style", "lnRef"])?;
        let idx: usize = reference.attribute("idx")?.parse().ok()?;
        if idx == 0 {
            return None;
        }
        Some(Stroke {
            color: self.theme.color(reference, None)?,
            width: [0.75, 1.0, 1.5].get(idx - 1).copied().unwrap_or(1.0),
            dash: None,
            head: false,
            tail: false,
        })
    }

    fn style_font_color(&self, node: Node) -> Option<Color> {
        let reference = node.path(&["style", "fontRef"])?;
        self.theme.color(reference, None)
    }

    fn cascade(
        &self,
        node: Node<'a, 'i>,
        ph: Option<(Option<&str>, Option<&str>)>,
        master: Option<Node<'a, 'i>>,
        layout: Option<Node<'a, 'i>>,
    ) -> Cascade<'a, 'i> {
        let mut lists = Vec::new();
        match ph {
            Some((kind, _)) => {
                let kind = layout.and_then(placeholder).and_then(|(k, _)| k).or(kind);
                let name = match kind {
                    Some("title" | "ctrTitle") => Some("titleStyle"),
                    Some("dt" | "ftr" | "sldNum" | "hdr") => Some("otherStyle"),
                    _ => Some("bodyStyle"),
                };
                if let (Some(styles), Some(name)) = (self.styles, name)
                    && let Some(list) = styles.child(name)
                {
                    lists.push(list);
                }
                for inherited in [master, layout].into_iter().flatten() {
                    if let Some(list) = inherited.path(&["txBody", "lstStyle"]) {
                        lists.push(list);
                    }
                }
            }
            None => {
                if let Some(list) = self.default_style {
                    lists.push(list);
                }
            }
        }
        let inherited = lists.len();
        if let Some(list) = node.path(&["txBody", "lstStyle"]) {
            lists.push(list);
        }
        Cascade {
            lists,
            color: self.style_font_color(node).map(|c| (inherited, c)),
        }
    }

    fn defaults(&self, ph: Option<(Option<&str>, Option<&str>)>) -> TextDefaults {
        let title = matches!(ph, Some((Some("title" | "ctrTitle"), _)));
        TextDefaults {
            font: if title {
                self.theme.major.clone()
            } else {
                self.theme.minor.clone()
            },
            size: 18.0,
            color: self.theme.scheme("tx1").unwrap_or(Color::BLACK),
            bold: false,
            number: self.number,
        }
    }

    /// Draws a shape tree. On a master or layout, placeholders are prompts
    /// for slides to fill, so they're left out.
    fn draw_tree(&self, canvas: &mut Canvas, tree: Node, skip_placeholders: bool) {
        for node in shapes(tree) {
            if skip_placeholders && placeholder(node).is_some() {
                continue;
            }
            match node.local() {
                "sp" | "cxnSp" => self.shape(canvas, node),
                "pic" => self.pic(canvas, node),
                "grpSp" => self.group(canvas, node, skip_placeholders),
                "graphicFrame" => self.frame(canvas, node),
                _ => {}
            }
        }
    }

    fn open_group(canvas: &mut Canvas, rect: Rect, rot: f32) {
        let _ = write!(
            canvas.body,
            r#"<g transform="translate({:.2} {:.2}) rotate({rot:.2} {:.2} {:.2})">"#,
            rect.x,
            rect.y,
            rect.w / 2.0,
            rect.h / 2.0
        );
    }

    fn flip(flip_h: bool, flip_v: bool, rect: Rect) -> String {
        if !flip_h && !flip_v {
            return String::new();
        }
        format!(
            r#" transform="translate({:.2} {:.2}) scale({} {})""#,
            if flip_h { rect.w } else { 0.0 },
            if flip_v { rect.h } else { 0.0 },
            if flip_h { -1 } else { 1 },
            if flip_v { -1 } else { 1 }
        )
    }

    fn shape(&self, canvas: &mut Canvas, node: Node<'a, 'i>) {
        let ph = placeholder(node);
        let (master_ph, layout_ph) = self.inherited(ph);
        let Some((rect, rot, flip_h, flip_v)) = self.xfrm(node, master_ph, layout_ph) else {
            return;
        };
        let props = Self::props(node);
        let inherited_props = [layout_ph, master_ph]
            .into_iter()
            .flatten()
            .filter_map(Self::props);
        let fill = props
            .and_then(fill_node)
            .or_else(|| inherited_props.clone().find_map(fill_node));
        let paint = match fill {
            Some(f) => drawing::paint(f, self.theme, &self.part, None).flatten(),
            None => self.style_fill(node),
        };
        let line_color = node
            .path(&["style", "lnRef"])
            .and_then(|r| self.theme.color(r, None));
        let ln = props
            .and_then(|p| p.child("ln"))
            .or_else(|| inherited_props.clone().find_map(|p| p.child("ln")));
        let stroke = match ln {
            Some(ln) => drawing::stroke(ln, self.theme, line_color),
            None => self.style_line(node),
        };
        let d = geometry(props, rect.w, rect.h);
        let local = Rect {
            x: 0.0,
            y: 0.0,
            ..rect
        };
        Self::open_group(canvas, rect, rot);
        let is_line = node.local() == "cxnSp"
            || props
                .and_then(|p| p.child("prstGeom"))
                .and_then(|g| g.attribute("prst"))
                .is_some_and(|p| p.contains("Connector") || p == "line");
        if let Some(Paint::Picture(uri)) = &paint {
            drawing::picture(canvas, uri, local, None, &d);
        }
        if paint.is_some() || stroke.is_some() {
            let fill = if is_line {
                r#" fill="none""#.to_owned()
            } else {
                match &paint {
                    Some(Paint::Picture(_)) | None => r#" fill="none""#.to_owned(),
                    some => fill_attrs(canvas, some.as_ref()),
                }
            };
            let strokes = stroke_attrs(canvas, stroke.as_ref());
            let _ = write!(
                canvas.body,
                r#"<path d="{d}"{fill}{strokes}{}/>"#,
                Self::flip(flip_h, flip_v, local)
            );
        }
        if let Some(body) = node.child("txBody") {
            let mut body_props = BodyProps::default();
            for source in [master_ph, layout_ph, Some(node)].into_iter().flatten() {
                if let Some(b) = source.path(&["txBody", "bodyPr"]) {
                    body_props.apply(b);
                }
            }
            let cascade = self.cascade(node, ph, master_ph, layout_ph);
            let text_rect = node
                .child("txXfrm")
                .and_then(drawing::xfrm)
                .map(|(r, ..)| {
                    let r = self.mapped(r);
                    Rect {
                        x: r.x - rect.x,
                        y: r.y - rect.y,
                        ..r
                    }
                })
                .unwrap_or(local);
            drawing::text_body(
                canvas,
                body,
                text_rect,
                &cascade,
                &body_props,
                self.theme,
                &self.defaults(ph),
                None,
            );
        }
        canvas.body.push_str("</g>");
    }

    fn pic(&self, canvas: &mut Canvas, node: Node<'a, 'i>) {
        let ph = placeholder(node);
        let (master_ph, layout_ph) = self.inherited(ph);
        let Some((rect, rot, ..)) = self.xfrm(node, master_ph, layout_ph) else {
            return;
        };
        let Some(fill) = node.child("blipFill") else {
            return;
        };
        let props = Self::props(node);
        let local = Rect {
            x: 0.0,
            y: 0.0,
            ..rect
        };
        let d = geometry(props, rect.w, rect.h);
        Self::open_group(canvas, rect, rot);
        if let Some(uri) = drawing::blip(fill).and_then(|id| self.part.picture(id)) {
            drawing::picture(canvas, &uri, local, fill.child("srcRect"), &d);
        }
        let stroke = props
            .and_then(|p| p.child("ln"))
            .and_then(|ln| drawing::stroke(ln, self.theme, None));
        if stroke.is_some() {
            let strokes = stroke_attrs(canvas, stroke.as_ref());
            let _ = write!(canvas.body, r#"<path d="{d}" fill="none"{strokes}/>"#);
        }
        canvas.body.push_str("</g>");
    }

    fn group(&self, canvas: &mut Canvas, node: Node<'a, 'i>, skip_placeholders: bool) {
        let Some((rect, rot, outer)) = self.enter_group(node) else {
            self.draw_tree(canvas, node, skip_placeholders);
            return;
        };
        let _ = write!(
            canvas.body,
            r#"<g transform="rotate({rot:.2} {:.2} {:.2})">"#,
            rect.x + rect.w / 2.0,
            rect.y + rect.h / 2.0
        );
        self.draw_tree(canvas, node, skip_placeholders);
        canvas.body.push_str("</g>");
        self.map.set(outer);
    }

    /// Maps a group's children into its box, through the groups around it.
    /// Gives the group's box on the slide, its rotation, and the mapping to
    /// put back once its children are done.
    fn enter_group(&self, node: Node<'a, 'i>) -> Option<(Rect, f32, Map)> {
        let x = Self::props(node).and_then(|p| p.child("xfrm"))?;
        let (own, rot, ..) = drawing::xfrm(x)?;
        let child = |name: &str, a: &str, b: &str| {
            let n = x.child(name)?;
            Some((n.attribute(a).and_then(emu)?, n.attribute(b).and_then(emu)?))
        };
        let (cx, cy) = child("chOff", "x", "y").unwrap_or((own.x, own.y));
        let (cw, ch) = child("chExt", "cx", "cy").unwrap_or((own.w, own.h));
        let (gx, gy) = (
            if cw > 0.0 { own.w / cw } else { 1.0 },
            if ch > 0.0 { own.h / ch } else { 1.0 },
        );
        let outer = self.map.get();
        let rect = self.mapped(own);
        // Children map into the group's box, then through the groups around it.
        self.map.set((
            outer.0 * gx,
            outer.1 * gy,
            outer.2 + outer.0 * (own.x - cx * gx),
            outer.3 + outer.1 * (own.y - cy * gy),
        ));
        Some((rect, rot, outer))
    }

    fn frame(&self, canvas: &mut Canvas, node: Node<'a, 'i>) {
        let Some((rect, ..)) = node.child("xfrm").and_then(drawing::xfrm) else {
            return;
        };
        let rect = self.mapped(rect);
        let Some(data) = node.path(&["graphic", "graphicData"]) else {
            return;
        };
        if let Some(tbl) = data.child("tbl") {
            // In a scaled group, the table is laid out at its own size and
            // scaled with the group, text and all.
            let (sx, sy, ..) = self.map.get();
            if (sx - 1.0).abs() > 1e-3 || (sy - 1.0).abs() > 1e-3 {
                let _ = write!(
                    canvas.body,
                    r#"<g transform="translate({:.2} {:.2}) scale({sx:.4} {sy:.4})">"#,
                    rect.x, rect.y
                );
                let own = Rect {
                    x: 0.0,
                    y: 0.0,
                    w: rect.w / sx.max(1e-3),
                    h: rect.h / sy.max(1e-3),
                };
                self.table(canvas, tbl, own);
                canvas.body.push_str("</g>");
            } else {
                self.table(canvas, tbl, rect);
            }
        } else if let Some(chart) = data.child("chart") {
            if let Some(xml) = chart
                .attr("id")
                .and_then(|id| rel_by_id(&self.part.rels, id))
                .and_then(|r| self.part.package.text(&r.target))
            {
                crate::chart::draw(canvas, &xml, rect, self.theme);
            }
        } else if let Some((xml, rels)) = smartart(self.part.package, &self.part.rels, data)
            && let Ok(doc) = Document::parse(&xml)
            && let Some(t) = doc.root_element().child("spTree")
        {
            let inner = Ctx {
                theme: self.theme,
                part: Part {
                    package: self.part.package,
                    rels,
                },
                layout: None,
                master: None,
                styles: None,
                default_style: self.default_style,
                number: self.number,
                map: Cell::new(NO_MAP),
            };
            let _ = write!(
                canvas.body,
                r#"<g transform="translate({:.2} {:.2})">"#,
                rect.x, rect.y
            );
            inner.draw_tree(canvas, t, false);
            canvas.body.push_str("</g>");
        }
    }

    fn table(&self, canvas: &mut Canvas, tbl: Node<'a, 'i>, rect: Rect) {
        let widths: Vec<f32> = tbl
            .child("tblGrid")
            .map(|g| {
                g.kids("gridCol")
                    .filter_map(|c| c.attribute("w").and_then(emu))
                    .collect()
            })
            .unwrap_or_default();
        let props = tbl.child("tblPr");
        let flag = |n: &str| matches!(props.and_then(|p| p.attribute(n)), Some("1" | "true"));
        let style_id = props
            .and_then(|p| p.child("tableStyleId"))
            .and_then(|s| s.text())
            .unwrap_or("");
        // Office's default table style, unless the table asks for a plain one.
        let styled = !style_id.is_empty()
            && !matches!(
                style_id,
                "{2D5ABB26-0587-4C30-8999-92F81FD0307C}" | "{5940675A-B579-460E-94D1-54222C63F5DA}"
            );
        let grid = style_id == "{5940675A-B579-460E-94D1-54222C63F5DA}";
        let accent = self
            .theme
            .scheme("accent1")
            .unwrap_or(Color::rgb(68, 114, 196));
        let blend = |share: f32| Color {
            rgb: accent
                .rgb
                .map(|c| (c as f32 + (255.0 - c as f32) * (1.0 - share)).round() as u8),
            alpha: 1.0,
        };
        let rows: Vec<Node> = tbl.kids("tr").collect();
        let paint_of = |r: usize, tc: Node| {
            let header = flag("firstRow") && r == 0;
            match tc.child("tcPr").and_then(fill_node) {
                Some(f) => drawing::paint(f, self.theme, &self.part, None).flatten(),
                None if styled && header => Some(Paint::Solid(accent)),
                None if styled => Some(Paint::Solid(
                    if flag("bandRow") && r.saturating_sub(usize::from(flag("firstRow"))) % 2 == 0 {
                        blend(0.4)
                    } else {
                        blend(0.2)
                    },
                )),
                None => None,
            }
        };
        let text_setup = |r: usize, tc: Node<'a, 'i>, paint: &Option<Paint>| {
            let header = flag("firstRow") && r == 0;
            let tcpr = tc.child("tcPr");
            let margin =
                |n: &str, d: f32| tcpr.and_then(|p| p.attribute(n)).and_then(emu).unwrap_or(d);
            let mut body_props = BodyProps {
                insets: [
                    margin("marL", 7.2),
                    margin("marT", 3.6),
                    margin("marR", 7.2),
                    margin("marB", 3.6),
                ],
                ..BodyProps::default()
            };
            body_props.anchor = match tcpr.and_then(|p| p.attribute("anchor")) {
                Some("ctr") => 'c',
                Some("b") => 'b',
                _ => 't',
            };
            let text_color = match paint {
                Some(p) if p.color().is_some_and(|c| c.luminance() < 0.5) => Color::WHITE,
                _ => self.theme.scheme("tx1").unwrap_or(Color::BLACK),
            };
            let mut lists: Vec<Node<'a, 'i>> = self.default_style.into_iter().collect();
            let inherited = lists.len();
            if let Some(l) = tc.path(&["txBody", "lstStyle"]) {
                lists.push(l);
            }
            let cascade = Cascade {
                lists,
                color: Some((inherited, text_color)),
            };
            let defaults = TextDefaults {
                bold: header && styled,
                color: text_color,
                ..self.defaults(None)
            };
            (body_props, cascade, defaults)
        };
        let span = |tc: Node, attr: &str| -> usize {
            tc.attribute(attr)
                .and_then(|s| s.parse().ok())
                .unwrap_or(1)
                .max(1)
        };
        // PowerPoint grows a row to fit its text, so measure first.
        let heights: Vec<f32> = rows
            .iter()
            .enumerate()
            .map(|(r, tr)| {
                let mut h = tr.attribute("h").and_then(emu).unwrap_or(20.0);
                let mut col = 0;
                for tc in tr.kids("tc") {
                    let cols = span(tc, "gridSpan");
                    let w: f32 = widths.iter().skip(col).take(cols).sum();
                    col += cols;
                    let merged = matches!(tc.attribute("hMerge"), Some("1"))
                        || matches!(tc.attribute("vMerge"), Some("1"));
                    if merged || span(tc, "rowSpan") > 1 {
                        continue;
                    }
                    let Some(body) = tc.child("txBody") else {
                        continue;
                    };
                    let paint = paint_of(r, tc);
                    let (body_props, cascade, defaults) = text_setup(r, tc, &paint);
                    let laid = drawing::lay_body(
                        body,
                        Rect {
                            x: 0.0,
                            y: 0.0,
                            w,
                            h,
                        },
                        &cascade,
                        &body_props,
                        self.theme,
                        &defaults,
                        None,
                    );
                    h = h.max(laid.total + laid.insets);
                }
                h
            })
            .collect();
        let mut y = rect.y;
        for (r, tr) in rows.iter().enumerate() {
            let h = heights[r];
            let mut x = rect.x;
            let mut col = 0;
            for tc in tr.kids("tc") {
                let cols = span(tc, "gridSpan");
                let w: f32 = widths.iter().skip(col).take(cols).sum();
                col += cols;
                let merged = matches!(tc.attribute("hMerge"), Some("1"))
                    || matches!(tc.attribute("vMerge"), Some("1"));
                let cell_h: f32 = heights.iter().skip(r).take(span(tc, "rowSpan")).sum();
                let cell = Rect { x, y, w, h: cell_h };
                x += w;
                if merged {
                    continue;
                }
                let tcpr = tc.child("tcPr");
                let paint = paint_of(r, tc);
                if let Some(paint) = &paint {
                    paint_rect(canvas, paint, cell);
                }
                let default_edge = (styled || grid).then(|| Stroke {
                    color: if styled { Color::WHITE } else { Color::BLACK },
                    width: if styled { 1.0 } else { 0.75 },
                    dash: None,
                    head: false,
                    tail: false,
                });
                let (right, bottom) = (cell.x + cell.w, cell.y + cell.h);
                for (name, x1, y1, x2, y2) in [
                    ("lnT", cell.x, cell.y, right, cell.y),
                    ("lnB", cell.x, bottom, right, bottom),
                    ("lnL", cell.x, cell.y, cell.x, bottom),
                    ("lnR", right, cell.y, right, bottom),
                ] {
                    let stroke = match tcpr.and_then(|p| p.child(name)) {
                        Some(ln) => drawing::stroke(ln, self.theme, None),
                        None => default_edge.clone(),
                    };
                    if stroke.is_some() {
                        let attrs = stroke_attrs(canvas, stroke.as_ref());
                        let _ = write!(
                            canvas.body,
                            r#"<line x1="{x1:.2}" y1="{y1:.2}" x2="{x2:.2}" y2="{y2:.2}"{attrs}/>"#
                        );
                    }
                }
                let Some(body) = tc.child("txBody") else {
                    continue;
                };
                let (body_props, cascade, defaults) = text_setup(r, tc, &paint);
                drawing::text_body(
                    canvas,
                    body,
                    cell,
                    &cascade,
                    &body_props,
                    self.theme,
                    &defaults,
                    None,
                );
            }
            y += h;
        }
    }
}

/// A SmartArt graphic's drawing (the shapes PowerPoint saved for it) and
/// that drawing's relationships.
fn smartart(package: &Package, rels: &[Rel], data: Node) -> Option<(String, Vec<Rel>)> {
    let ids = data.child("relIds")?;
    let data_rel = rel_by_id(rels, ids.attr("dm")?)?;
    let model = package.text(&data_rel.target)?;
    let drawing_id = Document::parse(&model).ok().and_then(|d| {
        d.descendants()
            .find(|n| n.has_tag_name_local("dataModelExt"))
            .and_then(|n| n.attribute("relId").map(str::to_owned))
    });
    let drawing = drawing_id
        .and_then(|id| rel_by_id(rels, &id).cloned())
        .or_else(|| rel_by_kind(rels, "diagramDrawing").cloned())?;
    Some((
        package.text(&drawing.target)?,
        package.rels(&drawing.target),
    ))
}

fn table_markdown(tbl: Node) -> String {
    let rows: Vec<Vec<String>> = tbl
        .kids("tr")
        .map(|tr| {
            tr.kids("tc")
                .map(|tc| {
                    let text: Vec<String> = tc
                        .descendants()
                        .filter(|n| n.has_tag_name_local("p"))
                        .map(|p| {
                            p.descendants()
                                .filter(|n| n.has_tag_name_local("t"))
                                .filter_map(|t| t.text())
                                .collect::<String>()
                        })
                        .filter(|t| !t.trim().is_empty())
                        .collect();
                    md_escape(&text.join(" ")).replace('\n', " ")
                })
                .collect()
        })
        .collect();
    let Some((head, body)) = rows.split_first() else {
        return String::new();
    };
    let mut md = format!(
        "| {} |\n|{}\n",
        head.join(" | "),
        " --- |".repeat(head.len())
    );
    for row in body {
        let _ = writeln!(md, "| {} |", row.join(" | "));
    }
    md
}

/// A piece of a slide read as text, and where it sits on the slide.
#[derive(Debug, Clone)]
struct TextBlock {
    rect: Rect,
    kind: BlockKind,
    md: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum BlockKind {
    /// The slide's title placeholder.
    Title,
    /// A text box, with its largest font size in points (0 when it inherits it).
    Text {
        size: f32,
    },
    Picture,
    /// A table, chart or SmartArt.
    Other,
}

/// Smallest a picture is, as a share of the slide's width and height, to
/// be worth showing as text: smaller ones are icons and bullets.
const ICON_SHARE: f32 = 0.12;
/// Thinner than this share of the slide, a picture is a rule or an accent bar.
const RULE_SHARE: f32 = 0.03;
/// Share of the slide a picture covers to count as its background.
const BACKGROUND_SHARE: f32 = 0.45;

/// A slide's blocks as markdown: decoration left out, the title first, the
/// rest in reading order.
fn slide_markdown(blocks: Vec<TextBlock>, (w, h): (f32, f32)) -> String {
    let mut seen = std::collections::HashSet::new();
    let mut blocks: Vec<TextBlock> = blocks
        .into_iter()
        .filter(|b| !b.md.trim().is_empty())
        .filter(|b| {
            let r = b.rect;
            // Off the slide: notes to self, parked shapes.
            let sized = r.w > 0.0 || r.h > 0.0;
            !(sized && (r.x >= w || r.y >= h || r.x + r.w <= 0.0 || r.y + r.h <= 0.0))
        })
        .filter(|b| {
            if b.kind != BlockKind::Picture {
                return true;
            }
            let r = b.rect;
            let icon = r.w < w * ICON_SHARE && r.h < h * ICON_SHARE;
            let rule = r.w < w * RULE_SHARE || r.h < h * RULE_SHARE;
            let background = r.w * r.h > w * h * BACKGROUND_SHARE;
            !icon && !rule && !background && seen.insert(b.md.clone())
        })
        // A color written down while designing, such as `#D4C8FF`.
        .filter(|b| {
            let text = b.md.trim().trim_start_matches('\\');
            !(text.len() == 7
                && text.starts_with('#')
                && text[1..].chars().all(|c| c.is_ascii_hexdigit()))
        })
        .collect();
    // Without a title placeholder, the one short line set larger than any
    // other text is the title.
    if !blocks.iter().any(|b| b.kind == BlockKind::Title) {
        let size = |b: &TextBlock| match b.kind {
            BlockKind::Text { size } => size,
            _ => 0.0,
        };
        let largest = blocks.iter().map(size).fold(0.0f32, f32::max);
        let at_largest: Vec<usize> = (0..blocks.len())
            .filter(|&i| size(&blocks[i]) == largest)
            .collect();
        if largest >= 18.0
            && let [i] = at_largest[..]
            && !blocks[i].md.contains('\n')
            && blocks[i].md.chars().count() <= 120
        {
            let text = blocks[i].md.trim().trim_matches('*').trim().to_owned();
            blocks[i].md = format!("## {text}");
            blocks[i].kind = BlockKind::Title;
        }
    }
    let mut ordered = reading_order(blocks);
    // The title leads, whatever sits above it (a kicker such as "01 — PROBLEM").
    if let Some(i) = ordered.iter().position(|b| b.kind == BlockKind::Title) {
        let title = ordered.remove(i);
        ordered.insert(0, title);
    }
    ordered
        .into_iter()
        .map(|b| b.md)
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Blocks in the order a reader takes them. The slide is cut in two at its
/// widest clear gap, across or down, and each side is read the same way:
/// the top before the bottom, the left before the right. So cards side by
/// side are read one card at a time, not row by row across all of them.
fn reading_order(blocks: Vec<TextBlock>) -> Vec<TextBlock> {
    if blocks.len() <= 1 {
        return blocks;
    }
    let span_y: fn(&TextBlock) -> (f32, f32) = |b| (b.rect.y, b.rect.y + b.rect.h);
    let span_x: fn(&TextBlock) -> (f32, f32) = |b| (b.rect.x, b.rect.x + b.rect.w);
    let cut = [span_y, span_x]
        .into_iter()
        .filter_map(|span| widest_gap(&blocks, span).map(|(gap, at)| (gap, at, span)))
        .max_by(|a, b| a.0.total_cmp(&b.0));
    let Some((_, at, span)) = cut else {
        let mut blocks = blocks;
        blocks.sort_by(|a, b| {
            a.rect
                .y
                .total_cmp(&b.rect.y)
                .then(a.rect.x.total_cmp(&b.rect.x))
        });
        return blocks;
    };
    let (before, after): (Vec<_>, Vec<_>) = blocks.into_iter().partition(|b| span(b).0 < at);
    let mut ordered = reading_order(before);
    ordered.extend(reading_order(after));
    ordered
}

/// The widest gap along one axis that no block spans, and where it ends.
/// Shapes that overlap by a hair still count as apart.
fn widest_gap(blocks: &[TextBlock], span: fn(&TextBlock) -> (f32, f32)) -> Option<(f32, f32)> {
    const TOUCH: f32 = 2.0;
    let mut spans: Vec<(f32, f32)> = blocks.iter().map(span).collect();
    spans.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut end = spans.first()?.1;
    let mut widest: Option<(f32, f32)> = None;
    for &(start, stop) in &spans[1..] {
        let gap = start - end;
        if gap > -TOUCH && widest.is_none_or(|(w, _)| gap > w) {
            widest = Some((gap, start));
        }
        end = end.max(stop);
    }
    widest
}

impl Source for Deck {
    fn page(&self, index: usize) -> Rc<str> {
        if self.slides.is_empty() {
            return "*This deck has no slides.*".into();
        }
        let main = match self.look.get() {
            Look::Original => format!("![]({})", Doc::image_href(&format!("slide-{index}"))),
            Look::Terminal => {
                let text = self.text.get(index).cloned().unwrap_or_default();
                if text.trim().is_empty() {
                    "*This slide has no text.*".to_owned()
                } else {
                    text
                }
            }
        };
        match self.notes.get(index).filter(|n| !n.trim().is_empty()) {
            Some(notes) => format!("{main}\n\n---\n\n*Speaker notes*\n\n{notes}").into(),
            None => main.into(),
        }
    }

    fn image_size(&self, id: &str) -> Option<(u32, u32)> {
        if id.starts_with("slide-") {
            let (w, h) = self.size;
            return Some((
                MEASURE_WIDTH,
                (MEASURE_WIDTH as f32 * h / w.max(1.0)).round().max(1.0) as u32,
            ));
        }
        let part = id.strip_prefix("pic:")?;
        if let Some(size) = self.pictures.borrow().get(part) {
            return Some(*size);
        }
        let size = crate::office::picture_size(&self.picture(part)?)?;
        self.pictures.borrow_mut().insert(part.to_owned(), size);
        Some(size)
    }

    fn decode(&self, id: &str, width: u32) -> Option<RgbaImage> {
        if let Some(index) = id.strip_prefix("slide-").and_then(|i| i.parse().ok()) {
            let svg = self.render(index)?;
            return crate::typeset::rasterize(&svg, width.clamp(1, MAX_WIDTH));
        }
        crate::office::picture_pixels(&self.picture(id.strip_prefix("pic:")?)?, width)
    }

    fn is_page(&self, id: &str) -> bool {
        id.starts_with("slide-")
    }

    fn look(&self) -> Option<Look> {
        Some(self.look.get())
    }

    fn set_look(&self, look: Look) {
        self.look.set(look);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A small deck: a master with a title style, a layout, and slides.
    fn deck(dir: &Path, slides: &[&str], notes: &[&str]) -> std::path::PathBuf {
        let path = dir.join("deck.pptx");
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        let o = zip::write::SimpleFileOptions::default();
        let mut put = |name: &str, body: String| {
            zip.start_file(name, o).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        };
        let ids: String = (0..slides.len())
            .map(|i| format!(r#"<p:sldId id="{}" r:id="rId{}"/>"#, 256 + i, i + 1))
            .collect();
        put(
            "ppt/presentation.xml",
            format!(
                r#"<p:presentation {NS}><p:sldIdLst>{ids}</p:sldIdLst><p:sldSz cx="9144000" cy="5143500"/></p:presentation>"#
            ),
        );
        let rels: String = (0..slides.len())
            .map(|i| {
                format!(
                    r#"<Relationship Id="rId{}" Type="x/slide" Target="slides/slide{}.xml"/>"#,
                    i + 1,
                    i + 1
                )
            })
            .collect();
        put(
            "ppt/_rels/presentation.xml.rels",
            format!("<Relationships>{rels}</Relationships>"),
        );
        put(
            "ppt/slideMasters/slideMaster1.xml",
            format!(
                r#"<p:sldMaster {NS}><p:cSld><p:bg><p:bgPr><a:solidFill><a:srgbClr val="102030"/></a:solidFill></p:bgPr></p:bg><p:spTree>
                <p:sp><p:nvSpPr><p:cNvPr id="2" name="T"/><p:cNvSpPr/><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:spPr><a:xfrm><a:off x="457200" y="228600"/><a:ext cx="8229600" cy="857250"/></a:xfrm></p:spPr></p:sp>
                </p:spTree></p:cSld><p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2"/>
                <p:txStyles><p:titleStyle><a:lvl1pPr><a:defRPr sz="4400"><a:solidFill><a:srgbClr val="FFFFFF"/></a:solidFill><a:latin typeface="+mj-lt"/></a:defRPr></a:lvl1pPr></p:titleStyle><p:bodyStyle/><p:otherStyle/></p:txStyles></p:sldMaster>"#
            ),
        );
        put(
            "ppt/slideMasters/_rels/slideMaster1.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Type="x/theme" Target="../theme/theme1.xml"/></Relationships>"#.into(),
        );
        put(
            "ppt/theme/theme1.xml",
            format!(
                r#"<a:theme {NS}><a:themeElements><a:clrScheme name="c"><a:dk1><a:srgbClr val="000000"/></a:dk1><a:lt1><a:srgbClr val="FFFFFF"/></a:lt1><a:accent1><a:srgbClr val="4472C4"/></a:accent1></a:clrScheme><a:fontScheme name="f"><a:majorFont><a:latin typeface="Calibri Light"/></a:majorFont><a:minorFont><a:latin typeface="Calibri"/></a:minorFont></a:fontScheme></a:themeElements></a:theme>"#
            ),
        );
        put(
            "ppt/slideLayouts/slideLayout1.xml",
            format!(r#"<p:sldLayout {NS}><p:cSld><p:spTree/></p:cSld></p:sldLayout>"#),
        );
        put(
            "ppt/slideLayouts/_rels/slideLayout1.xml.rels",
            r#"<Relationships><Relationship Id="rId1" Type="x/slideMaster" Target="../slideMasters/slideMaster1.xml"/></Relationships>"#.into(),
        );
        for (i, body) in slides.iter().enumerate() {
            let n = i + 1;
            put(
                &format!("ppt/slides/slide{n}.xml"),
                format!(r#"<p:sld {NS}><p:cSld><p:spTree>{body}</p:spTree></p:cSld></p:sld>"#),
            );
            let note = notes.get(i).copied().unwrap_or("");
            let notes_rel = if note.is_empty() {
                String::new()
            } else {
                put(
                    &format!("ppt/notesSlides/notesSlide{n}.xml"),
                    format!(
                        r#"<p:notes {NS}><p:cSld><p:spTree><p:sp><p:nvSpPr><p:cNvPr id="1" name="n"/><p:cNvSpPr/><p:nvPr><p:ph type="sldNum"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>{n}</a:t></a:r></a:p></p:txBody></p:sp><p:sp><p:nvSpPr><p:cNvPr id="2" name="b"/><p:cNvSpPr/><p:nvPr><p:ph type="body" idx="1"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>{note}</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:notes>"#
                    ),
                );
                format!(
                    r#"<Relationship Id="rId2" Type="x/notesSlide" Target="../notesSlides/notesSlide{n}.xml"/>"#
                )
            };
            put(
                &format!("ppt/slides/_rels/slide{n}.xml.rels"),
                format!(
                    r#"<Relationships><Relationship Id="rId1" Type="x/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>{notes_rel}</Relationships>"#
                ),
            );
        }
        zip.finish().unwrap();
        path
    }

    const TITLE: &str = r#"<p:sp><p:nvSpPr><p:cNvPr id="2" name="Title"/><p:cNvSpPr/><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:spPr/><p:txBody><a:bodyPr/><a:p><a:r><a:t>Quarterly &amp; review</a:t></a:r></a:p></p:txBody></p:sp>"#;
    const BOX: &str = r#"<p:sp><p:nvSpPr><p:cNvPr id="3" name="Box"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr><a:xfrm><a:off x="914400" y="2286000"/><a:ext cx="3657600" cy="914400"/></a:xfrm><a:prstGeom prst="roundRect"/><a:solidFill><a:srgbClr val="FF8800"/></a:solidFill></p:spPr><p:txBody><a:bodyPr/><a:p><a:pPr><a:buChar char="•"/></a:pPr><a:r><a:t>Point one</a:t></a:r></a:p></p:txBody></p:sp>"#;

    fn temp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("leaf-slides-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_slide_is_drawn_with_its_masters_background_and_title_style() {
        let dir = temp("draw");
        let path = deck(&dir, &[&format!("{TITLE}{BOX}")], &[]);
        let doc = open(&path).unwrap();
        assert_eq!(doc.page_count, Some(1));
        assert_eq!(&*doc.source.page(0), "![](leaf-image:slide-0)");
        assert_eq!(doc.source.image_size("slide-0"), Some((1600, 900)));
        let image = doc.source.decode("slide-0", 400).unwrap();
        assert_eq!(image.dimensions(), (400, 225));
        // The master's dark background, and the box's orange.
        assert_eq!(image.get_pixel(5, 5).0, [0x10, 0x20, 0x30, 255]);
        assert!(image.pixels().any(|p| p.0 == [0xFF, 0x88, 0x00, 255]));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_terminal_look_reads_titles_lists_and_notes() {
        let dir = temp("text");
        let path = deck(&dir, &[&format!("{BOX}{TITLE}")], &["Say *hi* first"]);
        let doc = open(&path).unwrap();
        assert_eq!(doc.source.look(), Some(Look::Original));
        doc.source.set_look(Look::Terminal);
        assert_eq!(
            &*doc.source.page(0),
            "## Quarterly \\& review\n\n- Point one\n\n---\n\n*Speaker notes*\n\nSay \\*hi\\* first"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn hidden_slides_are_skipped() {
        let dir = temp("hidden");
        let path = deck(&dir, &[TITLE, BOX], &[]);
        // Hide the second slide.
        let bytes = std::fs::read(&path).unwrap();
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let out = dir.join("hidden.pptx");
        let mut writer = zip::ZipWriter::new(std::fs::File::create(&out).unwrap());
        for i in 0..zip.len() {
            let mut file = zip.by_index(i).unwrap();
            let name = file.name().unwrap().into_owned();
            let mut text = String::new();
            std::io::Read::read_to_string(&mut file, &mut text).unwrap();
            if name == "ppt/slides/slide2.xml" {
                text = text.replacen("<p:sld ", r#"<p:sld show="0" "#, 1);
            }
            writer
                .start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(text.as_bytes()).unwrap();
        }
        writer.finish().unwrap();
        assert_eq!(open(&out).unwrap().page_count, Some(1));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn placeholders_find_what_they_inherit() {
        let xml = format!(
            r#"<p:spTree {NS}><p:sp><p:nvSpPr><p:cNvPr id="1" name="a"/><p:cNvSpPr/><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr></p:sp><p:sp><p:nvSpPr><p:cNvPr id="2" name="b"/><p:cNvSpPr/><p:nvPr><p:ph idx="1"/></p:nvPr></p:nvSpPr></p:sp></p:spTree>"#
        );
        let doc = Document::parse(&xml).unwrap();
        let tree = Some(doc.root_element());
        let found = |k, i, by| {
            find_placeholder(tree, k, i, by).and_then(|n| {
                n.attr("name").or_else(|| {
                    n.descendants()
                        .find(|d| d.has_tag_name_local("cNvPr"))
                        .and_then(|d| d.attribute("name"))
                })
            })
        };
        assert_eq!(found(Some("ctrTitle"), None, true), Some("a"));
        assert_eq!(found(Some("body"), Some("1"), true), Some("b"));
        assert_eq!(found(None, None, false), Some("b"));
    }

    #[test]
    fn tables_read_as_markdown() {
        let xml = format!(
            r#"<a:tbl {NS}><a:tr><a:tc><a:txBody><a:p><a:r><a:t>Name</a:t></a:r></a:p></a:txBody></a:tc><a:tc><a:txBody><a:p><a:r><a:t>Score</a:t></a:r></a:p></a:txBody></a:tc></a:tr><a:tr><a:tc><a:txBody><a:p><a:r><a:t>Ada</a:t></a:r></a:p></a:txBody></a:tc><a:tc><a:txBody><a:p><a:r><a:t>42</a:t></a:r></a:p></a:txBody></a:tc></a:tr></a:tbl>"#
        );
        let doc = Document::parse(&xml).unwrap();
        assert_eq!(
            table_markdown(doc.root_element()),
            "| Name | Score |\n| --- | --- |\n| Ada | 42 |\n"
        );
    }

    fn block(x: f32, y: f32, w: f32, h: f32, kind: BlockKind, md: &str) -> TextBlock {
        TextBlock {
            rect: Rect { x, y, w, h },
            kind,
            md: md.into(),
        }
    }

    #[test]
    fn cards_side_by_side_read_one_card_at_a_time() {
        let text = BlockKind::Text { size: 14.0 };
        let blocks = vec![
            block(40.0, 120.0, 200.0, 20.0, text, "Left head"),
            block(400.0, 120.0, 200.0, 20.0, text, "Right head"),
            block(40.0, 150.0, 200.0, 60.0, text, "Left body"),
            block(400.0, 150.0, 200.0, 60.0, text, "Right body"),
            block(
                40.0,
                40.0,
                560.0,
                40.0,
                BlockKind::Text { size: 32.0 },
                "Big title",
            ),
            block(
                40.0,
                10.0,
                100.0,
                12.0,
                BlockKind::Text { size: 9.0 },
                "01 — KICKER",
            ),
        ];
        assert_eq!(
            slide_markdown(blocks, (720.0, 405.0)),
            "## Big title\n\n01 — KICKER\n\nLeft head\n\nLeft body\n\nRight head\n\nRight body"
        );
    }

    #[test]
    fn decoration_is_left_out_of_the_text() {
        let blocks = vec![
            block(0.0, 0.0, 720.0, 405.0, BlockKind::Picture, "![](bg)"),
            block(10.0, 10.0, 30.0, 30.0, BlockKind::Picture, "![](icon)"),
            block(10.0, 60.0, 4.0, 200.0, BlockKind::Picture, "![](bar)"),
            block(100.0, 60.0, 300.0, 200.0, BlockKind::Picture, "![](chart)"),
            block(100.0, 300.0, 300.0, 200.0, BlockKind::Picture, "![](chart)"),
            block(
                800.0,
                60.0,
                100.0,
                20.0,
                BlockKind::Text { size: 12.0 },
                "parked",
            ),
            block(
                100.0,
                380.0,
                60.0,
                20.0,
                BlockKind::Text { size: 12.0 },
                "\\#D4C8FF",
            ),
        ];
        assert_eq!(slide_markdown(blocks, (720.0, 405.0)), "![](chart)");
    }
}
